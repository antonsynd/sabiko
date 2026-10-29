#!/usr/bin/env python3
"""Runtime and memory benchmarks: Sabiko-compiled programs against hand-written C#.

Every directory under benchmarks/ holding a ``bench.sabi`` and a ``bench.cs`` is a
benchmark: the same algorithm, printing the same output. The runner

1. builds ``sabikoc`` (cargo), the in-process host (``host/BenchHost.cs``, csc) and, for
   each build configuration, every benchmark three ways: ``sabikoc --emit=exe`` (then
   ILVerify), ``csc`` against the reference assemblies (no .NET SDK, decision 29) and,
   when ``rustc`` is on PATH, ``rustc -C opt-level=3`` with the differential harness's
   prelude (every ``.sabi`` is valid Rust, decision 49);
2. runs each build once and fails unless every language prints the same stdout and exits
   with 0 (the correctness guard);
3. measures two modes:
   - ``process``: W discarded warmup launches, then N timed launches of each program
     (``dotnet bench.dll`` or the native executable), interleaved round-robin across the
     languages so drift on a noisy machine hits all of them alike; each launch records
     wall time and peak resident set size (``os.wait4`` rusage ``ru_maxrss``);
   - ``inprocess`` (.NET only): ``host/BenchHost.dll`` loads the assembly, runs its entry
     point W times discarded and N times measured, recording per-iteration Stopwatch time
     and allocated managed bytes plus GC counts; this isolates JIT and start-up;
4. reports n, mean, stdev, min, p50, p90, p99 and max per metric and the Sabiko/C# ratio
   of mean, p50, p90 and p99, as a console table, ``--json`` and ``--markdown``.

Percentiles interpolate linearly between the closest ranks (Hyndman and Fan type 7,
NumPy's default and Excel's PERCENTILE.INC): for sorted samples x[0..n-1] the q-th
percentile is x[h] interpolated at h = (n - 1) * q / 100. stdev is the sample standard
deviation (n - 1 denominator).

Only the Python standard library is used (Python 3.10 or newer). Unix only: peak RSS comes
from ``os.wait4``. See benchmarks/README.md.
"""

from __future__ import annotations

import argparse
import datetime
import json
import math
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

BENCH_DIR = Path(__file__).resolve().parent
REPO_ROOT = BENCH_DIR.parent
TOOLS = REPO_ROOT / "tools"

SCHEMA_VERSION = 1

# Sample counts. p99 of N samples interpolates between the two largest order statistics
# when N = 100, so N must be at least ~100 for p99 to say anything beyond "max".
DEFAULT_PROCESS_WARMUP = 5
DEFAULT_PROCESS_RUNS = 100
DEFAULT_INPROC_WARMUP = 20
DEFAULT_INPROC_ITERATIONS = 100
SMOKE_COUNTS = (1, 3, 1, 3)  # process warmup/runs, in-process warmup/iterations

LAUNCH_TIMEOUT_SECONDS = 120

LANGUAGES = ("sabiko", "csharp", "rust")
LANGUAGE_NAMES = {"sabiko": "Sabiko", "csharp": "C#", "rust": "Rust"}
MODES = ("process", "inprocess")
PERCENTILES = (50, 90, 99)
PERCENTILE_METHOD = "linear interpolation between closest ranks (Hyndman-Fan type 7)"

# Prepended to every bench.sabi before rustc sees it; the same as RUSTC_PRELUDE in
# crates/sabikoc/tests/differential_run.rs.
RUSTC_PRELUDE = """#![allow(nonstandard_style, unused, dead_code)]
fn Print<T: std::fmt::Display>(x: T) { print!("{x}") }
fn PrintLine<T: std::fmt::Display>(x: T) { println!("{x}") }
fn Panic(msg: &str) -> ! { panic!("{msg}") }
fn main() { Main() }
"""


@dataclass(frozen=True)
class Config:
    """A build configuration: the matching flags for each compiler."""

    name: str
    description: str
    sabikoc: tuple[str, ...]
    csc: tuple[str, ...]
    rustc: tuple[str, ...]


CONFIGS = {
    "release": Config(
        "release",
        "Sabiko --release (wrapping arithmetic) vs C# -optimize+ (unchecked, C#'s default) "
        "vs rustc -C opt-level=3 -C overflow-checks=off",
        ("--release",),
        ("-optimize+",),
        ("-C", "opt-level=3", "-C", "overflow-checks=off"),
    ),
    "checked": Config(
        "checked",
        "Sabiko debug (overflow-checked arithmetic) vs C# -optimize+ -checked+ "
        "vs rustc -C opt-level=3 -C overflow-checks=on",
        (),
        ("-optimize+", "-checked+"),
        ("-C", "opt-level=3", "-C", "overflow-checks=on"),
    ),
}


class BenchError(Exception):
    """A failure that ends the run: a build error, a crash or an output mismatch."""


# ----- statistics -----------------------------------------------------------------------


def percentile(sorted_values: list[float], q: float) -> float:
    """The q-th percentile (0..100) of already sorted values, linearly interpolated.

    Hyndman and Fan type 7: h = (n - 1) * q / 100, interpolating between x[floor(h)] and
    x[floor(h) + 1].
    """
    if not sorted_values:
        raise ValueError("percentile of no samples")
    if not 0 <= q <= 100:
        raise ValueError(f"percentile {q} outside 0..100")
    h = (len(sorted_values) - 1) * q / 100
    lo = math.floor(h)
    hi = min(lo + 1, len(sorted_values) - 1)
    return sorted_values[lo] + (h - lo) * (sorted_values[hi] - sorted_values[lo])


def compute_stats(samples: list[float]) -> dict:
    """Distribution statistics of a sample list; the samples are kept for re-analysis."""
    if not samples:
        raise ValueError("no samples")
    s = sorted(samples)
    n = len(s)
    mean = math.fsum(s) / n
    stdev = math.sqrt(math.fsum((x - mean) ** 2 for x in s) / (n - 1)) if n > 1 else 0.0
    stats = {"n": n, "mean": mean, "stdev": stdev, "min": s[0], "max": s[-1]}
    for q in PERCENTILES:
        stats[f"p{q}"] = percentile(s, q)
    stats["samples"] = list(samples)
    return stats


def ratio(numerator: float | None, denominator: float | None) -> float | None:
    """numerator / denominator, or None when either is missing or the denominator is 0."""
    if numerator is None or denominator is None or denominator == 0:
        return None
    return numerator / denominator


# ----- toolchain ------------------------------------------------------------------------


@dataclass
class Toolchain:
    dotnet: Path
    csc: Path
    ref_dir: Path
    runtime_dir: Path
    ilverify: Path
    rustc: Path | None
    sabikoc: Path | None = None


def find_toolchain(use_rust: bool) -> Toolchain:
    """Locates the tools scripts/setup-dotnet.sh installs under tools/."""
    dotnet = os.environ.get("SABIKO_DOTNET")
    if not dotnet and (TOOLS / "dotnet" / "dotnet").is_file():
        dotnet = str(TOOLS / "dotnet" / "dotnet")
    dotnet = dotnet or shutil.which("dotnet")
    csc = TOOLS / "bin" / "csc"
    ref_dir = Path(os.environ.get("SABIKO_REF_DIR", TOOLS / "ref"))
    runtime_dir = Path(os.environ.get("SABIKO_RUNTIME_DIR", TOOLS / "runtime"))
    ilverify = TOOLS / "ilverify" / "ILVerify.dll"
    missing = [
        str(p)
        for p in (csc, ref_dir, runtime_dir / "Sabiko.Runtime.dll", ilverify)
        if not p.exists()
    ]
    if not dotnet:
        missing.append("dotnet")
    if missing:
        raise BenchError(
            "toolchain incomplete (run scripts/setup-dotnet.sh): missing " + ", ".join(missing)
        )
    rustc = shutil.which(os.environ.get("RUSTC", "rustc")) if use_rust else None
    return Toolchain(Path(dotnet), csc, ref_dir, runtime_dir, ilverify, Path(rustc) if rustc else None)


def run_checked(cmd: list[str], what: str, cwd: Path | None = None) -> str:
    """Runs a build command; raises BenchError with its output when it fails."""
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if proc.returncode != 0:
        raise BenchError(f"{what} failed ({' '.join(cmd)}):\n{proc.stdout}{proc.stderr}")
    return proc.stdout


def build_sabikoc() -> Path:
    """Builds sabikoc with cargo and returns the executable cargo reports."""
    out = run_checked(
        ["cargo", "build", "-q", "-p", "sabikoc", "--message-format=json-render-diagnostics"],
        "cargo build -p sabikoc",
        cwd=REPO_ROOT,
    )
    exe = parse_cargo_executable(out, "sabikoc")
    if exe is None:
        raise BenchError("cargo build did not report a sabikoc executable")
    return exe


def parse_cargo_executable(cargo_json: str, target: str) -> Path | None:
    """The executable of `target` among cargo's --message-format=json lines."""
    found = None
    for line in cargo_json.splitlines():
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (
            msg.get("reason") == "compiler-artifact"
            and msg.get("target", {}).get("name") == target
            and msg.get("executable")
        ):
            found = Path(msg["executable"])
    return found


def csc_command(tc: Toolchain, rsp: Path, source: Path, output: Path, flags: tuple[str, ...]) -> list[str]:
    """csc invocation in the style of scripts/build-runtime.sh: no SDK, reference assemblies."""
    return [
        str(tc.csc),
        "-nologo",
        "-noconfig",
        "-nostdlib",
        "-target:exe",
        "-deterministic",
        "-langversion:latest",
        "-nullable:enable",
        *flags,
        f"-out:{output}",
        f"@{rsp}",
        str(source),
    ]


def write_reference_rsp(tc: Toolchain, path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [f'-r:"{dll}"' for dll in sorted(tc.ref_dir.glob("*.dll"))]
    path.write_text("\n".join(lines) + "\n")
    return path


@dataclass
class Build:
    """The programs built for one benchmark in one configuration."""

    name: str
    config: str
    commands: dict[str, list[str]] = field(default_factory=dict)  # language -> argv
    dlls: dict[str, Path] = field(default_factory=dict)  # .NET language -> assembly


def build_host(tc: Toolchain, build_dir: Path, rsp: Path, runtimeconfig: str) -> Path:
    out = build_dir / "host" / "BenchHost.dll"
    out.parent.mkdir(parents=True, exist_ok=True)
    run_checked(
        csc_command(tc, rsp, BENCH_DIR / "host" / "BenchHost.cs", out, ("-optimize+", "-warnaserror")),
        "csc BenchHost.cs",
    )
    out.with_suffix(".runtimeconfig.json").write_text(runtimeconfig)
    return out


def build_benchmark(tc: Toolchain, bench: Path, config: Config, build_dir: Path, rsp: Path) -> Build:
    root = build_dir / config.name / bench.name
    build = Build(bench.name, config.name)

    sabiko_dll = root / "sabiko" / "bench.dll"
    sabiko_dll.parent.mkdir(parents=True, exist_ok=True)
    run_checked(
        [str(tc.sabikoc), str(bench / "bench.sabi"), "--emit=exe", *config.sabikoc,
         "--runtime-path", str(tc.runtime_dir), "-o", str(sabiko_dll)],
        f"sabikoc {bench.name} ({config.name})",
    )
    run_checked(
        [str(tc.dotnet), str(tc.ilverify), "-r", str(tc.ref_dir / "*.dll"),
         "-r", str(tc.runtime_dir / "Sabiko.Runtime.dll"), "-g", "ReturnPtrToStack", str(sabiko_dll)],
        f"ILVerify {bench.name} ({config.name})",
    )
    build.dlls["sabiko"] = sabiko_dll
    build.commands["sabiko"] = [str(tc.dotnet), str(sabiko_dll)]

    csharp_dll = root / "csharp" / "bench.dll"
    csharp_dll.parent.mkdir(parents=True, exist_ok=True)
    run_checked(csc_command(tc, rsp, bench / "bench.cs", csharp_dll, config.csc), f"csc {bench.name} ({config.name})")
    # The same runtimeconfig.json sabikoc wrote, so both run on the same framework.
    shutil.copyfile(sabiko_dll.with_suffix(".runtimeconfig.json"), csharp_dll.with_suffix(".runtimeconfig.json"))
    build.dlls["csharp"] = csharp_dll
    build.commands["csharp"] = [str(tc.dotnet), str(csharp_dll)]

    if tc.rustc:
        rust_dir = root / "rust"
        rust_dir.mkdir(parents=True, exist_ok=True)
        source = rust_dir / "bench.rs"
        source.write_text(RUSTC_PRELUDE + (bench / "bench.sabi").read_text())
        exe = rust_dir / "bench"
        run_checked(
            [str(tc.rustc), "--edition", "2021", "--crate-type", "bin", *config.rustc,
             "-A", "warnings", "-o", str(exe), str(source)],
            f"rustc {bench.name} ({config.name})",
        )
        build.commands["rust"] = [str(exe)]
    return build


def find_benchmarks(names: list[str]) -> list[Path]:
    found = sorted(d for d in BENCH_DIR.iterdir() if (d / "bench.sabi").is_file())
    for d in found:
        if not (d / "bench.cs").is_file():
            raise BenchError(f"{d.name}: bench.sabi has no bench.cs counterpart")
    if names:
        unknown = sorted(set(names) - {d.name for d in found})
        if unknown:
            raise BenchError(f"unknown benchmark(s): {', '.join(unknown)}")
        found = [d for d in found if d.name in names]
    return found


# ----- measurement ----------------------------------------------------------------------


@dataclass
class Launch:
    wall_seconds: float
    max_rss_kib: float
    exit_code: int
    stdout: str
    stderr: str


def maxrss_to_kib(ru_maxrss: int, system: str = sys.platform) -> float:
    """ru_maxrss is KiB on Linux and bytes on macOS."""
    return ru_maxrss / 1024 if system == "darwin" else float(ru_maxrss)


def launch(cmd: list[str], timeout: float = LAUNCH_TIMEOUT_SECONDS) -> Launch:
    """Runs one process to completion, timing it and reading its peak RSS from wait4."""
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        start = time.perf_counter()
        proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=out, stderr=err)
        killer = threading.Timer(timeout, proc.kill)
        killer.start()
        try:
            _, status, usage = os.wait4(proc.pid, 0)
        finally:
            killer.cancel()
        wall = time.perf_counter() - start
        proc.returncode = os.waitstatus_to_exitcode(status)
        out.seek(0)
        err.seek(0)
        return Launch(
            wall,
            maxrss_to_kib(usage.ru_maxrss),
            proc.returncode,
            out.read().decode("utf-8", "replace"),
            err.read().decode("utf-8", "replace"),
        )


def check_launch(result: Launch, expected: str | None, what: str) -> None:
    if result.exit_code != 0:
        raise BenchError(f"{what} exited with {result.exit_code}:\n{result.stderr}")
    if expected is not None and result.stdout != expected:
        raise BenchError(f"{what} printed different output:\n--- expected\n{expected}--- actual\n{result.stdout}")


def verify_outputs(build: Build) -> str:
    """The correctness guard: every language prints the same stdout and exits with 0."""
    outputs = {}
    for lang, cmd in build.commands.items():
        result = launch(cmd)
        check_launch(result, None, f"{build.name} ({build.config}, {lang})")
        outputs[lang] = result.stdout
    expected = outputs["sabiko"]
    mismatched = [lang for lang, out in outputs.items() if out != expected]
    if mismatched:
        detail = "".join(f"--- {lang}\n{out}" for lang, out in outputs.items())
        raise BenchError(f"{build.name} ({build.config}): outputs differ\n{detail}")
    return expected


def measure_process(build: Build, expected: str, warmup: int, runs: int, log) -> dict:
    """W + N launches per language, round-robin across languages."""
    langs = list(build.commands)
    wall = {lang: [] for lang in langs}
    rss = {lang: [] for lang in langs}
    for i in range(warmup + runs):
        for lang in langs:
            result = launch(build.commands[lang])
            check_launch(result, expected, f"{build.name} ({build.config}, {lang}) launch {i}")
            if i >= warmup:
                wall[lang].append(result.wall_seconds * 1000)
                rss[lang].append(result.max_rss_kib / 1024)
        if (i + 1) % 25 == 0:
            log(f"    {i + 1}/{warmup + runs} rounds")
    return {lang: {"wall_ms": compute_stats(wall[lang]), "peak_rss_mib": compute_stats(rss[lang])} for lang in langs}


def parse_host_result(data: dict) -> dict:
    """Turns BenchHost's JSON into the runner's in-process metrics."""
    return {
        "time_ms": compute_stats([ns / 1e6 for ns in data["elapsed_ns"]]),
        "allocated_bytes": compute_stats([float(b) for b in data["allocated_bytes"]]),
        "gc_collections": data["gc_collections"],
        "peak_working_set_mib": data["peak_working_set_bytes"] / (1024 * 1024),
        "runtime": data.get("runtime"),
        "output": data.get("output"),
    }


def measure_inprocess(tc: Toolchain, host: Path, build: Build, expected: str, warmup: int, iterations: int) -> dict:
    results = {}
    for lang, dll in build.dlls.items():
        with tempfile.TemporaryDirectory() as tmp:
            result_path = Path(tmp) / "result.json"
            proc = subprocess.run(
                [str(tc.dotnet), str(host), str(dll), str(max(warmup, 1)), str(iterations), str(result_path)],
                capture_output=True,
                text=True,
            )
            if proc.returncode != 0:
                raise BenchError(f"BenchHost on {build.name} ({build.config}, {lang}) failed:\n{proc.stdout}{proc.stderr}")
            data = json.loads(result_path.read_text())
        metrics = parse_host_result(data)
        if metrics["output"] != expected:
            raise BenchError(
                f"{build.name} ({build.config}, {lang}) printed different output in-process:\n"
                f"--- expected\n{expected}--- actual\n{metrics['output']}"
            )
        results[lang] = metrics
    return results


# ----- reporting ------------------------------------------------------------------------

# (mode, metric key, label, unit) in report order.
METRICS = (
    ("process", "wall_ms", "Process wall time", "ms"),
    ("process", "peak_rss_mib", "Process peak RSS", "MiB"),
    ("inprocess", "time_ms", "In-process time per iteration", "ms"),
    ("inprocess", "allocated_bytes", "In-process allocated bytes per iteration", "B"),
)


def metric_stats(results: dict, name: str, config: str, mode: str, metric: str, lang: str) -> dict | None:
    try:
        return results["benchmarks"][name][config][mode][lang][metric]
    except KeyError:
        return None


def stat(stats: dict | None, key: str) -> float | None:
    return None if stats is None else stats[key]


def fmt_value(value: float | None, unit: str) -> str:
    if value is None:
        return "—"
    if unit == "B":
        return f"{value:,.0f}"
    if unit == "MiB":
        return f"{value:.1f}"
    if abs(value) >= 100:
        return f"{value:.0f}"
    if abs(value) >= 10:
        return f"{value:.1f}"
    return f"{value:.2f}"


def fmt_ratio(value: float | None) -> str:
    return "—" if value is None else f"{value:.2f}x"


def summary_rows(results: dict) -> list[list[str]]:
    """One row per benchmark and configuration: p50 of each metric and Sabiko/C#."""
    rows = []
    for name, configs in results["benchmarks"].items():
        for config in configs:
            row = [name, config]
            for mode, metric, _, unit in METRICS:
                sab = stat(metric_stats(results, name, config, mode, metric, "sabiko"), "p50")
                cs = stat(metric_stats(results, name, config, mode, metric, "csharp"), "p50")
                row += [fmt_value(sab, unit), fmt_value(cs, unit), fmt_ratio(ratio(sab, cs))]
                if mode == "process" and metric == "wall_ms":
                    rust = stat(metric_stats(results, name, config, mode, metric, "rust"), "p50")
                    row.append(fmt_value(rust, unit))
            rows.append(row)
    return rows


SUMMARY_HEADER = [
    "Benchmark", "Config",
    "Wall Sabiko (ms)", "Wall C# (ms)", "Wall S/C#", "Wall Rust (ms)",
    "RSS Sabiko (MiB)", "RSS C# (MiB)", "RSS S/C#",
    "Iter Sabiko (ms)", "Iter C# (ms)", "Iter S/C#",
    "Alloc Sabiko (B)", "Alloc C# (B)", "Alloc S/C#",
]


def markdown_table(header: list[str], rows: list[list[str]], align_right_from: int = 1) -> list[str]:
    lines = ["| " + " | ".join(header) + " |"]
    lines.append("| " + " | ".join("---" if i < align_right_from else "---:" for i in range(len(header))) + " |")
    lines += ["| " + " | ".join(row) + " |" for row in rows]
    return lines


def text_table(header: list[str], rows: list[list[str]]) -> str:
    widths = [max(len(h), *(len(r[i]) for r in rows)) if rows else len(h) for i, h in enumerate(header)]
    out = ["  ".join(h.ljust(w) for h, w in zip(header, widths))]
    out.append("  ".join("-" * w for w in widths))
    out += ["  ".join(c.ljust(w) for c, w in zip(row, widths)) for row in rows]
    return "\n".join(out)


def distribution_rows(results: dict, mode: str, metric: str, unit: str) -> list[list[str]]:
    rows = []
    for name, configs in results["benchmarks"].items():
        for config in configs:
            for lang in LANGUAGES:
                s = metric_stats(results, name, config, mode, metric, lang)
                if s is None:
                    continue
                rows.append([
                    name, config, LANGUAGE_NAMES[lang], str(s["n"]),
                    *(fmt_value(s[k], unit) for k in ("mean", "stdev", "min", "p50", "p90", "p99", "max")),
                ])
    return rows


def ratio_rows(results: dict) -> list[list[str]]:
    """Sabiko/C# of mean, p50, p90 and p99, per benchmark, configuration and metric."""
    rows = []
    for name, configs in results["benchmarks"].items():
        for config in configs:
            for mode, metric, label, _ in METRICS:
                sab = metric_stats(results, name, config, mode, metric, "sabiko")
                cs = metric_stats(results, name, config, mode, metric, "csharp")
                if sab is None or cs is None:
                    continue
                rows.append([
                    name, config, label,
                    *(fmt_ratio(ratio(sab[k], cs[k])) for k in ("mean", "p50", "p90", "p99")),
                ])
    return rows


def render_markdown(results: dict) -> str:
    machine = results["machine"]
    settings = results["settings"]
    lines = [
        f"# Sabiko vs C# benchmarks ({results['generated'][:10]})",
        "",
        f"**Machine:** {machine['label']}  ",
        f"**CPU:** {machine['cpu']} ({machine['cpu_count']} logical) | **OS:** {machine['os']} | "
        f"**.NET:** {machine.get('dotnet_runtime') or '?'} | **rustc:** {machine.get('rustc') or 'not run'}  ",
        f"**Samples:** process mode {settings['process_warmup']} warmup + {settings['process_runs']} timed "
        f"launches per program (round-robin across languages); in-process {settings['inproc_warmup']} warmup + "
        f"{settings['inproc_iterations']} measured iterations  ",
        f"**Percentiles:** {settings['percentile_method']}; stdev is the sample standard deviation",
    ]
    for note in machine.get("notes", []):
        lines += ["", f"> {note}"]
    lines += ["", "## Configurations", ""]
    for name in settings["configs"]:
        lines.append(f"- `{name}`: {CONFIGS[name].description}")
    lines += [
        "",
        "## Summary (p50)",
        "",
        "Wall = process wall time per launch (start-up included); RSS = peak resident set size; "
        "Iter = in-process time per iteration (steady state); Alloc = managed bytes allocated "
        "per in-process iteration. S/C# below 1.00x means Sabiko is faster or smaller.",
        "",
    ]
    lines += markdown_table(SUMMARY_HEADER, summary_rows(results), align_right_from=2)
    lines += ["", "## Sabiko/C# ratios", ""]
    lines += markdown_table(
        ["Benchmark", "Config", "Metric", "mean", "p50", "p90", "p99"], ratio_rows(results), align_right_from=3
    )
    for mode, metric, label, unit in METRICS:
        rows = distribution_rows(results, mode, metric, unit)
        if not rows:
            continue
        lines += ["", f"## {label} ({unit})", ""]
        lines += markdown_table(
            ["Benchmark", "Config", "Language", "N", "mean", "stdev", "min", "p50", "p90", "p99", "max"],
            rows,
            align_right_from=3,
        )
    gc_rows = []
    for name, configs in results["benchmarks"].items():
        for config, data in configs.items():
            for lang, m in data.get("inprocess", {}).items():
                gc = m["gc_collections"]
                gc_rows.append([
                    name, config, LANGUAGE_NAMES[lang], str(gc["gen0"]), str(gc["gen1"]), str(gc["gen2"]),
                    f"{m['peak_working_set_mib']:.1f}",
                ])
    if gc_rows:
        lines += ["", "## In-process GC collections (over the measured iterations) and peak working set", ""]
        lines += markdown_table(
            ["Benchmark", "Config", "Language", "gen0", "gen1", "gen2", "peak WS (MiB)"], gc_rows, align_right_from=3
        )
    lines += ["", f"*Generated by `benchmarks/run_benchmarks.py` on {results['generated']}.*", ""]
    return "\n".join(lines)


def machine_info(tc: Toolchain | None, label: str | None, notes: list[str]) -> dict:
    cpu = platform.processor() or platform.machine()
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    info = {
        "label": label or f"{platform.system()} {platform.machine()} (unlabelled)",
        "os": f"{platform.system()} {platform.release()}",
        "cpu": cpu,
        "cpu_count": os.cpu_count(),
        "python": platform.python_version(),
        "notes": notes,
        # DOTNET_* variables that change JIT or GC behaviour apply to both .NET languages
        # alike, but make numbers incomparable across runs; record them.
        "dotnet_env": {
            k: v
            for k, v in sorted(os.environ.items())
            if k.startswith("DOTNET_") and k not in ("DOTNET_ROOT", "DOTNET_CLI_TELEMETRY_OPTOUT", "DOTNET_NOLOGO")
        },
    }
    if tc:
        try:
            runtimes = subprocess.run([str(tc.dotnet), "--list-runtimes"], capture_output=True, text=True).stdout
            info["dotnet_runtime"] = next(
                (line.split()[1] for line in runtimes.splitlines() if line.startswith("Microsoft.NETCore.App")), None
            )
        except OSError:
            info["dotnet_runtime"] = None
        if tc.rustc:
            info["rustc"] = subprocess.run([str(tc.rustc), "--version"], capture_output=True, text=True).stdout.strip()
    return info


# ----- main -----------------------------------------------------------------------------


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("benchmarks", nargs="*", help="benchmark names (default: all)")
    p.add_argument("--configs", default="release,checked", help="comma-separated: release, checked")
    p.add_argument("--modes", default="process,inprocess", help="comma-separated: process, inprocess")
    p.add_argument("--process-warmup", type=int, default=DEFAULT_PROCESS_WARMUP)
    p.add_argument("--process-runs", type=int, default=DEFAULT_PROCESS_RUNS)
    p.add_argument("--inproc-warmup", type=int, default=DEFAULT_INPROC_WARMUP)
    p.add_argument("--inproc-iterations", type=int, default=DEFAULT_INPROC_ITERATIONS)
    p.add_argument("--smoke", action="store_true", help="tiny sample counts: checks the harness, not performance")
    p.add_argument("--no-rust", action="store_true", help="skip the rustc column")
    p.add_argument("--sabikoc", type=Path, help="use this sabikoc instead of building it with cargo")
    p.add_argument("--build-dir", type=Path, default=REPO_ROOT / "target" / "benchmarks")
    p.add_argument("--json", type=Path, help="write the full results (samples included) here")
    p.add_argument("--markdown", type=Path, help="write the Markdown report here")
    p.add_argument("--machine-label", help="describes the machine in the report")
    p.add_argument("--note", action="append", default=[], help="a note for the report (repeatable)")
    args = p.parse_args(argv)
    if args.smoke:
        args.process_warmup, args.process_runs, args.inproc_warmup, args.inproc_iterations = SMOKE_COUNTS
    args.configs = [c for c in args.configs.split(",") if c]
    args.modes = [m for m in args.modes.split(",") if m]
    for c in args.configs:
        if c not in CONFIGS:
            p.error(f"unknown configuration {c!r} (expected one of {', '.join(CONFIGS)})")
    for m in args.modes:
        if m not in MODES:
            p.error(f"unknown mode {m!r} (expected one of {', '.join(MODES)})")
    for name in ("process_runs", "inproc_iterations"):
        if getattr(args, name) < 1:
            p.error(f"--{name.replace('_', '-')} must be at least 1")
    return args


def run(args: argparse.Namespace) -> dict:
    def log(msg: str) -> None:
        print(msg, file=sys.stderr, flush=True)

    started = time.monotonic()
    benches = find_benchmarks(args.benchmarks)
    tc = find_toolchain(not args.no_rust)
    if not args.no_rust and tc.rustc is None:
        log("note: rustc not found; the Rust column is skipped")
    log("building sabikoc" if args.sabikoc is None else f"using {args.sabikoc}")
    tc.sabikoc = args.sabikoc or build_sabikoc()
    build_dir = args.build_dir.resolve()
    rsp = write_reference_rsp(tc, build_dir / "references.rsp")

    builds: list[tuple[Build, str]] = []
    for config_name in args.configs:
        for bench in benches:
            log(f"building {bench.name} ({config_name})")
            build = build_benchmark(tc, bench, CONFIGS[config_name], build_dir, rsp)
            builds.append((build, verify_outputs(build)))
    host = None
    if "inprocess" in args.modes:
        runtimeconfig = builds[0][0].dlls["sabiko"].with_suffix(".runtimeconfig.json").read_text()
        host = build_host(tc, build_dir, rsp, runtimeconfig)

    results = {
        "schema": SCHEMA_VERSION,
        "generated": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "machine": machine_info(tc, args.machine_label, args.note),
        "settings": {
            "configs": args.configs,
            "modes": args.modes,
            "process_warmup": args.process_warmup,
            "process_runs": args.process_runs,
            "inproc_warmup": args.inproc_warmup,
            "inproc_iterations": args.inproc_iterations,
            "percentile_method": PERCENTILE_METHOD,
        },
        "benchmarks": {},
    }
    for build, expected in builds:
        entry = results["benchmarks"].setdefault(build.name, {}).setdefault(build.config, {"output": expected})
        if "process" in args.modes:
            log(f"process mode: {build.name} ({build.config}), {', '.join(build.commands)}")
            entry["process"] = measure_process(build, expected, args.process_warmup, args.process_runs, log)
        if host is not None:
            log(f"in-process mode: {build.name} ({build.config})")
            entry["inprocess"] = measure_inprocess(
                tc, host, build, expected, args.inproc_warmup, args.inproc_iterations
            )
    results["elapsed_seconds"] = round(time.monotonic() - started, 1)
    return results


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        results = run(args)
    except BenchError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    print(text_table(SUMMARY_HEADER, summary_rows(results)))
    print(f"\n(p50 values; S/C# = Sabiko/C#; {results['elapsed_seconds']}s in total)")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(results, indent=1) + "\n")
    if args.markdown:
        args.markdown.parent.mkdir(parents=True, exist_ok=True)
        args.markdown.write_text(render_markdown(results))
    return 0


if __name__ == "__main__":
    sys.exit(main())

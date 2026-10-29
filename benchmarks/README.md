# Benchmarks: Sabiko vs C#

Runtime and memory of Sabiko-compiled programs against the same programs written by hand
in C#, on the same .NET runtime, measured nightly. rustc builds of the Sabiko sources
are a third, reference column: every `.sabi` file is valid Rust (decision 49). Methodology:
decision 50 in `docs/DECISIONS.md`. The latest checked-in report is
[`results/latest.md`](results/latest.md); the nightly reports are in the job summaries and
artifacts of the `Nightly` workflow.

## Benchmarks

Each directory holds `bench.sabi` and `bench.cs`: the same algorithm, the same shape, the
same output. The workloads are sized to take roughly 50–250 ms per process launch and
10–110 ms per steady-state iteration.

| Name | What it exercises |
| --- | --- |
| `fibonacci` | Recursive `fib(32)` (call overhead) and an iterative loop with `i64` remainders |
| `collatz` | Longest Collatz chain below 300,000: `u64` arithmetic, unpredictable branches |
| `mandelbrot` | Escape counts over a 640×480 grid: `f64` arithmetic, int-to-float casts, early exit |
| `nbody` | Five planets as fields of a struct passed by `&mut`, disjoint field borrows, `f64` |
| `primes` | Prime counting below 600,000 by trial division: remainder by a variable divisor |
| `lcg_hash` | A MINSTD generator feeding a polynomial hash: `u64` multiply and remainder by constants |
| `tuples` | Pairs passed and returned by value, a quotient-and-remainder pair, destructuring and a three-element accumulator rebuilt per iteration; the C# uses `System.ValueTuple`, which Sabiko's tuples lower to (decision 52) |
| `methods` | Structs with methods in a hot loop: a generator stepped through `&mut self`, a Copy vector type with by-value, `&self` and `&mut self` methods and a constructor, particles and a world calling methods on their fields (one call a two-phase borrow); Sabiko's methods are static methods of the struct (decision 53), the C# uses instance methods on structs |

They stay inside today's subset (integers, floats, `bool`, structs, tuples, references,
functions, methods, `if`/`while`/`loop`): no arrays, enums or generics. Two consequences:
the n-body bodies are spelled out as struct fields with the ten pairs written out, and its
square root is a Newton iteration (there is no `f64::sqrt`: methods on primitives come
with traits). The C#
programs use the same Newton function and the same struct-of-fields shape, so the
comparison measures code generation, not libraries; otherwise the C# is what a developer
would write (`for` loops, `const`, `ref`/`in` parameters, `Console.WriteLine`).
Arithmetic never overflows in any benchmark, so the checked configuration computes the
same results; there are no `wrapping_*` methods yet.

To add when the rest of Phase 5 (arrays, slices) lands: a sieve of Eratosthenes, spectral
norm, sorting (quicksort and insertion sort over an array), matrix multiply, and n-body
over an array of bodies with `f64::sqrt`; with Phase 7's `Box`, `String` and `Vec`,
binary trees (allocation) and string building. Each of those is where Sabiko's bounds
checks, value-type layout and allocation strategy will differ from C#'s, which the current
set cannot show.

## Running

Needs the toolchain from `scripts/setup-dotnet.sh` (dotnet, csc, the reference assemblies,
ILVerify, `Sabiko.Runtime.dll`), cargo, Python 3.10 or newer (standard library only), and
optionally `rustc` on `PATH`. Linux or macOS: peak RSS comes from `os.wait4`.

```sh
python3 benchmarks/run_benchmarks.py                           # everything, nightly sample counts
python3 benchmarks/run_benchmarks.py nbody primes --configs release
python3 benchmarks/run_benchmarks.py --smoke fibonacci         # 1 warmup + 3 samples: checks the harness
python3 benchmarks/run_benchmarks.py --json out/r.json --markdown out/r.md --machine-label "my laptop"
python3 -m unittest discover -s benchmarks -p 'test_*.py'      # the harness's unit tests
```

To see what the JIT made of a program, `scripts/jit-summary.sh [--release] [--disasm
METHOD] file.sabi` compiles it and prints one line per compiled method with its tier
(`Tier0`, `Instrumented Tier0`, `Tier1`, `Tier1-OSR`), IL size and code size
(`DOTNET_JitDisasmSummary`), plus the disassembly of `METHOD` at each tier with
`--disasm`. It is a diagnostic, not a test: tiering and machine code change between
runtime versions. A method with a backward branch starts in instrumented Tier0 (the JIT
treats it as a loop), and IL size drives the inliner, so the summary is the first thing to
check when a benchmark's start-up or steady state moves.

Options: `--modes process,inprocess`, `--process-warmup`, `--process-runs`,
`--inproc-warmup`, `--inproc-iterations`, `--no-rust`, `--sabikoc PATH` (skip the cargo
build), `--build-dir` (default `target/benchmarks`), `--note TEXT` (repeatable, printed in
the report). The console shows a p50 summary; `--json` writes every statistic and every
raw sample, `--markdown` the full report.

## What is measured

Builds, per configuration and benchmark: `sabikoc --emit=exe` (and ILVerify on the result,
as the run-pass tests do); `csc` from `tools/` against the reference assemblies with no .NET
SDK (decision 29), given the very `runtimeconfig.json` sabikoc wrote, so both run on the same
framework with the same settings; `rustc --edition 2021 -C opt-level=3` on the `.sabi` file
with the differential harness's prelude (`Print`, `PrintLine`, `Panic`, `fn main()`).

Correctness guard: before anything is timed, every build runs once and all languages must
exit with 0 and print byte-identical stdout; every timed launch is checked again, and the
in-process host's captured output must match too. A mismatch fails the run.

Two modes:

1. **Process** (end to end, what a user sees). W discarded warmup launches, then N timed
   launches of `dotnet bench.dll` (or the native executable). Each launch records wall time
   (`time.perf_counter` around spawn-to-reap) and peak resident set size (`os.wait4`
   rusage `ru_maxrss`, KiB on Linux and bytes on macOS, reported in MiB). Launches are
   interleaved round-robin across the languages, so slow phases of a noisy machine hit all
   of them alike. This includes runtime start-up and the JIT's tiering: much of a 100 ms
   program runs in Tier0 or OSR code.
2. **In-process** (steady state, .NET only). `host/BenchHost.cs`, compiled with csc, loads
   the benchmark assembly with `Assembly.LoadFrom` (which finds `Sabiko.Runtime.dll` next
   to it), binds its entry point to a delegate and calls it W times discarded and N times
   measured. Per iteration it records the `Stopwatch` time of the call and the managed bytes
   allocated during it (`GC.GetTotalAllocatedBytes(precise: true)`, read outside the timed
   region); over the measured iterations, the gen0/1/2 collection counts; at the end, the
   peak working set. Standard output goes through a forwarding writer installed before the
   assembly loads (Sabiko.Runtime caches `Console.Out` on first use): the first iteration's
   output is captured for the guard, later iterations write to `TextWriter.Null`. Sabiko's
   generated entry point wraps `Main` in a `try` and returns 101 after reporting a panic;
   it is safe to call repeatedly and the host fails the run on any non-zero return.

Metrics: process wall time (ms), process peak RSS (MiB), in-process time per iteration
(ms), in-process allocated bytes per iteration. For each: n, mean, stdev, min, p50, p90,
p99, max, and the Sabiko/C# ratio of mean, p50, p90 and p99 (below 1.00x: Sabiko is faster
or smaller).

## Configurations and fairness

| Config | Sabiko | C# | rustc |
| --- | --- | --- | --- |
| `release` (headline) | `--release`: wrapping arithmetic | `-optimize+`, unchecked (C#'s default) | `-C opt-level=3 -C overflow-checks=off` |
| `checked` | default build: overflow-checked arithmetic | `-optimize+ -checked+` | `-C opt-level=3 -C overflow-checks=on` |

- Same runtime, same `runtimeconfig.json`, same environment: tiered compilation, OSR and
  dynamic PGO at their defaults for both. The runner records any `DOTNET_*` variable that
  would change JIT or GC behaviour in the report's JSON.
- Sabiko's `checked` build differs from `release` only in overflow checks, as `-checked+`
  does for C#. Division and remainder by a variable divisor are checked for zero in both
  Sabiko configurations, as in Rust; C# relies on the hardware exception.
- The Sabiko side loads one more assembly (`Sabiko.Runtime.dll`) and formats numbers through
  it, which shows in peak RSS (about 2 MiB) and, for floats, in allocations.
- rustc targets generic x86-64 (no `-C target-cpu=native`) while the JIT uses the host's
  instruction set; the Rust column is a reference point, not part of the comparison.

## Statistics and sample counts

Warmups are discarded. Percentiles interpolate linearly between the closest ranks (Hyndman
and Fan type 7, NumPy's default, Excel's `PERCENTILE.INC`): for sorted samples
`x[0..n-1]`, the q-th percentile is `x[h]` interpolated at `h = (n - 1) * q / 100`. stdev
is the sample standard deviation (n − 1). The JSON keeps every sample in measurement order,
so other statistics can be computed later.

| Mode | Warmup | Measured | Why |
| --- | --- | --- | --- |
| process | 5 launches | 100 launches | Warm the OS file cache and the runtime's on-disk state; 100 is the least for a p99 |
| in-process | 20 iterations | 100 iterations | `fibonacci` in C# needs about 12 iterations to reach Tier1, the slowest seen; after 20, the first 10 measured iterations are within 8% of the last 50 for every series. 100 for the p99 |

p99 needs on the order of 100 samples to mean anything: with N = 100, h = 98.01, so p99 is
essentially the second-largest sample; with fewer it is just the maximum. Read it as "the
slowest run or two", and prefer p50 and p90 for comparisons; more samples would make p99
a real tail estimate but would not fit the nightly budget.

Budget: the full run (8 benchmarks × 2 configurations × 3 languages × 105 launches, plus
120 in-process iterations per benchmark, configuration and .NET language) took 810 s,
builds included, on an otherwise idle 4-core container when `methods` was added (6
benchmarks took 552 s on a loaded one, `results/latest.md`). Add 3–4 minutes of setup on a
runner (Rust and .NET toolchains, building `sabikoc`): about 17 minutes, over the
15-minute target but within the job's 30-minute timeout. Lower `--process-runs` (decision
50 keeps 100 for the p99) or split the run before adding benchmarks that would approach
the timeout.

## Allocation gate

`allocation-baseline.json` holds the managed bytes per in-process iteration (the median of
the measured iterations) of every Sabiko benchmark in each configuration. The nightly job
runs `allocation_gate.py check`, which fails if any entry grows by more than 10%; a zero
baseline allows no allocation. Allocation counts are exact and machine-independent, so this
can gate a build; time is not gated, because it is too noisy on a shared runner. The
baseline changes only deliberately, and the diff is reviewed like a golden file:

```sh
python3 benchmarks/run_benchmarks.py --json target/benchmarks/results.json
python3 benchmarks/allocation_gate.py check  --results target/benchmarks/results.json
python3 benchmarks/allocation_gate.py update --results target/benchmarks/results.json   # deliberate
```

Today only `nbody` allocates on the Sabiko side: printing an `f64` goes through
`Sabiko.Runtime`'s Rust-compatible float formatting, which builds several strings.

## Nightly job and CI

The `benchmarks` job in `.github/workflows/nightly.yml` (also `workflow_dispatch`) installs
the pinned Rust toolchain and the .NET toolchain, runs everything, appends the Markdown
report to the job summary, uploads `results.json`, `report.md` and the gate's output as the
`benchmark-results` artifact (kept 90 days), and then runs the allocation gate. Results are
never committed by CI. `results/latest.md` is a local run checked in by hand as an example
and labelled with the machine it came from; regenerate it with `--markdown` when the
benchmark set changes.

`ci.yml` runs the harness's unit tests and `--smoke fibonacci` (both configurations, tiny
sample counts) plus the allocation gate on Linux on every push, so the harness cannot rot
between nightlies.

## Adding a benchmark

1. Create `benchmarks/<name>/` with `bench.sabi` (an `fn Main()`) and `bench.cs` (a
   `static void Main()`), same algorithm and output. Keep arithmetic overflow-free so the
   checked configuration computes the same thing, and the `.sabi` valid Rust: the runner
   builds it with rustc as well.
2. Size it to roughly 10–250 ms per run and check the job still fits its
   budget (the run prints its total time).
3. Run `python3 benchmarks/run_benchmarks.py <name>`: the guard fails on any output
   difference.
4. Run the full set with `--json`, then `allocation_gate.py update`, and commit the new
   baseline entries with the benchmark.

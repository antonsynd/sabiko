#!/usr/bin/env python3
"""Benchmark history and the performance-over-time chart.

Monday's scheduled run of the nightly ``benchmarks`` job (or a manual run with the
``record-trend`` input) runs ``run_benchmarks.py --json`` on a GitHub runner, then:

    # append the run's p50 values to benchmarks/results/history.json (one line per run)
    python3 benchmarks/trend.py record --results bench-out/results.json --commit "$GITHUB_SHA"

    # redraw benchmarks/results/trend.svg from the history
    python3 benchmarks/trend.py chart

and commits both files. The chart plots Sabiko/C# ratios of the p50, so a value below
1.00x means Sabiko is faster: ratios of programs measured on the same runner in the same
run (interleaved, see README.md) are far steadier across runs than absolute times on a
shared runner. The top panel is the geometric mean over the benchmarks of each run; below
it, one small multiple per benchmark on a shared scale.

Only the standard library is used.
"""

from __future__ import annotations

import argparse
import datetime
import json
import math
import sys
from html import escape
from pathlib import Path

RESULTS_DIR = Path(__file__).resolve().parent / "results"
DEFAULT_HISTORY = RESULTS_DIR / "history.json"
DEFAULT_CHART = RESULTS_DIR / "trend.svg"

# (mode, metric) pairs recorded per benchmark, configuration and language.
RECORDED = (
    ("process", "wall_ms"),
    ("process", "peak_rss_mib"),
    ("inprocess", "time_ms"),
    ("inprocess", "allocated_bytes"),
)
LANGUAGES = ("sabiko", "csharp", "rust")

# The charted series: (key, label, config, metric key in a history entry). The order is
# the legend order and the colour order; colours follow the series, never their rank.
SERIES = (
    ("steady_release", "Steady state, release", "release", "inprocess_time_ms"),
    ("process_release", "End to end, release", "release", "process_wall_ms"),
    ("steady_checked", "Steady state, checked", "checked", "inprocess_time_ms"),
    ("process_checked", "End to end, checked", "checked", "process_wall_ms"),
)


# ----- history ---------------------------------------------------------------------------


def significant(value: float | None, digits: int = 4) -> float | None:
    """`value` rounded to `digits` significant digits (keeps history.json small)."""
    if value is None or value == 0:
        return value
    return round(value, digits - 1 - math.floor(math.log10(abs(value))))


def entry_from_results(results: dict, commit: str | None, date: str | None = None) -> dict:
    """The history entry for one run_benchmarks.py JSON document: p50s only."""
    benchmarks: dict = {}
    for name, configs in sorted(results.get("benchmarks", {}).items()):
        for config, data in sorted(configs.items()):
            metrics: dict = {}
            for mode, metric in RECORDED:
                values = {}
                for lang in LANGUAGES:
                    stats = data.get(mode, {}).get(lang, {}).get(metric)
                    if stats is not None:
                        values[lang] = significant(stats["p50"])
                if values:
                    metrics[f"{mode}_{metric}"] = values
            if metrics:
                benchmarks.setdefault(name, {})[config] = metrics
    machine = results.get("machine", {})
    return {
        "date": date or results.get("generated", "")[:10],
        "commit": (commit or "")[:12],
        "machine": machine.get("label"),
        "dotnet": machine.get("dotnet_runtime"),
        "benchmarks": benchmarks,
    }


def load_history(path: Path) -> list[dict]:
    if not path.exists():
        return []
    text = path.read_text().strip()
    return json.loads(text) if text else []


def dump_history(history: list[dict]) -> str:
    """A JSON array with one run per line, so each week's diff is one added line."""
    if not history:
        return "[]\n"
    lines = [json.dumps(entry, separators=(",", ":"), sort_keys=False) for entry in history]
    return "[\n" + ",\n".join(lines) + "\n]\n"


def add_entry(history: list[dict], entry: dict) -> list[dict]:
    """`history` with `entry` added in date order; a run on the same date replaces it."""
    kept = [h for h in history if h["date"] != entry["date"]]
    return sorted(kept + [entry], key=lambda h: h["date"])


# ----- series ----------------------------------------------------------------------------


def ratio_of(entry: dict, bench: str, config: str, metric: str) -> float | None:
    values = entry["benchmarks"].get(bench, {}).get(config, {}).get(metric, {})
    sab, cs = values.get("sabiko"), values.get("csharp")
    if sab is None or not cs:
        return None
    return sab / cs


def geomean(values: list[float]) -> float | None:
    positive = [v for v in values if v and v > 0]
    if not positive:
        return None
    return math.exp(sum(math.log(v) for v in positive) / len(positive))


def benchmark_names(history: list[dict]) -> list[str]:
    return sorted({name for h in history for name in h["benchmarks"]})


def series_for(history: list[dict], bench: str | None) -> dict[str, list[float | None]]:
    """{series key: one Sabiko/C# ratio per history entry}; `bench=None` is the geomean."""
    out: dict[str, list[float | None]] = {}
    for key, _, config, metric in SERIES:
        values: list[float | None] = []
        for h in history:
            if bench is None:
                ratios = [ratio_of(h, b, config, metric) for b in h["benchmarks"]]
                values.append(geomean([r for r in ratios if r is not None]))
            else:
                values.append(ratio_of(h, bench, config, metric))
        out[key] = values
    return out


# ----- SVG -------------------------------------------------------------------------------

# Colours: categorical slots 1-4 (blue, orange, aqua, yellow) of the validated reference
# palette, a light and a dark step each; the chart follows the viewer's colour scheme.
STYLE = """
.surface { fill: #fcfcfb; }
.title { fill: #0b0b0b; font-size: 15px; font-weight: 600; }
.subtitle { fill: #52514e; font-size: 11px; }
.panel-title { fill: #0b0b0b; font-size: 12px; font-weight: 600; }
.tick { fill: #898781; font-size: 10px; }
.legend { fill: #52514e; font-size: 11px; }
.grid { stroke: #e1e0d9; stroke-width: 1; }
.axis { stroke: #c3c2b7; stroke-width: 1; }
.parity { stroke: #898781; stroke-width: 1; stroke-dasharray: 4 3; }
.line { fill: none; stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; }
.dot { stroke: #fcfcfb; stroke-width: 1.5; }
.s0 { stroke: #2a78d6; } .d0 { fill: #2a78d6; }
.s1 { stroke: #eb6834; } .d1 { fill: #eb6834; }
.s2 { stroke: #1baf7a; } .d2 { fill: #1baf7a; }
.s3 { stroke: #eda100; } .d3 { fill: #eda100; }
@media (prefers-color-scheme: dark) {
  .surface { fill: #1a1a19; }
  .title, .panel-title { fill: #f0efec; }
  .subtitle, .legend { fill: #c3c2b7; }
  .grid { stroke: #2c2c2a; }
  .axis { stroke: #383835; }
  .dot { stroke: #1a1a19; }
  .s0 { stroke: #3987e5; } .d0 { fill: #3987e5; }
  .s1 { stroke: #d95926; } .d1 { fill: #d95926; }
  .s2 { stroke: #199e70; } .d2 { fill: #199e70; }
  .s3 { stroke: #c98500; } .d3 { fill: #c98500; }
}
"""

WIDTH = 860
MARGIN_X = 20
FONT = "system-ui, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"


def y_range(values: list[float]) -> tuple[float, float]:
    """A range on 0.25x steps that always shows parity and some room on each side."""
    lo = min([0.75, *values])
    hi = max([1.25, *values])
    return math.floor(lo * 4) / 4, math.ceil(hi * 4) / 4


def y_ticks(lo: float, hi: float, max_ticks: int = 6) -> list[float]:
    step = 0.25
    while (hi - lo) / step > max_ticks:
        step *= 2
    first = math.ceil(lo / step) * step
    ticks = []
    t = first
    while t <= hi + 1e-9:
        ticks.append(round(t, 4))
        t += step
    if not any(abs(t - 1.0) < 1e-9 for t in ticks):
        ticks.append(1.0)
    return sorted(ticks)


def day_number(date: str) -> int:
    return datetime.date.fromisoformat(date).toordinal()


def date_ticks(dates: list[str], max_labels: int) -> list[int]:
    """Indices of the dates to label: evenly thinned, always the first and the last."""
    n = len(dates)
    if n <= max_labels:
        return list(range(n))
    step = math.ceil((n - 1) / (max_labels - 1))
    picked = list(range(0, n, step))
    if picked[-1] != n - 1:
        if n - 1 - picked[-1] < step / 2:
            picked[-1] = n - 1
        else:
            picked.append(n - 1)
    return picked


def short_date(date: str) -> str:
    d = datetime.date.fromisoformat(date)
    return f"{d.strftime('%b')} {d.day}"


def render_panel(
    x: float,
    y: float,
    w: float,
    h: float,
    title: str,
    history: list[dict],
    series: dict[str, list[float | None]],
    lo: float,
    hi: float,
    max_date_labels: int,
    every_dot: bool = True,
) -> list[str]:
    """One panel: title, gridlines, parity line, a line and dots per series.

    Without `every_dot` (the small multiples) only each line's last point and isolated
    points get a dot: at that size the dots' surface rings would chop the line up.
    """
    out = [f'<text class="panel-title" x="{x:.1f}" y="{y + 12:.1f}">{escape(title)}</text>']
    left, top = x + 40, y + 24
    plot_w, plot_h = w - 48, h - 24 - 22
    bottom = top + plot_h
    dates = [e["date"] for e in history]
    first, last = day_number(dates[0]), day_number(dates[-1])

    def xp(i: int) -> float:
        if last == first:
            return left + plot_w / 2
        return left + (day_number(dates[i]) - first) / (last - first) * plot_w

    def yp(v: float) -> float:
        return bottom - (v - lo) / (hi - lo) * plot_h

    for t in y_ticks(lo, hi):
        cls = "parity" if abs(t - 1.0) < 1e-9 else "grid"
        out.append(f'<line class="{cls}" x1="{left:.1f}" y1="{yp(t):.1f}" x2="{left + plot_w:.1f}" y2="{yp(t):.1f}"/>')
        out.append(f'<text class="tick" x="{left - 6:.1f}" y="{yp(t) + 3.5:.1f}" text-anchor="end">{t:.2f}x</text>')
    out.append(f'<line class="axis" x1="{left:.1f}" y1="{bottom:.1f}" x2="{left + plot_w:.1f}" y2="{bottom:.1f}"/>')
    for i in date_ticks(dates, max_date_labels):
        anchor = "middle"
        if len(dates) > 1 and i == 0:
            anchor = "start"
        elif len(dates) > 1 and i == len(dates) - 1:
            anchor = "end"
        out.append(
            f'<text class="tick" x="{xp(i):.1f}" y="{bottom + 14:.1f}" text-anchor="{anchor}">{short_date(dates[i])}</text>'
        )

    for si, (key, label, _, _) in enumerate(SERIES):
        points = [(i, v) for i, v in enumerate(series[key]) if v is not None]
        # A run of consecutive points is one path; a missing value breaks the line.
        runs: list[list[tuple[int, float]]] = []
        for i, v in points:
            if runs and runs[-1][-1][0] == i - 1:
                runs[-1].append((i, v))
            else:
                runs.append([(i, v)])
        for run in runs:
            if len(run) >= 2:
                d = " ".join(f"{'M' if j == 0 else 'L'}{xp(i):.1f},{yp(v):.1f}" for j, (i, v) in enumerate(run))
                out.append(f'<path class="line s{si}" d="{d}"/>')
        dotted = points if every_dot else [run[-1] for run in runs]
        for i, v in dotted:
            e = history[i]
            tip = f"{label}: {v:.2f}x ({e['date']}, {e.get('commit') or 'unknown commit'})"
            out.append(
                f'<circle class="dot d{si}" cx="{xp(i):.1f}" cy="{yp(v):.1f}" r="3.5"><title>{escape(tip)}</title></circle>'
            )
    return out


def render_svg(history: list[dict]) -> str:
    """The whole chart; a placeholder that says so when there is no history yet."""
    head = (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {{h}}" width="{WIDTH}" height="{{h}}" '
        f'font-family="{FONT}" role="img" aria-labelledby="t">'
    )
    title = "Sabiko vs C#: time ratio over time (p50, Sabiko / C#)"
    if not history:
        h = 120
        return "\n".join(
            [
                head.format(h=h),
                f"<style>{STYLE}</style>",
                f'<rect class="surface" width="{WIDTH}" height="{h}" rx="6"/>',
                f'<text id="t" class="title" x="{MARGIN_X}" y="34">{escape(title)}</text>',
                f'<text class="subtitle" x="{MARGIN_X}" y="58">No history yet: the nightly benchmarks job adds a point '
                "every Monday (or run Nightly by hand with record-trend).</text>",
                "</svg>",
                "",
            ]
        )

    names = benchmark_names(history)
    overall = series_for(history, None)
    per_bench = {name: series_for(history, name) for name in names}
    every = [v for s in [overall, *per_bench.values()] for vs in s.values() for v in vs if v is not None]
    lo, hi = y_range(every)

    body: list[str] = []
    first, last = history[0], history[-1]
    span = first["date"] if len(history) == 1 else f"{first['date']} to {last['date']}"
    machine = last.get("machine") or "unlabelled machine"
    body.append(f'<text id="t" class="title" x="{MARGIN_X}" y="28">{escape(title)}</text>')
    body.append(
        f'<text class="subtitle" x="{MARGIN_X}" y="46">Below 1.00x = Sabiko faster. {len(history)} weekly '
        f"run{'s' if len(history) != 1 else ''}, {escape(span)}, latest {escape(last.get('commit') or '?')} "
        f"on {escape(machine)}.</text>"
    )
    body.append(
        f'<text class="subtitle" x="{MARGIN_X}" y="62">Steady state = in-process time per iteration after '
        "warm-up; end to end = process wall time per launch, start-up included.</text>"
    )

    # Legend: always present (four series), in series order.
    lx, ly = MARGIN_X, 86
    for si, (_, label, _, _) in enumerate(SERIES):
        body.append(f'<line class="line s{si}" x1="{lx:.1f}" y1="{ly - 4}" x2="{lx + 18:.1f}" y2="{ly - 4}"/>')
        body.append(f'<circle class="dot d{si}" cx="{lx + 9:.1f}" cy="{ly - 4}" r="3.5"/>')
        body.append(f'<text class="legend" x="{lx + 24:.1f}" y="{ly}">{escape(label)}</text>')
        lx += 24 + len(label) * 6.2 + 22

    inner_w = WIDTH - 2 * MARGIN_X
    y = 104
    body += render_panel(
        MARGIN_X, y, inner_w, 230,
        f"Geometric mean over each run's benchmarks ({len(last['benchmarks'])} in the latest)",
        history, overall, lo, hi, max_date_labels=10,
    )
    y += 230 + 16

    cols = 3
    gap = 18
    cell_w = (inner_w - (cols - 1) * gap) / cols
    cell_h = 170
    for k, name in enumerate(names):
        cx = MARGIN_X + (k % cols) * (cell_w + gap)
        cy = y + (k // cols) * (cell_h + 10)
        body += render_panel(
            cx, cy, cell_w, cell_h, name, history, per_bench[name], lo, hi, max_date_labels=3, every_dot=False
        )
    rows = math.ceil(len(names) / cols)
    total_h = int(y + rows * (cell_h + 10) + 10)

    return "\n".join(
        [
            head.format(h=total_h),
            f"<style>{STYLE}</style>",
            f'<rect class="surface" width="{WIDTH}" height="{total_h}" rx="6"/>',
            *body,
            "</svg>",
            "",
        ]
    )


# ----- main ------------------------------------------------------------------------------


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="command", required=True)
    rec = sub.add_parser("record", help="append a run_benchmarks.py --json result to the history")
    rec.add_argument("--results", type=Path, required=True)
    rec.add_argument("--commit", help="the commit the run measured")
    rec.add_argument("--date", help="YYYY-MM-DD (default: the run's own date)")
    rec.add_argument("--history", type=Path, default=DEFAULT_HISTORY)
    chart = sub.add_parser("chart", help="draw the SVG from the history")
    chart.add_argument("--history", type=Path, default=DEFAULT_HISTORY)
    chart.add_argument("--output", type=Path, default=DEFAULT_CHART)
    chart.add_argument("--weeks", type=int, default=0, help="only the last N weeks (default: all)")
    return p.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    history = load_history(args.history)
    if args.command == "record":
        entry = entry_from_results(json.loads(args.results.read_text()), args.commit, args.date)
        if not entry["benchmarks"]:
            print(f"error: {args.results} has no benchmark results", file=sys.stderr)
            return 1
        if not entry["date"]:
            print("error: no date in the results; pass --date", file=sys.stderr)
            return 1
        history = add_entry(history, entry)
        args.history.parent.mkdir(parents=True, exist_ok=True)
        args.history.write_text(dump_history(history))
        print(f"recorded {entry['date']} ({len(entry['benchmarks'])} benchmarks); {len(history)} runs in {args.history}")
        return 0
    if args.weeks > 0 and history:
        cutoff = datetime.date.fromisoformat(history[-1]["date"]) - datetime.timedelta(weeks=args.weeks)
        history = [h for h in history if datetime.date.fromisoformat(h["date"]) > cutoff]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(render_svg(history))
    print(f"chart written to {args.output} ({len(history)} runs)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

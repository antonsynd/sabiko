#!/usr/bin/env python3
"""Allocation-regression gate for the Sabiko benchmarks.

The in-process host measures the managed bytes each iteration of a benchmark allocates
with ``GC.GetTotalAllocatedBytes(precise: true)``. Unlike time, that count is exact and
does not depend on the machine or its load, so it can gate a nightly run where timings
cannot. This script compares the Sabiko side of a ``run_benchmarks.py --json`` result
(the median of the per-iteration counts, per benchmark and configuration) against the
checked-in ``benchmarks/allocation-baseline.json`` and fails when any regresses by more
than the threshold (10% by default; a zero baseline therefore allows no allocation).

The baseline changes only on purpose, through ``update``, and the diff is reviewed like
any other expectation file:

    # exit 1 if any benchmark allocates more than 10% over its baseline
    python3 benchmarks/allocation_gate.py check --results target/benchmarks/results.json

    # deliberately rewrite the baseline from a run (review the diff!)
    python3 benchmarks/allocation_gate.py update --results target/benchmarks/results.json

Only the standard library is used.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path

DEFAULT_BASELINE = Path(__file__).resolve().parent / "allocation-baseline.json"
DEFAULT_THRESHOLD = 0.10
LANGUAGE = "sabiko"


@dataclass(frozen=True)
class Regression:
    key: str
    baseline: int
    current: int

    @property
    def increase(self) -> float:
        """Fractional increase over the baseline; infinite when the baseline is zero."""
        return float("inf") if self.baseline == 0 else (self.current - self.baseline) / self.baseline


def allocations_from_results(results: dict) -> dict[str, int]:
    """``{"<benchmark>/<config>": bytes}`` from a run_benchmarks.py JSON document.

    The value is the median per-iteration allocation of the Sabiko program in the
    in-process mode: one-off allocations by the runtime (tiering, a lazily created
    buffer) do not move a median the way they move a mean.
    """
    found: dict[str, int] = {}
    for name, configs in results.get("benchmarks", {}).items():
        for config, data in configs.items():
            stats = data.get("inprocess", {}).get(LANGUAGE, {}).get("allocated_bytes")
            if stats is not None:
                found[f"{name}/{config}"] = round(stats["p50"])
    return found


def load_baseline(path: Path) -> dict[str, int]:
    data = json.loads(path.read_text())
    return {key: int(entry["allocated_bytes"]) for key, entry in data["benchmarks"].items()}


def find_regressions(baseline: dict[str, int], current: dict[str, int], threshold: float) -> list[Regression]:
    """Entries in both maps whose current value exceeds baseline * (1 + threshold)."""
    return [
        Regression(key, base, current[key])
        for key, base in sorted(baseline.items())
        if key in current and current[key] > base * (1 + threshold)
    ]


def coverage(baseline: dict[str, int], current: dict[str, int]) -> tuple[list[str], list[str]]:
    """(in the baseline but not measured, measured but not in the baseline)."""
    return sorted(set(baseline) - set(current)), sorted(set(current) - set(baseline))


def baseline_document(current: dict[str, int], threshold: float) -> dict:
    return {
        "_comment": (
            "Allocation-regression baseline: managed bytes allocated per in-process iteration "
            "of each Sabiko benchmark (median over the measured iterations). Updated only "
            "deliberately with `python3 benchmarks/allocation_gate.py update`; review the diff."
        ),
        "threshold": threshold,
        "benchmarks": {key: {"allocated_bytes": current[key]} for key in sorted(current)},
    }


def check(baseline_path: Path, results_path: Path, threshold: float | None) -> int:
    document = json.loads(baseline_path.read_text())
    if threshold is None:
        threshold = float(document.get("threshold", DEFAULT_THRESHOLD))
    baseline = load_baseline(baseline_path)
    current = allocations_from_results(json.loads(results_path.read_text()))
    if not current:
        print(f"error: {results_path} has no in-process Sabiko allocation data", file=sys.stderr)
        return 2
    missing, new = coverage(baseline, current)
    for key in new:
        print(f"note: {key} is not in the baseline (not gated): {current[key]:,} B per iteration")
    for key in missing:
        print(f"warning: {key} is in the baseline but was not measured")
    regressions = find_regressions(baseline, current, threshold)
    if regressions:
        print(f"ALLOCATION REGRESSION: {len(regressions)} benchmark(s) over the {threshold:.0%} threshold")
        for r in regressions:
            print(f"  {r.key}: baseline {r.baseline:,} B, now {r.current:,} B (+{r.current - r.baseline:,} B, +{r.increase:.1%})")
        print(
            "If the increase is intended, update the baseline deliberately and review the diff:\n"
            f"  python3 benchmarks/allocation_gate.py update --results {results_path}"
        )
        return 1
    checked = len(set(baseline) & set(current))
    print(f"OK: {checked} benchmark(s) within the {threshold:.0%} allocation threshold")
    return 0


def update(baseline_path: Path, results_path: Path, threshold: float | None) -> int:
    current = allocations_from_results(json.loads(results_path.read_text()))
    if not current:
        print(f"error: {results_path} has no in-process Sabiko allocation data", file=sys.stderr)
        return 2
    if threshold is None:
        threshold = DEFAULT_THRESHOLD
        if baseline_path.exists():
            threshold = float(json.loads(baseline_path.read_text()).get("threshold", DEFAULT_THRESHOLD))
    baseline_path.write_text(json.dumps(baseline_document(current, threshold), indent=2) + "\n")
    print(f"wrote {len(current)} entries to {baseline_path}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Gate Sabiko benchmark allocations against a baseline.")
    sub = parser.add_subparsers(dest="command", required=True)
    for name, text in (("check", "fail if any benchmark regressed"), ("update", "rewrite the baseline")):
        p = sub.add_parser(name, help=text)
        p.add_argument("--results", type=Path, required=True, help="run_benchmarks.py --json output")
        p.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
        p.add_argument("--threshold", type=float, help="allowed fractional increase (default: the baseline's, else 0.10)")
    args = parser.parse_args(argv)
    try:
        if args.command == "check":
            return check(args.baseline, args.results, args.threshold)
        return update(args.baseline, args.results, args.threshold)
    except (OSError, ValueError, KeyError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

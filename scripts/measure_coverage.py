#!/usr/bin/env python3
"""Measures the generator's coverage counts (`crates/sabiko_testgen/tests/validity.rs`,
the `programs_*` tests) over several windows of 500 seeds, and prints each count's
minimum, mean and maximum with the threshold the file's rule gives: 75% of the minimum,
rounded down, at least 1. A construct that some window has none of has no threshold: the
script lists it and exits with 1.

Usage: python3 scripts/measure_coverage.py [--windows N]   (default 10: seeds 1-5000)

Run it after a change to the generator moves the counts, and set the thresholds in
validity.rs (and the table in docs/TESTING.md) from its output.
"""

import argparse
import collections
import os
import re
import subprocess
import sys

LINE = re.compile(r"^coverage (\S+): (.*) = (\d+) \(min (\d+)\)$")


def measure(base):
    env = dict(os.environ, SABIKO_COVERAGE_MEASURE="1", SABIKO_COVERAGE_BASE_SEED=str(base))
    out = subprocess.run(
        ["cargo", "test", "-q", "-p", "sabiko_testgen", "--test", "validity",
         "programs_", "--", "--nocapture", "--test-threads=1"],
        env=env, capture_output=True, text=True, check=True,
    )
    counts = {}
    for line in out.stderr.splitlines():
        m = LINE.match(line.strip())
        if m:
            counts[(m.group(1), m.group(2))] = (int(m.group(3)), int(m.group(4)))
    return counts


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--windows", type=int, default=10)
    args = parser.parse_args()
    seen = collections.defaultdict(list)
    current = {}
    for w in range(args.windows):
        base = 1 + 500 * w
        print(f"window {w + 1}/{args.windows}: seeds {base}-{base + 499}", file=sys.stderr)
        for key, (count, threshold) in measure(base).items():
            seen[key].append(count)
            current[key] = threshold
    print("| test | count | min | mean | max | threshold now | 75% of min |")
    print("| --- | --- | --- | --- | --- | --- | --- |")
    absent = []
    for (test, name), counts in seen.items():
        lo = min(counts)
        mean = sum(counts) / len(counts)
        if lo == 0:
            # Some window of 500 programs has none: no threshold can hold, and one of 1
            # would fail on that window. The construct is (nearly) not generated.
            absent.append(f"{test}: {name} (counts {counts})")
            suggested = "NONE IN A WINDOW"
        else:
            suggested = max(1, lo * 3 // 4)
        print(f"| {test} | {name} | {lo} | {mean:.1f} | {max(counts)} | "
              f"{current[(test, name)]} | {suggested} |")
    if absent:
        print("\nERROR: some window of 500 seeds has none of these constructs; fix the "
              "generator or drop the count before setting thresholds:", file=sys.stderr)
        for line in absent:
            print(f"  {line}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()

"""Unit tests for the allocation-regression gate.

Run with: python3 -m unittest discover -s benchmarks -p 'test_*.py'
"""

from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import allocation_gate as gate  # noqa: E402


def results_with(allocations: dict[str, float]) -> dict:
    """A run_benchmarks.py-shaped document with the given Sabiko p50 allocations."""
    doc: dict = {"benchmarks": {}}
    for key, p50 in allocations.items():
        name, config = key.split("/")
        doc["benchmarks"].setdefault(name, {})[config] = {
            "inprocess": {
                "sabiko": {"allocated_bytes": {"p50": p50}},
                "csharp": {"allocated_bytes": {"p50": 999_999.0}},
            }
        }
    return doc


class GateTest(unittest.TestCase):
    def test_reads_the_sabiko_median(self):
        doc = results_with({"nbody/release": 1312.0, "primes/checked": 0.0})
        doc["benchmarks"]["primes"]["release"] = {"process": {}}  # no in-process data
        self.assertEqual(gate.allocations_from_results(doc), {"nbody/release": 1312, "primes/checked": 0})

    def test_threshold_is_exclusive(self):
        baseline = {"a/release": 1000, "b/release": 1000, "c/release": 1000}
        current = {"a/release": 1100, "b/release": 1101, "c/release": 500}
        regressions = gate.find_regressions(baseline, current, 0.10)
        self.assertEqual([r.key for r in regressions], ["b/release"])
        self.assertAlmostEqual(regressions[0].increase, 0.101)

    def test_zero_baseline_allows_nothing(self):
        regressions = gate.find_regressions({"a/release": 0}, {"a/release": 24}, 0.10)
        self.assertEqual(len(regressions), 1)
        self.assertEqual(regressions[0].increase, float("inf"))
        self.assertEqual(gate.find_regressions({"a/release": 0}, {"a/release": 0}, 0.10), [])

    def test_coverage(self):
        missing, new = gate.coverage({"a/r": 1, "b/r": 1}, {"b/r": 1, "c/r": 1})
        self.assertEqual((missing, new), (["a/r"], ["c/r"]))

    def run_gate(self, *argv: str) -> tuple[int, str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            code = gate.main(list(argv))
        return code, out.getvalue()

    def test_update_then_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            results = Path(tmp) / "results.json"
            results.write_text(json.dumps(results_with({"nbody/release": 1312.0, "fibonacci/release": 0.0})))
            code, _ = self.run_gate("update", "--results", str(results), "--baseline", str(baseline))
            self.assertEqual(code, 0)
            doc = json.loads(baseline.read_text())
            self.assertEqual(doc["threshold"], gate.DEFAULT_THRESHOLD)
            self.assertEqual(doc["benchmarks"]["nbody/release"], {"allocated_bytes": 1312})

            code, out = self.run_gate("check", "--results", str(results), "--baseline", str(baseline))
            self.assertEqual(code, 0, out)
            self.assertIn("OK: 2 benchmark(s)", out)

            results.write_text(json.dumps(results_with({"nbody/release": 2000.0, "lcg_hash/release": 8.0})))
            code, out = self.run_gate("check", "--results", str(results), "--baseline", str(baseline))
            self.assertEqual(code, 1)
            self.assertIn("nbody/release: baseline 1,312 B, now 2,000 B", out)
            self.assertIn("note: lcg_hash/release is not in the baseline", out)
            self.assertIn("warning: fibonacci/release is in the baseline but was not measured", out)

            # A looser threshold on the command line overrides the baseline's.
            code, _ = self.run_gate(
                "check", "--results", str(results), "--baseline", str(baseline), "--threshold", "1.0"
            )
            self.assertEqual(code, 0)

    def test_results_without_allocations_are_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp) / "results.json"
            results.write_text(json.dumps({"benchmarks": {}}))
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text(json.dumps(gate.baseline_document({"a/release": 1}, 0.1)))
            code, out = self.run_gate("check", "--results", str(results), "--baseline", str(baseline))
            self.assertEqual(code, 2, out)
            code, _ = self.run_gate("check", "--results", str(Path(tmp) / "missing.json"), "--baseline", str(baseline))
            self.assertEqual(code, 2)

    def test_checked_in_baseline_is_well_formed(self):
        doc = json.loads(gate.DEFAULT_BASELINE.read_text())
        self.assertIn("threshold", doc)
        entries = gate.load_baseline(gate.DEFAULT_BASELINE)
        self.assertTrue(entries)
        for key, value in entries.items():
            self.assertRegex(key, r"^[a-z0-9_]+/(release|checked)$")
            self.assertGreaterEqual(value, 0)


if __name__ == "__main__":
    unittest.main()

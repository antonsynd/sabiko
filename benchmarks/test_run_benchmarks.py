"""Unit tests for the benchmark runner: statistics, parsing and reporting.

Run with: python3 -m unittest discover -s benchmarks -p 'test_*.py'
"""

from __future__ import annotations

import contextlib
import io
import json
import math
import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_benchmarks as rb  # noqa: E402


class PercentileTest(unittest.TestCase):
    def test_interpolates_between_closest_ranks(self):
        # NumPy: np.percentile([1, 2, 3, 4], [50, 90]) == [2.5, 3.7]
        self.assertAlmostEqual(rb.percentile([1, 2, 3, 4], 50), 2.5)
        self.assertAlmostEqual(rb.percentile([1, 2, 3, 4], 90), 3.7)

    def test_extremes_are_min_and_max(self):
        data = [3.0, 5.0, 8.0]
        self.assertEqual(rb.percentile(data, 0), 3.0)
        self.assertEqual(rb.percentile(data, 100), 8.0)

    def test_p99_of_one_to_hundred(self):
        # h = 99 * 0.99 = 98.01: between the 99th and 100th order statistics.
        data = [float(i) for i in range(1, 101)]
        self.assertAlmostEqual(rb.percentile(data, 99), 99.01)
        self.assertAlmostEqual(rb.percentile(data, 50), 50.5)
        self.assertAlmostEqual(rb.percentile(data, 90), 90.1)

    def test_single_sample(self):
        for q in (0, 50, 99, 100):
            self.assertEqual(rb.percentile([7.0], q), 7.0)

    def test_rejects_empty_and_out_of_range(self):
        with self.assertRaises(ValueError):
            rb.percentile([], 50)
        with self.assertRaises(ValueError):
            rb.percentile([1.0], 101)


class StatsTest(unittest.TestCase):
    def test_summary_statistics(self):
        samples = [9.0, 2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0]
        s = rb.compute_stats(samples)
        self.assertEqual(s["n"], 8)
        self.assertAlmostEqual(s["mean"], 5.0)
        # Sample standard deviation: sqrt(32 / 7).
        self.assertAlmostEqual(s["stdev"], math.sqrt(32 / 7))
        self.assertEqual((s["min"], s["max"]), (2.0, 9.0))
        self.assertAlmostEqual(s["p50"], 4.5)
        # The samples are kept in measurement order for later analysis.
        self.assertEqual(s["samples"], samples)

    def test_one_sample_has_zero_stdev(self):
        s = rb.compute_stats([3.0])
        self.assertEqual(s["stdev"], 0.0)
        self.assertEqual(s["p99"], 3.0)

    def test_constant_samples(self):
        s = rb.compute_stats([4.0] * 100)
        self.assertEqual(s["stdev"], 0.0)
        self.assertEqual((s["p50"], s["p90"], s["p99"]), (4.0, 4.0, 4.0))

    def test_rejects_no_samples(self):
        with self.assertRaises(ValueError):
            rb.compute_stats([])

    def test_ratio(self):
        self.assertAlmostEqual(rb.ratio(3.0, 2.0), 1.5)
        self.assertIsNone(rb.ratio(3.0, 0.0))
        self.assertIsNone(rb.ratio(None, 2.0))
        self.assertIsNone(rb.ratio(1.0, None))


class ParsingTest(unittest.TestCase):
    def test_maxrss_units(self):
        self.assertEqual(rb.maxrss_to_kib(2048, "linux"), 2048.0)
        self.assertEqual(rb.maxrss_to_kib(2048 * 1024, "darwin"), 2048.0)

    def test_cargo_executable(self):
        lines = [
            "not json",
            json.dumps({"reason": "compiler-artifact", "target": {"name": "sabiko_driver"}, "executable": None}),
            json.dumps({"reason": "compiler-artifact", "target": {"name": "sabikoc"}, "executable": "/t/debug/sabikoc"}),
            json.dumps({"reason": "build-finished", "success": True}),
        ]
        self.assertEqual(rb.parse_cargo_executable("\n".join(lines), "sabikoc"), Path("/t/debug/sabikoc"))
        self.assertIsNone(rb.parse_cargo_executable(lines[1], "sabikoc"))

    def test_host_result(self):
        data = {
            "runtime": ".NET 10.0.14",
            "warmup": 2,
            "iterations": 3,
            "output": "1\n",
            "elapsed_ns": [1_000_000, 2_000_000, 3_000_000],
            "allocated_bytes": [0, 24, 0],
            "gc_collections": {"gen0": 1, "gen1": 0, "gen2": 0},
            "peak_working_set_bytes": 32 * 1024 * 1024,
        }
        m = rb.parse_host_result(data)
        self.assertEqual(m["time_ms"]["samples"], [1.0, 2.0, 3.0])
        self.assertAlmostEqual(m["time_ms"]["p50"], 2.0)
        self.assertEqual(m["allocated_bytes"]["p50"], 0.0)
        self.assertEqual(m["allocated_bytes"]["max"], 24.0)
        self.assertEqual(m["gc_collections"]["gen0"], 1)
        self.assertEqual(m["peak_working_set_mib"], 32.0)
        self.assertEqual(m["output"], "1\n")

    def test_args(self):
        args = rb.parse_args(["--smoke", "fibonacci", "--configs", "release"])
        self.assertEqual(args.benchmarks, ["fibonacci"])
        self.assertEqual(args.configs, ["release"])
        self.assertEqual(args.modes, ["process", "inprocess"])
        self.assertEqual(
            (args.process_warmup, args.process_runs, args.inproc_warmup, args.inproc_iterations), rb.SMOKE_COUNTS
        )
        defaults = rb.parse_args([])
        self.assertEqual(defaults.configs, ["release", "checked"])
        self.assertEqual(defaults.process_runs, rb.DEFAULT_PROCESS_RUNS)
        for bad in (["--configs", "fast"], ["--modes", "process,remote"], ["--process-runs", "0"]):
            with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
                rb.parse_args(bad)


class LaunchTest(unittest.TestCase):
    def test_measures_a_real_process(self):
        result = rb.launch([sys.executable, "-c", "print('hi')"])
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(result.stdout, "hi\n")
        self.assertGreater(result.wall_seconds, 0)
        self.assertGreater(result.max_rss_kib, 0)

    def test_failures_and_mismatches_are_errors(self):
        failed = rb.launch([sys.executable, "-c", "import sys; print('x'); sys.exit(3)"])
        self.assertEqual(failed.exit_code, 3)
        with self.assertRaises(rb.BenchError):
            rb.check_launch(failed, None, "failing")
        ok = rb.launch([sys.executable, "-c", "print('a')"])
        rb.check_launch(ok, "a\n", "matching")
        with self.assertRaises(rb.BenchError):
            rb.check_launch(ok, "b\n", "mismatched")


class CorpusTest(unittest.TestCase):
    def test_every_benchmark_has_both_programs(self):
        benches = rb.find_benchmarks([])
        self.assertGreaterEqual(len(benches), 5)
        for bench in benches:
            self.assertTrue((bench / "bench.cs").is_file(), bench.name)
        self.assertEqual([b.name for b in rb.find_benchmarks(["nbody"])], ["nbody"])
        with self.assertRaises(rb.BenchError):
            rb.find_benchmarks(["no_such_benchmark"])

    def test_rustc_prelude_matches_the_differential_harness(self):
        source = (rb.REPO_ROOT / "crates/sabikoc/tests/differential_run.rs").read_text()
        match = re.search(r'const RUSTC_PRELUDE: &str = "(.*?)";\n', source, re.S)
        self.assertIsNotNone(match)
        self.assertEqual(match.group(1).replace('\\"', '"'), rb.RUSTC_PRELUDE)


def synthetic_results() -> dict:
    def stats(values):
        return rb.compute_stats(values)

    def lang(wall, rss):
        return {"wall_ms": stats(wall), "peak_rss_mib": stats(rss)}

    def host(time_ms, alloc):
        return {
            "time_ms": stats(time_ms),
            "allocated_bytes": stats(alloc),
            "gc_collections": {"gen0": 0, "gen1": 0, "gen2": 0},
            "peak_working_set_mib": 30.0,
            "runtime": ".NET 10",
            "output": "1\n",
        }

    return {
        "schema": rb.SCHEMA_VERSION,
        "generated": "2026-10-29T00:00:00Z",
        "machine": {"label": "test machine", "cpu": "cpu", "cpu_count": 4, "os": "Linux", "notes": ["noisy"]},
        "settings": {
            "configs": ["release"],
            "modes": ["process", "inprocess"],
            "process_warmup": 1,
            "process_runs": 3,
            "inproc_warmup": 1,
            "inproc_iterations": 3,
            "percentile_method": rb.PERCENTILE_METHOD,
        },
        "benchmarks": {
            "fibonacci": {
                "release": {
                    "output": "1\n",
                    "process": {
                        "sabiko": lang([200.0, 200.0, 200.0], [30.0, 30.0, 30.0]),
                        "csharp": lang([100.0, 100.0, 100.0], [30.0, 30.0, 30.0]),
                    },
                    "inprocess": {
                        "sabiko": host([20.0, 20.0, 20.0], [0.0, 0.0, 0.0]),
                        "csharp": host([10.0, 10.0, 10.0], [80.0, 80.0, 80.0]),
                    },
                }
            }
        },
    }


class ReportTest(unittest.TestCase):
    def test_summary_row_has_ratios_and_dashes(self):
        rows = rb.summary_rows(synthetic_results())
        self.assertEqual(len(rows), 1)
        row = dict(zip(rb.SUMMARY_HEADER, rows[0]))
        self.assertEqual(row["Wall S/C#"], "2.00x")
        self.assertEqual(row["Wall Rust (ms)"], "—")
        self.assertEqual(row["RSS S/C#"], "1.00x")
        self.assertEqual(row["Iter S/C#"], "2.00x")
        self.assertEqual(row["Alloc Sabiko (B)"], "0")
        self.assertEqual(row["Alloc S/C#"], "0.00x")

    def test_markdown_report(self):
        md = rb.render_markdown(synthetic_results())
        self.assertIn("**Machine:** test machine", md)
        self.assertIn("> noisy", md)
        self.assertIn("## Sabiko/C# ratios", md)
        self.assertIn("| fibonacci | release | Process wall time | 2.00x | 2.00x | 2.00x | 2.00x |", md)
        self.assertIn("## In-process allocated bytes per iteration (B)", md)
        # Every table row has as many cells as its header.
        for block in md.split("\n\n"):
            rows = [line for line in block.splitlines() if line.startswith("|")]
            if rows:
                self.assertEqual(len({row.count("|") for row in rows}), 1, block)

    def test_text_table(self):
        text = rb.text_table(["a", "bb"], [["xyz", "1"]])
        self.assertEqual(text.splitlines()[0].rstrip(), "a    bb")
        self.assertEqual(len(text.splitlines()), 3)


if __name__ == "__main__":
    unittest.main()

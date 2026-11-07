"""Unit tests for the benchmark history and trend chart.

Run with: python3 -m unittest discover -s benchmarks -p 'test_*.py'
"""

from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import trend  # noqa: E402


def stats(p50: float) -> dict:
    return {"n": 3, "mean": p50, "p50": p50, "p90": p50, "p99": p50, "min": p50, "max": p50, "samples": [p50] * 3}


def results_doc(times: dict[str, tuple[float, float]], generated: str = "2026-11-09T05:12:00Z") -> dict:
    """A run_benchmarks.py-shaped document: {bench: (sabiko ms, C# ms)} in both configs."""
    doc: dict = {
        "schema": 1,
        "generated": generated,
        "machine": {"label": "test runner", "dotnet_runtime": "10.0.14"},
        "benchmarks": {},
    }
    for name, (sab, cs) in times.items():
        for config in ("release", "checked"):
            doc["benchmarks"].setdefault(name, {})[config] = {
                "output": "42\n",
                "process": {
                    "sabiko": {"wall_ms": stats(sab * 2), "peak_rss_mib": stats(30.123456)},
                    "csharp": {"wall_ms": stats(cs * 2), "peak_rss_mib": stats(28.0)},
                    "rust": {"wall_ms": stats(1.0), "peak_rss_mib": stats(2.0)},
                },
                "inprocess": {
                    "sabiko": {"time_ms": stats(sab), "allocated_bytes": stats(0.0), "gc": {}},
                    "csharp": {"time_ms": stats(cs), "allocated_bytes": stats(72.0), "gc": {}},
                },
            }
    return doc


def entry(date: str, times: dict[str, tuple[float, float]]) -> dict:
    return trend.entry_from_results(results_doc(times), commit="0123456789abcdef", date=date)


class EntryTest(unittest.TestCase):
    def test_keeps_p50s_only_rounded_to_four_significant_digits(self):
        e = entry("2026-11-09", {"fibonacci": (11.23456, 11.2)})
        self.assertEqual(e["date"], "2026-11-09")
        self.assertEqual(e["commit"], "0123456789ab")
        self.assertEqual(e["machine"], "test runner")
        self.assertEqual(e["dotnet"], "10.0.14")
        release = e["benchmarks"]["fibonacci"]["release"]
        self.assertEqual(release["inprocess_time_ms"], {"sabiko": 11.23, "csharp": 11.2})
        self.assertEqual(release["process_peak_rss_mib"]["sabiko"], 30.12)
        self.assertEqual(release["process_wall_ms"]["rust"], 1.0)
        self.assertEqual(release["inprocess_allocated_bytes"], {"sabiko": 0.0, "csharp": 72.0})
        self.assertNotIn("samples", json.dumps(e))

    def test_date_defaults_to_the_run_date(self):
        e = trend.entry_from_results(results_doc({"a": (1, 1)}, generated="2026-11-16T05:00:00Z"), None)
        self.assertEqual(e["date"], "2026-11-16")
        self.assertEqual(e["commit"], "")

    def test_a_process_only_run_records_what_it_has(self):
        doc = results_doc({"a": (1, 1)})
        for config in doc["benchmarks"]["a"].values():
            del config["inprocess"]
        e = trend.entry_from_results(doc, "c", "2026-11-09")
        self.assertEqual(sorted(e["benchmarks"]["a"]["release"]), ["process_peak_rss_mib", "process_wall_ms"])


class HistoryTest(unittest.TestCase):
    def test_entries_stay_in_date_order_and_a_rerun_replaces_its_day(self):
        h = trend.add_entry([], entry("2026-11-16", {"a": (2, 1)}))
        h = trend.add_entry(h, entry("2026-11-09", {"a": (3, 1)}))
        h = trend.add_entry(h, entry("2026-11-16", {"a": (4, 1)}))
        self.assertEqual([e["date"] for e in h], ["2026-11-09", "2026-11-16"])
        self.assertEqual(h[1]["benchmarks"]["a"]["release"]["inprocess_time_ms"]["sabiko"], 4)

    def test_dump_is_one_run_per_line_and_round_trips(self):
        h = [entry("2026-11-09", {"a": (2, 1)}), entry("2026-11-16", {"a": (2, 1)})]
        text = trend.dump_history(h)
        self.assertEqual(len(text.splitlines()), 4)
        self.assertEqual(json.loads(text), h)
        self.assertEqual(trend.dump_history([]), "[]\n")


class SeriesTest(unittest.TestCase):
    def test_ratios_and_geomean(self):
        h = [
            entry("2026-11-09", {"a": (2, 1), "b": (0.5, 1)}),
            entry("2026-11-16", {"a": (1, 1)}),  # b missing this week
        ]
        a = trend.series_for(h, "a")
        b = trend.series_for(h, "b")
        overall = trend.series_for(h, None)
        self.assertEqual(a["steady_release"], [2.0, 1.0])
        self.assertEqual(a["process_checked"], [2.0, 1.0])
        self.assertEqual(b["steady_release"], [0.5, None])
        self.assertAlmostEqual(overall["steady_release"][0], 1.0)  # sqrt(2 * 0.5)
        self.assertAlmostEqual(overall["steady_release"][1], 1.0)

    def test_zero_csharp_time_gives_no_ratio(self):
        self.assertIsNone(trend.ratio_of(entry("2026-11-09", {"a": (1, 0)}), "a", "release", "inprocess_time_ms"))
        self.assertIsNone(trend.geomean([]))


class ChartTest(unittest.TestCase):
    def parse(self, svg: str) -> ET.Element:
        return ET.fromstring(svg)  # well-formed XML, or this raises

    def test_placeholder_without_history(self):
        svg = trend.render_svg([])
        self.parse(svg)
        self.assertIn("No history yet", svg)

    def test_one_panel_per_benchmark_plus_the_geomean(self):
        h = [entry("2026-11-09", {"a": (2, 1), "b": (1, 1)}), entry("2026-11-16", {"a": (1.5, 1), "b": (1, 1)})]
        svg = trend.render_svg(h)
        root = self.parse(svg)
        ns = "{http://www.w3.org/2000/svg}"
        titles = [t.text for t in root.iter(f"{ns}text") if t.get("class") == "panel-title"]
        self.assertEqual(titles[1:], ["a", "b"])
        self.assertTrue(titles[0].startswith("Geometric mean"))
        # Four series, each a line in each of the three panels.
        self.assertEqual(sum(1 for p in root.iter(f"{ns}path")), 12)
        self.assertIn("2.00x", svg)  # a's first-week ratio sets the scale's top
        self.assertIn("<title>Steady state, release: 1.50x (2026-11-16, 0123456789ab)</title>", svg)

    def test_a_single_run_still_draws_points(self):
        svg = trend.render_svg([entry("2026-11-09", {"a": (2, 1)})])
        self.parse(svg)
        self.assertIn("<circle", svg)
        self.assertNotIn("<path", svg)

    def test_y_range_always_shows_parity(self):
        self.assertEqual(trend.y_range([1.0]), (0.75, 1.25))
        self.assertEqual(trend.y_range([0.6, 2.1]), (0.5, 2.25))
        self.assertIn(1.0, trend.y_ticks(0.5, 4.0))

    def test_date_ticks_keep_the_ends(self):
        dates = [f"2026-01-{d:02d}" for d in range(1, 31)]
        ticks = trend.date_ticks(dates, 4)
        self.assertEqual(ticks[0], 0)
        self.assertEqual(ticks[-1], 29)
        self.assertLessEqual(len(ticks), 5)


class CliTest(unittest.TestCase):
    def test_record_then_chart(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            results = tmp / "results.json"
            history = tmp / "history.json"
            chart = tmp / "trend.svg"
            results.write_text(json.dumps(results_doc({"fibonacci": (11.0, 10.0)})))
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(trend.main(["record", "--results", str(results), "--commit", "abc", "--history", str(history)]), 0)
                self.assertEqual(trend.main(["chart", "--history", str(history), "--output", str(chart)]), 0)
            self.assertEqual(len(json.loads(history.read_text())), 1)
            self.assertIn("1.10x", chart.read_text())

    def test_record_rejects_an_empty_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp) / "results.json"
            results.write_text(json.dumps({"generated": "2026-11-09T00:00:00Z", "benchmarks": {}}))
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(
                    trend.main(["record", "--results", str(results), "--history", str(Path(tmp) / "h.json")]), 1
                )


if __name__ == "__main__":
    unittest.main()

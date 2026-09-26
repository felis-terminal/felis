#!/usr/bin/env python3
"""Unit tests for tools/bench/criterion.py.

Hermetic — synthetic Criterion trees and canned cargo-metadata JSON,
no cargo invocation — so CI can run them before the real gate and a
parser break is caught here, not on a real regression PR (same role
the bench-gate.test.sh smoke tests played for the bash gate).

Run: python3 tools/bench/criterion_test.py
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import criterion as bench


def make_change(root: Path, bench_id: str, point: float) -> None:
    """Synthetic `change/estimates.json` with the given mean change."""
    d = root / bench_id / "change"
    d.mkdir(parents=True)
    d.joinpath("estimates.json").write_text(
        json.dumps(
            {
                "mean": {
                    "confidence_interval": {
                        "confidence_level": 0.95,
                        "lower_bound": point,
                        "upper_bound": point,
                    },
                    "point_estimate": point,
                    "standard_error": 0.001,
                }
            }
        )
    )


def make_new(
    root: Path, bench_id: str, mean_ns: float, bytes_: int | None = None
) -> None:
    """Synthetic `new/{estimates,benchmark}.json` for one bench."""
    d = root / bench_id / "new"
    d.mkdir(parents=True)
    d.joinpath("estimates.json").write_text(
        json.dumps({"mean": {"point_estimate": mean_ns, "standard_error": 1.0}})
    )
    benchmark = {"full_id": bench_id}
    if bytes_ is not None:
        benchmark["throughput"] = {"Bytes": bytes_}
    d.joinpath("benchmark.json").write_text(json.dumps(benchmark))


class GateTest(unittest.TestCase):
    """Exit-code contract ported from bench-gate.test.sh (t1–t6)."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "criterion"
        self.root.mkdir()
        self.addCleanup(self.tmp.cleanup)

    def test_exits_2_when_no_change_files(self):
        self.assertEqual(bench.gate(self.root, 0.15, []), 2)

    def test_exits_2_when_root_missing(self):
        self.assertEqual(bench.gate(self.root / "absent", 0.15, []), 2)

    def test_exits_0_when_all_within_threshold(self):
        make_change(self.root, "daemon_side/csi_redraw", -0.05)
        make_change(self.root, "daemon_side/plaintext_scroll", 0.02)
        self.assertEqual(bench.gate(self.root, 0.15, []), 0)

    def test_exits_1_on_regression_past_threshold(self):
        make_change(self.root, "daemon_side/csi_redraw", -0.05)
        make_change(self.root, "daemon_side/plaintext_scroll", 0.20)
        self.assertEqual(bench.gate(self.root, 0.15, []), 1)

    def test_allowlist_bypasses_regressing_bench(self):
        make_change(self.root, "daemon_side/csi_redraw", -0.05)
        make_change(self.root, "daemon_side/plaintext_scroll", 0.20)
        rc = bench.gate(self.root, 0.15, ["daemon_side/plaintext_scroll"])
        self.assertEqual(rc, 0)

    def test_tighter_threshold_catches_small_regression(self):
        make_change(self.root, "daemon_side/csi_redraw", 0.05)
        self.assertEqual(bench.gate(self.root, 0.03, []), 1)

    def test_noise_within_threshold_passes(self):
        make_change(self.root, "daemon_side/csi_redraw", 0.03)
        make_change(self.root, "daemon_side/plaintext_scroll", 0.10)
        self.assertEqual(bench.gate(self.root, 0.15, []), 0)


class CollectTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "criterion"
        self.root.mkdir()
        self.addCleanup(self.tmp.cleanup)

    def test_collects_id_mean_and_bytes_throughput(self):
        # 1 MiB in 1 ms → 1000 MiB/s, a hand-checkable rate.
        make_new(self.root, "ascii/flood", mean_ns=1e6, bytes_=1 << 20)
        results = bench.collect_results(self.root)
        self.assertEqual(len(results), 1)
        self.assertEqual(results[0]["id"], "ascii/flood")
        self.assertEqual(results[0]["mean_ns"], 1e6)
        self.assertEqual(results[0]["throughput"]["unit"], "MiB/s")
        self.assertAlmostEqual(results[0]["throughput"]["rate"], 1000.0)

    def test_no_throughput_when_benchmark_json_has_none(self):
        make_new(self.root, "grid/resize", mean_ns=500.0)
        results = bench.collect_results(self.root)
        self.assertIsNone(results[0]["throughput"])

    def test_since_filters_out_stale_results(self):
        make_new(self.root, "ascii/old", mean_ns=1.0)
        far_future = self.root.joinpath("ascii/old").stat().st_mtime + 3600
        self.assertEqual(bench.collect_results(self.root, since=far_future), [])

    def test_markdown_renders_one_row_per_bench(self):
        make_new(self.root, "ascii/flood", mean_ns=1e6, bytes_=1 << 20)
        md = bench.render_markdown(bench.collect_results(self.root))
        self.assertIn("| `ascii/flood` | 1.00 ms | 1000.0 MiB/s |", md)


class DiscoveryTest(unittest.TestCase):
    METADATA = {
        "packages": [
            {
                "name": "felis-protocol",
                "targets": [
                    {"name": "felis-protocol", "kind": ["lib"]},
                    {
                        "name": "ipc_throughput",
                        "kind": ["bench"],
                        "required-features": ["postcard"],
                    },
                ],
            },
            {
                "name": "felis-vt",
                "targets": [
                    {"name": "parser_throughput", "kind": ["bench"]},
                ],
            },
        ]
    }

    def test_only_bench_targets_with_features(self):
        targets = bench.bench_targets(self.METADATA)
        self.assertEqual(
            targets,
            [
                {
                    "package": "felis-protocol",
                    "name": "ipc_throughput",
                    "required_features": ["postcard"],
                },
                {
                    "package": "felis-vt",
                    "name": "parser_throughput",
                    "required_features": [],
                },
            ],
        )

    def test_required_features_reach_cargo_argv(self):
        args = bench.parse_args(["run", "--quick", "--save-baseline", "ci-base"])
        cmd = bench.cargo_bench_cmd(bench.bench_targets(self.METADATA)[0], args)
        self.assertEqual(
            cmd,
            [
                "cargo",
                "bench",
                "-p",
                "felis-protocol",
                "--bench",
                "ipc_throughput",
                "--features",
                "postcard",
                "--",
                "--warm-up-time",
                "1",
                "--measurement-time",
                "3",
                "--save-baseline",
                "ci-base",
            ],
        )


class FormatTest(unittest.TestCase):
    def test_ns_unit_ladder(self):
        self.assertEqual(bench.format_ns(950), "950.00 ns")
        self.assertEqual(bench.format_ns(74033.5), "74.03 µs")
        self.assertEqual(bench.format_ns(1.5e7), "15.00 ms")
        self.assertEqual(bench.format_ns(2.5e9), "2.50 s")


if __name__ == "__main__":
    unittest.main()

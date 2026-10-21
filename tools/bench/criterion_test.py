"""Unit tests for tools/bench/criterion.py.

Hermetic — synthetic Criterion trees and canned cargo-metadata JSON,
no cargo invocation — so CI can run them and a parser break is
caught here rather than as a gate that silently passes a local A/B.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

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


@pytest.fixture
def root(tmp_path: Path) -> Path:
    root = tmp_path / "criterion"
    root.mkdir()
    return root


def test_gate_exits_2_when_no_change_files(root):
    assert bench.gate(root, 0.15, []) == 2


def test_gate_exits_2_when_root_missing(root):
    assert bench.gate(root / "absent", 0.15, []) == 2


def test_gate_exits_0_when_all_within_threshold(root):
    make_change(root, "daemon_side/csi_redraw", -0.05)
    make_change(root, "daemon_side/plaintext_scroll", 0.02)
    assert bench.gate(root, 0.15, []) == 0


def test_gate_exits_1_on_regression_past_threshold(root):
    make_change(root, "daemon_side/csi_redraw", -0.05)
    make_change(root, "daemon_side/plaintext_scroll", 0.20)
    assert bench.gate(root, 0.15, []) == 1


def test_gate_allowlist_bypasses_regressing_bench(root):
    make_change(root, "daemon_side/csi_redraw", -0.05)
    make_change(root, "daemon_side/plaintext_scroll", 0.20)
    assert bench.gate(root, 0.15, ["daemon_side/plaintext_scroll"]) == 0


def test_gate_tighter_threshold_catches_small_regression(root):
    make_change(root, "daemon_side/csi_redraw", 0.05)
    assert bench.gate(root, 0.03, []) == 1


def test_gate_noise_within_threshold_passes(root):
    make_change(root, "daemon_side/csi_redraw", 0.03)
    make_change(root, "daemon_side/plaintext_scroll", 0.10)
    assert bench.gate(root, 0.15, []) == 0


def test_collect_reads_id_mean_and_bytes_throughput(root):
    # 1 MiB in 1 ms → 1000 MiB/s, a hand-checkable rate.
    make_new(root, "ascii/flood", mean_ns=1e6, bytes_=1 << 20)
    results = bench.collect_results(root)
    assert len(results) == 1
    assert results[0]["id"] == "ascii/flood"
    assert results[0]["mean_ns"] == 1e6
    assert results[0]["throughput"]["unit"] == "MiB/s"
    assert results[0]["throughput"]["rate"] == pytest.approx(1000.0)


def test_collect_has_no_throughput_when_benchmark_json_has_none(root):
    make_new(root, "grid/resize", mean_ns=500.0)
    results = bench.collect_results(root)
    assert results[0]["throughput"] is None


def test_collect_since_filters_out_stale_results(root):
    make_new(root, "ascii/old", mean_ns=1.0)
    far_future = root.joinpath("ascii/old").stat().st_mtime + 3600
    assert bench.collect_results(root, since=far_future) == []


def test_markdown_renders_one_row_per_bench(root):
    make_new(root, "ascii/flood", mean_ns=1e6, bytes_=1 << 20)
    md = bench.render_markdown(bench.collect_results(root))
    assert "| `ascii/flood` | 1.00 ms | 1000.0 MiB/s |" in md


METADATA = {
    "packages": [
        {
            "name": "felis-protocol",
            "targets": [
                {
                    "name": "felis-protocol",
                    "kind": ["lib"],
                    "src_path": "src/lib.rs",
                },
                {
                    "name": "ipc_throughput",
                    "kind": ["bench"],
                    "src_path": "benches/ipc_throughput.rs",
                    "required-features": ["postcard"],
                },
            ],
        },
        {
            "name": "felis-vt",
            "targets": [
                {
                    "name": "parser_throughput",
                    "kind": ["bench"],
                    "src_path": "benches/parser_throughput.rs",
                },
                {
                    "name": "gungraun_gate",
                    "kind": ["bench"],
                    "src_path": "benches/gungraun_gate.rs",
                },
            ],
        },
    ]
}


def is_criterion(src_path: str) -> bool:
    return not src_path.endswith("gungraun_gate.rs")


def test_discovery_keeps_only_criterion_bench_targets_with_features():
    targets = bench.bench_targets(METADATA, is_criterion)
    assert targets == [
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
    ]


def test_discovery_required_features_reach_cargo_argv():
    args = bench.parse_args(["run", "--quick", "--save-baseline", "ci-base"])
    cmd = bench.cargo_bench_cmd(bench.bench_targets(METADATA, is_criterion)[0], args)
    assert cmd == [
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
    ]


@pytest.mark.parametrize(
    ("ns", "text"),
    [
        (950, "950.00 ns"),
        (74033.5, "74.03 µs"),
        (1.5e7, "15.00 ms"),
        (2.5e9, "2.50 s"),
    ],
)
def test_format_ns_unit_ladder(ns, text):
    assert bench.format_ns(ns) == text

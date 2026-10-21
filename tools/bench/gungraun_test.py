"""Unit tests for tools/bench/gungraun.py.

Hermetic: synthetic gungraun summary trees, no cargo or valgrind, so a
parser break surfaces here rather than as a gate that passes everything.
"""

from __future__ import annotations

import contextlib
import io
import json
from pathlib import Path

import pytest

import gungraun


def make_summary(
    home: Path,
    function: str,
    bench_id: str,
    new: int,
    old: int | None,
    regressed: bool,
    group: str = "gate",
) -> None:
    values = {"new": new} if old is None else {"new": new, "old": old}
    regressions = (
        [
            {
                "Soft": {
                    "metric": "Ir",
                    "new": new,
                    "old": old,
                    "diff_pct": "5",
                    "limit": "2",
                }
            }
        ]
        if regressed
        else []
    )
    d = home / "felis-client-core" / "gungraun_gate" / group / f"{function}.{bench_id}"
    d.mkdir(parents=True)
    d.joinpath("summary.json").write_text(
        json.dumps(
            {
                "function_name": function,
                "id": bench_id,
                "module_path": f"gungraun_gate::{group}::{function}",
                "profiles": [
                    {
                        "tool": "Callgrind",
                        "data": {
                            "total": {
                                "metrics": {"Ir": {"values": values}},
                                "regressions": regressions,
                            }
                        },
                    }
                ],
            }
        )
    )


def run_verdict(
    rows: list[dict],
    rc: int = 0,
    accepted: bool = False,
    base_ids: set[str] | None = None,
) -> int:
    if base_ids is None:
        base_ids = {row["key"] for row in rows}
    with (
        contextlib.redirect_stdout(io.StringIO()),
        contextlib.redirect_stderr(io.StringIO()),
    ):
        return gungraun.verdict(rows, base_ids, rc, accepted, 2.0)


BASE_IDS = {
    "gungraun_gate::gate::daemon_side::csi_redraw",
    "gungraun_gate::gate::daemon_side::shell_prompt",
}


def test_unchanged_counts_pass(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 100, 100, False)
    assert run_verdict(gungraun.collect(tmp_path)) == 0


def test_regression_past_limit_fails(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 105, 100, True)
    make_summary(tmp_path, "daemon_side", "shell_prompt", 100, 100, False)
    rows = gungraun.collect(tmp_path)
    assert run_verdict(rows, rc=gungraun.REGRESSION_RC) == 1


def test_accept_label_passes_a_regression(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 105, 100, True)
    rows = gungraun.collect(tmp_path)
    assert run_verdict(rows, rc=gungraun.REGRESSION_RC, accepted=True) == 0


def test_accept_label_does_not_hide_a_failed_run(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 100, 100, False)
    rows = gungraun.collect(tmp_path)
    assert run_verdict(rows, rc=101, accepted=True) == 1


def test_accept_label_does_not_hide_a_crash_after_a_regression(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 105, 100, True)
    rows = gungraun.collect(tmp_path)
    assert run_verdict(rows, rc=101, accepted=True) == 1


@pytest.mark.parametrize("accepted", [False, True])
def test_a_base_bench_missing_from_head_fails_even_when_accepted(tmp_path, accepted):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 100, 100, False)
    rows = gungraun.collect(tmp_path)
    assert run_verdict(rows, accepted=accepted, base_ids=BASE_IDS) == 1


def test_a_bench_new_in_head_does_not_fail(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 100, 100, False)
    make_summary(tmp_path, "daemon_side", "fresh", 100, None, False)
    rows = gungraun.collect(tmp_path)
    base = {row["key"] for row in rows if row["old"] is not None}
    assert run_verdict(rows, base_ids=base) == 0


def test_a_base_bench_moved_to_another_group_fails(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 100, 100, False)
    make_summary(tmp_path, "daemon_side", "shell_prompt", 100, None, False, "moved")
    rows = gungraun.collect(tmp_path)
    assert run_verdict(rows, base_ids=BASE_IDS) == 1


def test_a_base_bench_run_without_a_base_count_fails(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 100, 100, False)
    make_summary(tmp_path, "daemon_side", "shell_prompt", 100, None, False)
    rows = gungraun.collect(tmp_path)
    base = {row["key"] for row in rows}
    assert run_verdict(rows, base_ids=base) == 1


def test_a_regression_exit_without_a_recorded_regression_fails(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 100, 100, False)
    rows = gungraun.collect(tmp_path)
    assert run_verdict(rows, rc=gungraun.REGRESSION_RC) == 1


def test_nothing_compared_exits_2(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 100, None, False)
    assert run_verdict(gungraun.collect(tmp_path), base_ids=set()) == 2
    assert run_verdict([]) == 2


def test_report_marks_the_regressed_bench(tmp_path):
    make_summary(tmp_path, "daemon_side", "csi_redraw", 105, 100, True)
    report = gungraun.render(gungraun.collect(tmp_path))
    assert "daemon_side/csi_redraw" in report
    assert "+5.00%" in report
    assert "REGRESSED" in report


def test_accept_label_in_event_labels_accepts():
    labels = json.dumps([{"name": "ci"}, {"name": gungraun.ACCEPT_LABEL}])
    assert gungraun.label_accepts(labels)


def test_other_labels_do_not_accept():
    assert not gungraun.label_accepts(json.dumps([{"name": "ci"}]))


@pytest.mark.parametrize("labels", ["", "null", "[]"])
def test_absent_labels_do_not_accept(labels):
    assert not gungraun.label_accepts(labels)

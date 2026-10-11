#!/usr/bin/env python3
"""Unit tests for tools/bench/gungraun.py.

Hermetic: synthetic gungraun summary trees, no cargo or valgrind, so a
parser break surfaces here rather than as a gate that passes everything.

Run: python3 tools/bench/gungraun_test.py
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


class GateTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.home = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def test_unchanged_counts_pass(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 100, 100, False)
        self.assertEqual(run_verdict(gungraun.collect(self.home)), 0)

    def test_regression_past_limit_fails(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 105, 100, True)
        make_summary(self.home, "daemon_side", "shell_prompt", 100, 100, False)
        rows = gungraun.collect(self.home)
        self.assertEqual(run_verdict(rows, rc=gungraun.REGRESSION_RC), 1)

    def test_accept_label_passes_a_regression(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 105, 100, True)
        rows = gungraun.collect(self.home)
        self.assertEqual(run_verdict(rows, rc=gungraun.REGRESSION_RC, accepted=True), 0)

    def test_accept_label_does_not_hide_a_failed_run(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 100, 100, False)
        rows = gungraun.collect(self.home)
        self.assertEqual(run_verdict(rows, rc=101, accepted=True), 1)

    def test_accept_label_does_not_hide_a_crash_after_a_regression(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 105, 100, True)
        rows = gungraun.collect(self.home)
        self.assertEqual(run_verdict(rows, rc=101, accepted=True), 1)

    def test_a_base_bench_missing_from_head_fails_even_when_accepted(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 100, 100, False)
        rows = gungraun.collect(self.home)
        base = {
            "gungraun_gate::gate::daemon_side::csi_redraw",
            "gungraun_gate::gate::daemon_side::shell_prompt",
        }
        self.assertEqual(run_verdict(rows, base_ids=base), 1)
        self.assertEqual(run_verdict(rows, accepted=True, base_ids=base), 1)

    def test_a_bench_new_in_head_does_not_fail(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 100, 100, False)
        make_summary(self.home, "daemon_side", "fresh", 100, None, False)
        rows = gungraun.collect(self.home)
        base = {row["key"] for row in rows if row["old"] is not None}
        self.assertEqual(run_verdict(rows, base_ids=base), 0)

    def test_a_base_bench_moved_to_another_group_fails(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 100, 100, False)
        make_summary(
            self.home, "daemon_side", "shell_prompt", 100, None, False, "moved"
        )
        rows = gungraun.collect(self.home)
        base = {
            "gungraun_gate::gate::daemon_side::csi_redraw",
            "gungraun_gate::gate::daemon_side::shell_prompt",
        }
        self.assertEqual(run_verdict(rows, base_ids=base), 1)

    def test_a_base_bench_run_without_a_base_count_fails(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 100, 100, False)
        make_summary(self.home, "daemon_side", "shell_prompt", 100, None, False)
        rows = gungraun.collect(self.home)
        base = {row["key"] for row in rows}
        self.assertEqual(run_verdict(rows, base_ids=base), 1)

    def test_a_regression_exit_without_a_recorded_regression_fails(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 100, 100, False)
        rows = gungraun.collect(self.home)
        self.assertEqual(run_verdict(rows, rc=gungraun.REGRESSION_RC), 1)

    def test_nothing_compared_exits_2(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 100, None, False)
        self.assertEqual(run_verdict(gungraun.collect(self.home), base_ids=set()), 2)
        self.assertEqual(run_verdict([]), 2)

    def test_report_marks_the_regressed_bench(self):
        make_summary(self.home, "daemon_side", "csi_redraw", 105, 100, True)
        report = gungraun.render(gungraun.collect(self.home))
        self.assertIn("daemon_side/csi_redraw", report)
        self.assertIn("+5.00%", report)
        self.assertIn("REGRESSED", report)


class LabelTest(unittest.TestCase):
    def test_accept_label_in_event_labels(self):
        labels = json.dumps([{"name": "ci"}, {"name": gungraun.ACCEPT_LABEL}])
        self.assertTrue(gungraun.label_accepts(labels))

    def test_other_labels_do_not_accept(self):
        self.assertFalse(gungraun.label_accepts(json.dumps([{"name": "ci"}])))

    def test_absent_labels_do_not_accept(self):
        self.assertFalse(gungraun.label_accepts(""))
        self.assertFalse(gungraun.label_accepts("null"))
        self.assertFalse(gungraun.label_accepts("[]"))


if __name__ == "__main__":
    unittest.main()

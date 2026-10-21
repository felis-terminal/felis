"""Unit tests for the cross-terminal suite (loaders.py, report.py, envinfo.py).

Hermetic — synthetic harness artifacts and recorded command output, no
terminal is launched and nothing is shelled out — so a parser, scale or
provenance break is caught here rather than after a 30-minute suite run
has produced a wrong-looking chart.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import fcntl
import os
import plistlib
import pty
import re
import shlex
import signal
import subprocess
import struct
import sys
import termios
import time
from pathlib import Path
from unittest import mock

import pytest

import crossterm as ct
import envinfo
import field
import firstwindow
import loaders as ld
import report
import suites
import usage
import wm
from wm import mactiler

PRESENTING = {"locked": False, "asleep": False}


@pytest.fixture
def presenting(monkeypatch):
    """Sample every leg's machine from fixtures: presentable, unthrottled."""
    monkeypatch.setattr(envinfo, "display_state", lambda: dict(PRESENTING))
    monkeypatch.setattr(envinfo, "throttle_state", lambda: "unavailable")


class TestVtebenchLoader:
    def test_mean_and_stddev_per_benchmark(self, tmp_path):
        (tmp_path / "felis.dat").write_text(
            "scrolling unicode\n10 100\n20 120\n30 140\n"
        )
        (suite,) = ld.load_vtebench(tmp_path)
        point = suite.data["felis"]["scrolling"]
        assert point.value == pytest.approx(20.0)
        assert point.err == pytest.approx(8.16496580927726)
        assert suite.data["felis"]["unicode"].value == pytest.approx(120.0)

    def test_underscore_samples_are_dropped_not_zeroed(self, tmp_path):
        (tmp_path / "kitty.dat").write_text("a b\n10 _\n30 _\n")
        (suite,) = ld.load_vtebench(tmp_path)
        assert suite.data["kitty"]["a"].value == pytest.approx(20.0)
        assert "b" not in suite.data["kitty"]
        # A benchmark no terminal produced must not leave an empty row.
        assert suite.categories == ["a"]

    def test_no_dat_files_yields_no_suite(self, tmp_path):
        assert ld.load_vtebench(tmp_path) == []


class TestTermbenchLoader:
    def test_stalled_category_keeps_the_marker(self, tmp_path):
        (tmp_path / "ghostty.json").write_text(
            json.dumps(
                [
                    {"name": "binary", "MB/s": "stalled"},
                    {"name": "many_lines", "MB/s": 512.5},
                ]
            )
        )
        (suite,) = ld.load_termbench(tmp_path)
        assert suite.data["ghostty"]["binary"].value == 0.0
        assert suite.data["ghostty"]["binary"].note == "stalled"
        assert suite.data["ghostty"]["many_lines"].value == 512.5


class TestCatLoader:
    def test_one_bar_per_payload_named_by_hyperfine(self, tmp_path):
        (tmp_path / "felis.json").write_text(
            json.dumps(
                {
                    "results": [
                        {"command": "ascii", "mean": 1.25, "stddev": 0.03},
                        {"command": "unicode", "mean": 2.5, "stddev": 0.1},
                    ]
                }
            )
        )
        (suite,) = ld.load_cat(tmp_path)
        assert suite.categories == ["ascii", "unicode"]
        assert suite.data["felis"]["ascii"].value == pytest.approx(1.25)
        assert suite.data["felis"]["unicode"].err == pytest.approx(0.1)

    def test_the_pty_drain_caveat_travels_with_the_chart(self, tmp_path):
        # The number is not parse time, and a reader who does not know
        # that will read the cat and kittenbench charts as the same
        # measurement taken twice.
        (tmp_path / "kitty.json").write_text(
            json.dumps({"results": [{"command": "ascii", "mean": 1.0}]})
        )
        (suite,) = ld.load_cat(tmp_path)
        assert any("kittenbench" in n for n in suite.notes)


class TestKittenbenchLoader:
    def test_rate_survives_the_color_kitten_prints_it_in(self, tmp_path):
        (tmp_path / "felis.kitten").write_text(
            "  Only ASCII chars : 1.234s    @ \x1b[32m812.5  \x1b[m MB/s\n"
            "These results measure the time it takes to parse.\n"
            "  Unicode chars    : 2.5s      @ \x1b[32m120.25 \x1b[m MB/s\n"
        )
        (suite,) = ld.load_kittenbench(tmp_path)
        assert suite.categories == ["Only ASCII chars", "Unicode chars"]
        assert suite.data["felis"]["Only ASCII chars"].value == pytest.approx(812.5)

    def test_prose_lines_are_not_benchmarks(self, tmp_path):
        (tmp_path / "kitty.kitten").write_text(
            "Note that rendering is suppressed, so MB/s is parse only.\n"
        )
        assert ld.load_kittenbench(tmp_path) == []


class TestDoomFireLoader:
    def test_fps_and_the_byte_rate_it_implies(self, tmp_path):
        (tmp_path / "felis.doom-fire").write_text(
            "doom-fire frames 2827 secs 20.000 fps 141.350 bytes_avg 1048576\n"
        )
        (suite,) = ld.load_doom_fire(tmp_path)
        assert suite.data["felis"]["DOOM-fire"].value == pytest.approx(141.35)
        assert any("141 MB/s" in n for n in suite.notes)

    def test_a_run_that_never_reported_is_left_out(self, tmp_path):
        (tmp_path / "wezterm.doom-fire").write_text("")
        assert ld.load_doom_fire(tmp_path) == []


class TestPayload:
    def test_the_same_seed_gives_the_same_bytes(self, tmp_path):
        # The whole point of generating rather than downloading: two runs
        # on two machines must flood the terminals with identical files.
        import payloads

        a = payloads.build("csi", 1, tmp_path / "a")
        b = payloads.build("csi", 1, tmp_path / "b")
        assert a.read_bytes() == b.read_bytes()
        assert payloads.digest(a) == payloads.digest(b)

    def test_size_is_exact_and_kinds_differ(self, tmp_path):
        import payloads

        built = {k: payloads.build(k, 1, tmp_path) for k in payloads.KINDS}
        for path in built.values():
            assert path.stat().st_size == 1 << 20
        assert len({p.read_bytes() for p in built.values()}) == 3

    def test_a_cached_payload_of_the_wrong_size_is_rebuilt(self, tmp_path):
        import payloads

        stale = tmp_path / "ascii-1mb.bin"
        tmp_path.mkdir(exist_ok=True)
        stale.write_bytes(b"truncated")
        assert payloads.build("ascii", 1, tmp_path).stat().st_size == 1 << 20


class TestStartupLoader:
    def test_felis_legs_fold_into_one_terminal(self, tmp_path):
        for name, mean in (
            ("felis-cold", 0.2),
            ("felis-warm", 0.08),
            ("kitty", 0.14),
        ):
            (tmp_path / f"{name}.json").write_text(
                json.dumps({"results": [{"mean": mean, "stddev": 0.01}]})
            )
        (suite,) = ld.load_startup(tmp_path)
        assert suite.terminals() == ["felis", ld.WARM, "kitty"]
        assert suite.categories == ["launch"]
        assert suite.data["felis"]["launch"].value == pytest.approx(200.0)
        assert suite.data[ld.WARM]["launch"].value == pytest.approx(80.0)
        assert suite.label("felis") == "felis (cold)"

    def test_felis_warm_is_never_the_best_other_terminal(self, tmp_path):
        for name, mean in (
            ("felis-cold", 0.2),
            ("felis-warm", 0.08),
            ("kitty", 0.3),
        ):
            (tmp_path / f"{name}.json").write_text(
                json.dumps({"results": [{"mean": mean}]})
            )
        (suite,) = ld.load_startup(tmp_path)
        for term in suite.samples:
            for per_round in suite.samples[term].values():
                per_round[2] = per_round[1]
        suite.rounds = 2
        assert report.ratio(suite, "launch")[0] == "1.50x"

    def test_tiler_timing_caveat_is_in_the_subtitle(self, tmp_path):
        (tmp_path / "kitty.json").write_text(json.dumps({"results": [{"mean": 0.1}]}))
        (suite,) = ld.load_startup(tmp_path)
        assert "animation completion is not timed" in suite.subtitle
        assert "work delaying first configure is included" in suite.subtitle


class TestFirstWindowLoader:
    @staticmethod
    def write(directory: Path, stem: str, samples, **extra) -> None:
        payload = {"watcher": "niri", "samples_ms": samples} | extra
        (directory / f"{stem}.startup.json").write_text(json.dumps(payload))

    def test_the_bar_is_the_median_launch(self, tmp_path):
        self.write(tmp_path, "kitty", [100.0, 110.0, 300.0, 120.0, 105.0])
        self.write(tmp_path, "felis-cold", [80.0, 90.0])
        self.write(tmp_path, "felis-warm", [40.0, 50.0])
        (suite,) = ld.load_startup(tmp_path)
        assert suite.terminals() == ["felis", ld.WARM, "kitty"]
        assert suite.categories == ["first window"]
        assert suite.data["kitty"]["first window"].value == pytest.approx(110.0)
        assert suite.data[ld.WARM]["first window"].value == pytest.approx(45.0)
        assert "first window" in suite.title

    def test_a_terminal_that_showed_no_window_has_no_bar_and_says_why(self, tmp_path):
        self.write(tmp_path, "kitty", [100.0])
        self.write(
            tmp_path, "foot", [], failed="launch 1 of 13 put no window on screen"
        )
        (suite,) = ld.load_startup(tmp_path)
        assert "foot" not in suite.data
        assert "foot: launch 1 of 13" in " ".join(suite.notes)

    def test_macos_is_not_claimed_to_prove_a_drawn_frame(self, tmp_path):
        self.write(tmp_path, "kitty", [100.0])
        (suite,) = ld.load_startup(tmp_path)
        assert "does not prove" in suite.subtitle
        assert "first buffer committed" in suite.subtitle

    def test_an_older_root_s_lifetimes_are_labelled_as_such(self, tmp_path):
        (tmp_path / "kitty.json").write_text(json.dumps({"results": [{"mean": 0.1}]}))
        (suite,) = ld.load_startup(tmp_path)
        assert "process lifetime" in suite.subtitle


class TestMemoryLoader:
    @staticmethod
    def write(directory: Path, term: str, **record) -> None:
        payload = {
            "metric": "pss",
            "idle_kb": 102400,
            "flooded_kb": 204800,
            "peak_kb": None,
            "idle_cpu_cs": 100,
            "flooded_cpu_cs": 512,
        } | record
        (directory / f"{term}.mem.json").write_text(json.dumps(payload))

    def test_memory_in_mb_and_cpu_in_seconds(self, tmp_path):
        self.write(tmp_path, "felis")
        rss, cpu = ld.load_memory(tmp_path)
        assert rss.data["felis"]["idle"].value == pytest.approx(100.0)
        assert rss.data["felis"]["after 200k-line flood"].value == pytest.approx(200.0)
        # 4.12, not 5.12: the CPU already spent when the flood began
        # is startup and the idle wait, which the flood did not do
        # and which the field does not spend equally.
        assert cpu.data["felis"]["200k-line flood"].value == pytest.approx(4.12)

    def test_the_metric_is_named_on_the_chart_it_was_drawn_for(self, tmp_path):
        # Two runs under different accountings do not belong on one axis,
        # so the axis has to say which one it is.
        self.write(tmp_path, "felis", metric="phys_footprint")
        rss, _cpu = ld.load_memory(tmp_path)
        assert "Physical footprint" in rss.subtitle

    def test_the_peak_says_it_is_not_the_metric_above_it(self, tmp_path):
        self.write(tmp_path, "felis", peak_kb=307200)
        rss, _cpu = ld.load_memory(tmp_path)
        note = " ".join(rss.notes)
        assert "felis 300" in note
        assert "VmHWM" in note

    def test_a_run_from_before_the_metric_changed_still_charts(self, tmp_path):
        # Dropping it would read as a suite that was never run, and the
        # bars are still the numbers that run took.
        (tmp_path / "felis.mem").write_text("102400 204800 412\n")
        (rss,) = ld.load_memory(tmp_path)
        assert rss.data["felis"]["idle"].value == pytest.approx(100.0)
        assert "superseded" in rss.subtitle

    def test_the_memory_note_names_the_ledger_not_a_decomposition(self, tmp_path):
        # The note claims only what the ledger defines: in vs. out of
        # the OS's own charge to the process. It must not claim a
        # breakdown (heap, mapping, atlas) the harness cannot check.
        self.write(tmp_path, "felis")
        rss, _cpu = ld.load_memory(tmp_path)
        assert "what the OS charges to the processes" in rss.subtitle
        assert "GPU driver allocates" in rss.subtitle
        for claim in ("GPU-side", "heap-side", "outside every one"):
            assert claim not in rss.subtitle

    def test_the_flood_cpu_note_says_sampled_not_all(self, tmp_path):
        self.write(tmp_path, "felis")
        _rss, cpu = ld.load_memory(tmp_path)
        assert "utime + stime" in cpu.subtitle
        assert "named after it or its helpers" in cpu.subtitle
        assert "parse plus render" not in cpu.subtitle


class TestMemorySampler:
    """What the memory suite counts, and what it refuses to count."""

    @staticmethod
    def fake_proc(root: Path, pid: int, pss: int, hwm: int) -> Path:
        proc = root / str(pid)
        proc.mkdir(parents=True)
        (proc / "smaps_rollup").write_text(
            f"55a0-7ffd ---p 00000000 00:00 0 [rollup]\n"
            f"Rss:  {pss * 2} kB\nPss:  {pss} kB\nPss_Anon:  {pss // 2} kB\n"
        )
        (proc / "status").write_text(
            f"Name:\tkitty\nVmRSS:\t{hwm} kB\nVmHWM:\t{hwm} kB\n"
        )
        return root

    def test_pss_is_read_and_pss_anon_is_not_mistaken_for_it(self, tmp_path):
        root = self.fake_proc(tmp_path, 42, pss=1000, hwm=4000)
        sample = field.linux_sample(42, root)
        assert sample.metric == "pss"
        assert sample.kb == 1000
        assert sample.peak_kb == 4000

    def test_a_process_that_cannot_be_read_is_not_a_zero(self, tmp_path):
        # Summing a missing half would undercount by exactly the quantity
        # the chart is about, and nothing on the bar would say so.
        assert field.linux_sample(42, tmp_path) is None
        assert field.memory_sample([]) is None

    def test_a_leg_is_the_sum_of_its_processes(self):
        client = field.MemorySample("pss", 1000, 1500)
        daemon = field.MemorySample("pss", 3000, 3500)
        summed = field.total([client, daemon])
        assert summed.kb == 4000
        assert summed.peak_kb == 5000
        assert field.total([client, None]) is None

    def test_a_peak_only_some_processes_report_is_not_a_total(self):
        pair = [
            field.MemorySample("pss", 1000, 1500),
            field.MemorySample("pss", 10, None),
        ]
        assert field.total(pair).peak_kb is None

    def test_linux_cpu_comes_from_proc_not_whole_second_ps(self, tmp_path):
        # procps prints `cputime` as `00:00:04`; a flood that costs a
        # fast terminal a third of a second charts as zero through it.
        proc = tmp_path / "42"
        proc.mkdir(parents=True)
        # A comm with a space and a `)` in it: the fields are counted
        # from the last `)`, not the first.
        proc.joinpath("stat").write_text(
            "42 (we ird) S " + " ".join(str(n) for n in range(4, 14)) + " 30 70 5 5\n"
        )
        ticks = field.os.sysconf("SC_CLK_TCK")
        assert field.linux_cpu_centiseconds(42, tmp_path) == 100 * 100 // ticks

    def test_a_pid_that_cannot_be_read_is_not_folded_in_as_zero(
        self, tmp_path, monkeypatch
    ):
        # Summing an unreadable pid as 0 would undercount by exactly the
        # quantity the chart is about, so the leg fails the way an
        # unreadable memory sample already does (see `field.total`).
        assert field.linux_cpu_centiseconds(42, tmp_path) is None
        monkeypatch.setattr(field, "DARWIN", False)
        monkeypatch.setattr(field, "linux_cpu_centiseconds", lambda _pid: None)
        assert field.cpu_centiseconds([42]) is None

    def test_macos_is_sampled_through_its_own_ledger(self, monkeypatch):
        # `ps -o rss` on macOS also loses whatever the memory compressor
        # took, so the idle bar would record what else the machine was
        # doing.
        darwin = mock.Mock(return_value=field.MemorySample("x", 1, None))
        monkeypatch.setattr(field, "DARWIN", True)
        monkeypatch.setattr(field, "darwin_sample", darwin)
        field.memory_sample([7])
        darwin.assert_called_once_with(7)

    def test_the_v4_rusage_layout_matches_the_c_struct(self):
        # A field out of place reads a neighbouring counter as the
        # footprint, which is a plausible-looking wrong number.
        assert field.ctypes.sizeof(field.RusageInfoV4) == 296
        assert field.RusageInfoV4.ri_phys_footprint.offset == 72
        assert field.RusageInfoV4.ri_lifetime_max_phys_footprint.offset == 240

    def test_macos_cpu_time_is_converted_from_mach_ticks(self):
        # Apple Silicon counts 125/3 ns per tick; read as nanoseconds the
        # same counter is 42 times too small.
        info = field.RusageInfoV4(ri_user_time=24_000_000, ri_system_time=0)
        assert field.rusage_cpu_seconds(info, 125 / 3) == pytest.approx(1.0)

    def test_macos_cpu_goes_through_rusage_not_ps(self, monkeypatch):
        info = field.RusageInfoV4(ri_user_time=1_200_000, ri_system_time=1_200_000)
        run = mock.Mock()
        monkeypatch.setattr(field, "DARWIN", True)
        monkeypatch.setattr(field, "darwin_rusage", lambda _pid: info)
        monkeypatch.setattr(field, "mach_ns_per_tick", lambda: 125 / 3)
        monkeypatch.setattr(field.subprocess, "run", run)
        assert field.cpu_centiseconds([7, 8]) == 20
        run.assert_not_called()

    def test_macos_reports_no_peak_rather_than_its_startup_peak(self, monkeypatch):
        info = field.RusageInfoV4(
            ri_phys_footprint=50 << 20, ri_lifetime_max_phys_footprint=400 << 20
        )
        monkeypatch.setattr(field, "darwin_rusage", lambda _pid: info)
        sample = field.darwin_sample(7)
        assert sample.kb == 50 << 10
        assert sample.peak_kb is None

    def test_a_child_is_matched_by_its_executable_not_its_install_path(self):
        # macOS prints the whole path; a shell installed under a
        # directory named after the terminal is still the shell.
        assert field.comm_matches("wezterm-gui", "wezterm")
        assert not field.comm_matches("/opt/kitty-tools/bin/zsh", "kitty")
        assert not field.comm_matches("/Applications/kitty.app/bin/bash", "kitty")

    def test_kitty_s_helper_counts_as_kitty(self):
        path = "/Applications/kitty.app/Contents/MacOS/kitten"
        assert field.comm_matches(path, "kitty")
        assert field.comm_matches("kitten", "kitty")
        assert not field.comm_matches("kitten", "ghostty")


class TestUsageSampler:
    def test_cpu_is_the_growth_over_the_window_and_memory_its_samples(self):
        summary = usage.summarize([(10.0, 100), (10.5, 300), (11.25, 200)], 2.0)
        assert summary["cpu_s"] == 1.25
        assert summary["wall_s"] == 2.0
        assert summary["peak_kb"] == 300
        assert summary["mean_kb"] == 200

    def test_felis_is_summed_sample_by_sample_and_kept_apart(self):
        # The client peaks at the first sample and the daemon at the
        # second; the leg never held both peaks at once.
        series = {
            "client": [(1.0, 500), (2.0, 100)],
            "daemon": [(0.0, 100), (0.5, 400)],
        }
        record = usage.record({"client": [1], "daemon": [2]}, series, 1.0)
        assert record["peak_kb"] == 600
        assert record["cpu_s"] == 1.5
        assert record["parts"]["daemon"]["peak_kb"] == 400

    def test_one_part_is_recorded_flat(self):
        record = usage.record({"terminal": [1, 2]}, {"terminal": [(0, 1), (1, 2)]}, 1)
        assert "parts" not in record
        assert record["cpu_s"] == 1

    @staticmethod
    def run_sampler(monkeypatch, results: Path, readings, end_after: int):
        """Samples until the end marker appears after `end_after` readings."""
        calls = []

        def fake_reading(pids):
            calls.append(pids)
            if len(calls) == end_after:
                (results / "kitty.end").touch()
            return readings(len(calls))

        (results / "kitty.start").touch()
        sampler = usage.Sampler(
            results, "kitty", lambda: {"terminal": [7]}, lambda: True, 10
        )
        monkeypatch.setattr(usage, "reading", fake_reading)
        monkeypatch.setattr(usage, "INTERVAL", 0.01)
        sampler.start()
        return sampler.finish(), calls

    def test_the_window_runs_from_start_marker_to_end_marker(
        self, tmp_path, monkeypatch
    ):
        results = tmp_path
        outcome, calls = self.run_sampler(
            monkeypatch,
            results,
            lambda n: (float(min(n, 4)), 100 * min(n, 4)),
            end_after=3,
        )
        assert outcome is None
        record = json.loads((results / "kitty.res.json").read_text())
        # The baseline comes before `.started` releases the workload,
        # and the window runs past `.end` until a reading shows the
        # terminal idle.
        assert (results / "kitty.started").exists()
        assert record["samples"] == 5
        assert record["cpu_s"] == 3.0
        assert record["peak_kb"] == 400

    def test_the_window_stays_open_while_the_terminal_still_works(
        self, tmp_path, monkeypatch
    ):
        # `cat` returns before an eager reader has parsed what it wrote;
        # the CPU it spends after the end marker is the workload's.
        results = tmp_path
        outcome, _calls = self.run_sampler(
            monkeypatch, results, lambda n: (float(min(n, 6)), 1), end_after=2
        )
        assert outcome is None
        record = json.loads((results / "kitty.res.json").read_text())
        assert record["cpu_s"] == 5.0

    def test_the_wrapper_waits_for_the_baseline_before_the_workload(self):
        lines = usage.wrap(Path("/r"), "kitty", ["seq 1 10"])
        assert lines[0] == "touch /r/kitty.start"
        assert "/r/kitty.started" in lines[1]
        assert lines[2:] == ["seq 1 10", "touch /r/kitty.end"]

    def test_a_process_lost_mid_window_leaves_no_record(self, tmp_path, monkeypatch):
        results = tmp_path
        outcome, _calls = self.run_sampler(
            monkeypatch, results, lambda n: None if n == 2 else (1.0, 1), end_after=5
        )
        assert "unreadable" in outcome
        assert not (results / "kitty.res.json").exists()

    def test_a_workload_that_never_starts_is_named(self, tmp_path):
        sampler = usage.Sampler(tmp_path, "kitty", lambda: {}, lambda: False, 10)
        sampler.start()
        assert sampler.finish() == "the workload never started"

    def test_felis_is_charged_for_its_daemon_too(self):
        run = mock.Mock()
        run.daemon_pid.return_value = 99
        leg = field.Leg("felis", mock.Mock(pid=5), Path("x"), None, run)
        assert suites.leg_processes(leg) == {"client": [5], "daemon": [99]}
        run.daemon_pid.return_value = None
        with pytest.raises(RuntimeError):
            suites.leg_processes(leg)

    def test_a_variant_is_sampled_under_its_real_process_name(self, monkeypatch):
        leg = field.Leg("ghostty-tip", mock.Mock(pid=5), Path("x"), None, None)
        children = mock.Mock(return_value=[5, 6])
        monkeypatch.setattr(suites.fieldmod, "child_pids", children)
        assert suites.leg_processes(leg) == {"terminal": [5, 6]}
        children.assert_called_once_with(5, "ghostty")


class TestUsageLoader:
    def test_each_sampled_suite_is_a_panel_in_both_charts(self, tmp_path):
        for suite, cpu in (("vtebench", 2.0), ("cat", 1.0)):
            (tmp_path / suite).mkdir()
            (tmp_path / suite / "kitty.res.json").write_text(
                json.dumps(
                    {"metric": "pss", "cpu_s": cpu, "wall_s": 4.0, "peak_kb": 2048}
                )
            )
        cpu, memory = ld.load_usage(tmp_path)
        assert cpu.categories == ["vtebench", "cat"]
        assert cpu.data["kitty"]["vtebench"].value == pytest.approx(2.0)
        assert memory.data["kitty"]["cat"].value == pytest.approx(2.0)
        assert "vtebench: kitty 50%" in " ".join(cpu.notes)

    def test_the_notes_say_what_the_cpu_number_leaves_out(self, tmp_path):
        (tmp_path / "cat").mkdir()
        (tmp_path / "cat" / "felis.res.json").write_text(
            json.dumps(
                {
                    "metric": "pss",
                    "cpu_s": 3.0,
                    "wall_s": 2.0,
                    "peak_kb": 1024,
                    "parts": {
                        "client": {"cpu_s": 1.0, "peak_kb": 512},
                        "daemon": {"cpu_s": 2.0, "peak_kb": 512},
                    },
                }
            )
        )
        cpu, memory = ld.load_usage(tmp_path)
        assert "client and its daemon summed" in cpu.subtitle
        assert "GPU driver" in cpu.subtitle
        assert "client 1.00 s" in " ".join(cpu.notes)
        assert "Not a kernel high-water mark" in memory.subtitle

    def test_a_root_with_no_records_has_no_resource_charts(self, tmp_path):
        (tmp_path / "vtebench").mkdir()
        assert ld.load_usage(tmp_path) == []


class TestLatencyLoader:
    @staticmethod
    def write(directory: Path, term: str, samples: list[float]) -> None:
        (directory / f"{term}.latency.json").write_text(
            json.dumps({"count": len(samples), "delay_ms": 150, "samples_ms": samples})
        )

    def test_mean_carries_the_spread_as_its_whisker(self, tmp_path):
        self.write(tmp_path, "felis", [10.0, 20.0, 30.0])
        (suite,) = ld.load_latency(tmp_path)
        point = suite.data["felis"]["mean"]
        assert point.value == pytest.approx(20.0)
        assert point.err == pytest.approx(8.16496580927726)

    def test_the_tail_is_an_observed_sample_not_an_interpolation(self, tmp_path):
        # 100 samples: nearest-rank p95 is the 95th in order, 95.0.
        self.write(tmp_path, "kitty", [float(i) for i in range(1, 101)])
        (suite,) = ld.load_latency(tmp_path)
        assert suite.data["kitty"]["95th percentile"].value == pytest.approx(95.0)

    def test_the_terminal_name_survives_the_double_suffix(self, tmp_path):
        # `Path.stem` would leave "ghostty-tip.latency" here.
        self.write(tmp_path, "ghostty-tip", [12.0])
        (suite,) = ld.load_latency(tmp_path)
        assert list(suite.data) == ["ghostty-tip"]

    def test_a_retried_leg_is_named_with_its_reason(self, tmp_path):
        self.write(tmp_path, "alacritty", [10.0, 20.0])
        (tmp_path / "alacritty.done").touch()
        (tmp_path / "alacritty.latency-failed").write_text("the line would not clear\n")
        (tmp_path / "ghostty-tip.latency-failed").write_text("no row\nno row\n")
        (suite,) = ld.load_latency(tmp_path)
        note = " ".join(suite.notes)
        assert (
            "alacritty (measured on a later attempt): the line would not clear" in note
        )
        assert "ghostty-tip (not measured): no row; no row" in note

    def test_cells_the_terminal_repainted_on_its_own_are_counted(self, tmp_path):
        (tmp_path / "ghostty-tip.latency.json").write_text(
            json.dumps({"samples_ms": [10.0], "covered_before_key": 3})
        )
        (suite,) = ld.load_latency(tmp_path)
        assert "ghostty-tip 3" in " ".join(suite.notes)

    def test_a_leg_that_produced_no_samples_is_not_a_bar(self, tmp_path):
        self.write(tmp_path, "wezterm", [])
        assert ld.load_latency(tmp_path) == []


class TestLatencyRetry:
    @staticmethod
    def attempts(results: Path, outcomes: list[bool | str]) -> tuple[bool, int]:
        """Drive `with_retry`; a string outcome is an instrument failure."""
        calls = []

        def attempt():
            outcome = outcomes[len(calls)]
            calls.append(outcome)
            if isinstance(outcome, str):
                suites.record_latency_failure(results, "kitty", outcome)
                return False
            return outcome

        with contextlib.redirect_stderr(io.StringIO()):
            ok = suites.with_retry(results, "kitty", attempt)
        return ok, len(calls)

    def test_an_instrument_failure_gets_one_more_window(self, tmp_path):
        assert self.attempts(tmp_path, ["line would not clear", True]) == (True, 2)

    def test_a_terminal_that_fails_twice_stays_failed(self, tmp_path):
        assert self.attempts(tmp_path, ["no pattern", "no pattern"]) == (False, 2)

    def test_a_skipped_leg_is_not_retried(self, tmp_path):
        # No focus or no quiet desktop is not a transient of the
        # measurement, and a second window would be skipped the same way.
        assert self.attempts(tmp_path, [False]) == (False, 1)

    def test_the_reason_is_the_instrument_s_last_line(self):
        assert (
            suites.last_line("  wl-latency: inserting...\nwl-latency: no row\n\n")
            == "wl-latency: no row"
        )


class TestFrontmostWindow:
    """Refusing to type into the wrong window is the whole guard."""

    def test_the_pid_is_read_out_of_lsappinfo(self, monkeypatch):
        run = mock.Mock(
            side_effect=[
                mock.Mock(stdout="ASN:0x0-0x51051:\n"),
                mock.Mock(stdout='"pid"=2012\n'),
            ]
        )
        monkeypatch.setattr(field.subprocess, "run", run)
        assert field.frontmost_pid() == 2012

    def test_no_frontmost_application_is_not_a_pid(self, monkeypatch):
        run = mock.Mock(return_value=mock.Mock(stdout="\n"))
        monkeypatch.setattr(field.subprocess, "run", run)
        assert field.frontmost_pid() is None

    def test_a_locked_screen_is_named_rather_than_guessed_at(self, monkeypatch):
        # It fails every leg the same way and the fix is a password, not
        # a re-run, so the suite has to say so.
        run = mock.Mock(
            return_value=mock.Mock(
                stdout="/System/Library/CoreServices/loginwindow.app/"
                "Contents/MacOS/loginwindow\n"
            )
        )
        monkeypatch.setattr(field, "frontmost_pid", lambda: 442)
        monkeypatch.setattr(field.subprocess, "run", run)
        assert field.screen_locked()

    def test_the_tree_reaches_a_gui_process_the_cli_spawned(self, monkeypatch):
        # `wezterm start` is the launched pid; wezterm-gui owns the
        # window, and matching only the launch pid would read as "not
        # frontmost" for a window that is.
        children = {"10": "20\n", "20": "30\n"}
        monkeypatch.setattr(
            field.subprocess,
            "run",
            lambda argv, **_: mock.Mock(stdout=children.get(argv[2], "")),
        )
        assert field.process_tree(10) == {10, 20, 30}


class TestPlatformEscapes:
    """The two AppKit detours every leg and every run go through."""

    def test_off_macos_the_raise_is_already_done(self, monkeypatch):
        run = mock.Mock()
        monkeypatch.setattr(field, "DARWIN", False)
        monkeypatch.setattr(field.subprocess, "run", run)
        assert field.raise_window(4242)
        run.assert_not_called()

    def test_linux_holds_the_display_through_logind(self, monkeypatch):
        popen = mock.Mock()
        monkeypatch.setattr(field, "DARWIN", False)
        monkeypatch.setattr(field.shutil, "which", lambda _: "/bin/inhibit")
        monkeypatch.setattr(field.subprocess, "Popen", popen)
        field.keep_display_awake()
        argv = popen.call_args.args[0]
        assert argv[0] == "systemd-inhibit"
        assert "--what=idle:sleep" in argv

    def test_no_logind_leaves_the_run_without_one(self, monkeypatch):
        monkeypatch.setattr(field, "DARWIN", False)
        monkeypatch.setattr(field.shutil, "which", lambda _: None)
        assert field.keep_display_awake() is None


class StubPin:
    """A pin that answers at once, so a test is not a 20-second wait."""

    name = "stub"
    can_resize = True

    def __init__(self, status: str = wm.PINNED, detail: str = "") -> None:
        self.status = status
        self.detail = detail
        self.targets: list[wm.Target] = []

    @classmethod
    def detect(cls):
        return cls()

    def prepare(self):
        return None

    def pin(self, _launch_pid, target):
        self.targets.append(target)
        return wm.PinResult(self.status, field.Size(40, 120, 960, 640), self.detail)

    def release(self):
        return None


def attempt_dir(results: Path) -> Path:
    """The one attempt directory a leg under test is writing into."""
    (found,) = [
        p
        for p in (results / field.ATTEMPTS_DIR).iterdir()
        if not p.name.endswith(field.PROMOTING) and not p.name.startswith(".")
    ]
    return found


def stub_field(pin=None, **pres) -> field.Field:
    return field.Field(
        binaries={"kitty": "/usr/bin/true"},
        felis_bin="/usr/bin/true",
        pres=field.Presentation(rows=40, cols=120, **pres),
        pin=pin or StubPin(),
    )


@pytest.mark.usefixtures("presenting")
class TestDrivenLeg:
    """The `drive` hook the latency suite steers its window through."""

    @staticmethod
    def run_leg(monkeypatch, results: Path, drive, opens_marker: bool, fld=None):
        def launch(_launch, _wrapper):
            if opens_marker:
                (attempt_dir(results) / "kitty.ready").touch()
            return mock.Mock(pid=4242, poll=lambda: 0)

        monkeypatch.setattr(suites.fieldmod, "raise_window", lambda _pid: True)
        monkeypatch.setattr(suites.fieldmod, "launch_terminal", launch)
        monkeypatch.setattr(
            suites.fieldmod, "wait_for", lambda path, _t, _a=None: path.exists()
        )
        quiet = io.StringIO()
        with contextlib.redirect_stdout(quiet), contextlib.redirect_stderr(quiet):
            return suites.run_leg(
                fld or stub_field(),
                "kitty",
                results,
                lambda _n, _o: [],
                1,
                drive,
                "ready",
            )

    def test_the_driver_is_handed_the_launch_pid(self, tmp_path, monkeypatch):
        seen = []
        ok = self.run_leg(
            monkeypatch,
            tmp_path,
            lambda name, pid, _o: seen.append((name, pid)) or True,
            True,
        )
        assert ok
        assert seen == [("kitty", 4242)]

    def test_a_marker_from_a_failed_attempt_does_not_stand_in(
        self, tmp_path, monkeypatch
    ):
        # Left behind by a previous run, it would otherwise be taken for
        # this window's — and satisfied before the window even opened,
        # so the driver would type into whatever was in front.
        results = tmp_path
        (results / "kitty.ready").touch()
        seen = []
        ok = self.run_leg(
            monkeypatch,
            results,
            lambda name, _pid, _o: seen.append(name) or True,
            False,
        )
        assert not ok
        assert seen == []

    def test_the_window_is_raised_before_it_is_pinned(self, tmp_path, monkeypatch):
        # ghostty opens no window at all until its application is
        # activated, so a pin that came first would have nothing to
        # place and a wait would sit out its whole timeout.
        order = []
        quiet = io.StringIO()

        class Ordered(StubPin):
            def pin(self, launch_pid, target):
                order.append("pin")
                return super().pin(launch_pid, target)

        monkeypatch.setattr(
            suites.fieldmod,
            "raise_window",
            lambda _pid: order.append("raise") or True,
        )
        monkeypatch.setattr(
            suites.fieldmod,
            "launch_terminal",
            lambda *_a: mock.Mock(pid=1, poll=lambda: 0),
        )
        monkeypatch.setattr(
            suites.fieldmod,
            "wait_for",
            lambda *_a, **_k: order.append("wait") or True,
        )
        with contextlib.redirect_stdout(quiet), contextlib.redirect_stderr(quiet):
            suites.run_leg(
                stub_field(pin=Ordered()),
                "kitty",
                tmp_path,
                lambda _n, _o: [],
                1,
                lambda _n, _p, _o: True,
                "ready",
            )
        assert order == ["raise", "pin", "wait"]

    def test_an_undriven_suite_is_raised_too(self, tmp_path, monkeypatch):
        # Not only the suite that types: ghostty's window-on-activation
        # behavior costs an undriven leg its whole timeout just the
        # same, which reads on the chart as a terminal that was never
        # measured.
        raise_window = mock.Mock()
        monkeypatch.setattr(suites.fieldmod, "raise_window", raise_window)
        monkeypatch.setattr(
            suites.fieldmod,
            "launch_terminal",
            lambda *_a: mock.Mock(pid=1, poll=lambda: 0),
        )
        monkeypatch.setattr(suites.fieldmod, "wait_for", lambda *_a, **_k: True)
        quiet = io.StringIO()
        with contextlib.redirect_stdout(quiet), contextlib.redirect_stderr(quiet):
            suites.run_leg(stub_field(), "kitty", tmp_path, lambda _n, _o: [], 1)
        raise_window.assert_called_once_with(1)

    def test_a_refused_measurement_leaves_the_leg_to_be_retried(
        self, tmp_path, monkeypatch
    ):
        results = tmp_path
        assert not self.run_leg(monkeypatch, results, lambda _n, _p, _o: False, True)
        # No `.done`: the next run launches this terminal again
        # rather than reporting it measured.
        assert not (results / "kitty.done").exists()

    def test_a_leg_the_pin_could_not_place_is_recorded_before_it_runs(
        self, tmp_path, monkeypatch
    ):
        # After the stty fallback the cell grid matches, so a check on
        # the final `.size` alone would call this leg pinned.
        fld = stub_field(pin=StubPin(wm.TTY_ONLY, "niri left it at 100x30"))
        self.run_leg(monkeypatch, tmp_path, lambda _n, _p, _o: True, True, fld)
        assert fld.mismatches == [
            "kitty: tty pinned, window unpinned (niri left it at 100x30)"
        ]

    def test_an_unpinned_leg_is_on_disk_before_its_workload_runs(
        self, tmp_path, monkeypatch
    ):
        # A run killed mid-suite would otherwise leave `.size` files
        # that agree in cells and nothing saying the window behind one
        # of them was never placed.
        recorded = []

        def seen(results: Path) -> str:
            path = results / "grid-mismatch"
            return path.read_text() if path.exists() else ""

        results = tmp_path
        fld = stub_field(pin=StubPin(wm.TTY_ONLY, "niri left it at 100x30"))

        def peek(_launch, _wrapper):
            recorded.append(seen(results))
            (attempt_dir(results) / "kitty.ready").touch()
            return mock.Mock(pid=4242, poll=lambda: 0)

        monkeypatch.setattr(suites.fieldmod, "raise_window", lambda _pid: True)
        monkeypatch.setattr(suites.fieldmod, "launch_terminal", peek)
        monkeypatch.setattr(
            suites.fieldmod,
            "wait_for",
            lambda path, _t, _a=None: recorded.append(seen(results)) or path.exists(),
        )
        quiet = io.StringIO()
        with contextlib.redirect_stdout(quiet), contextlib.redirect_stderr(quiet):
            suites.run_leg(fld, "kitty", results, lambda _n, _o: [], 1, marker="ready")
        # The wait is the workload's start: the note is already there.
        assert "tty pinned, window unpinned" in recorded[-1]

    def test_a_note_is_not_written_twice(self, tmp_path):
        results = tmp_path
        field.write_mismatch(results, "kitty: unpinned (no window)")
        field.write_mismatch(results, "kitty: unpinned (no window)")
        assert (
            results / "grid-mismatch"
        ).read_text() == "kitty: unpinned (no window)\n"

    def test_the_attempt_local_files_are_cleared_before_the_launch(
        self, tmp_path, monkeypatch
    ):
        # A `.pinned` from a failed attempt releases the next window
        # before it has been placed.
        results = tmp_path
        for suffix in ("pinned", "size", "stty", "ready"):
            (results / f"kitty.{suffix}").write_text("stale\n")
        self.run_leg(monkeypatch, results, lambda _n, _p, _o: True, True)
        assert not (results / "kitty.stty").exists()
        assert not (results / "kitty.size").exists()

    def test_a_leg_whose_display_cannot_present_is_never_launched(
        self, tmp_path, monkeypatch
    ):
        results = tmp_path
        locked = {"locked": True, "asleep": False}
        launch = mock.Mock()
        monkeypatch.setattr(envinfo, "display_state", lambda: locked)
        monkeypatch.setattr(suites.fieldmod, "launch_terminal", launch)
        with contextlib.redirect_stderr(io.StringIO()) as err:
            ok = suites.run_leg(stub_field(), "kitty", results, lambda _n, _o: [], 1)
        assert not ok
        launch.assert_not_called()
        assert "REFUSED: kitty" in err.getvalue()
        record = json.loads((results / "kitty.env.json").read_text())
        assert record["refused"] == "the screen was locked before the leg"
        assert not (results / "kitty.done").exists()

    def test_a_refused_leg_leaves_no_wrapper_script_behind(self, tmp_path, monkeypatch):
        locked = {"locked": True, "asleep": False}
        wrapper = mock.Mock()
        monkeypatch.setattr(envinfo, "display_state", lambda: locked)
        monkeypatch.setattr(suites.fieldmod, "wrapper_script", wrapper)
        with contextlib.redirect_stderr(io.StringIO()):
            suites.run_leg(stub_field(), "kitty", tmp_path, lambda _n, _o: [], 1)
        wrapper.assert_not_called()

    def test_a_leg_that_ended_unable_to_present_lands_in_refused(
        self, tmp_path, monkeypatch
    ):
        states = iter([PRESENTING, {"locked": False, "asleep": True}, PRESENTING])
        results = tmp_path

        def drive(name, _pid, out):
            (out / f"{name}.done").touch()
            (out / f"{name}.latency.json").write_text("{}")
            return True

        monkeypatch.setattr(envinfo, "display_state", lambda: next(states))
        ok = self.run_leg(monkeypatch, results, drive, True)
        assert not ok
        # Out of the loaders' reach and the resume marker with them.
        assert not (results / "kitty.done").exists()
        assert not (results / "kitty.latency.json").exists()
        assert (results / "refused" / "kitty.latency.json").exists()
        assert (results / "refused" / "kitty.done").exists()
        record = json.loads((results / "kitty.env.json").read_text())
        assert record["refused"] == "the display was asleep when the leg ended"
        assert "after" in record

    def test_a_write_the_stop_did_not_prevent_is_not_charted(
        self, tmp_path, monkeypatch
    ):
        results = tmp_path

        def drive(name, _pid, out):
            (out / f"{name}.latency.json").write_text("measured")
            (out / f"{name}.done").touch()
            return True

        def escaped(_roots):
            (attempt_dir(results) / "kitty.latency.json").write_text("late")
            return [4243]

        monkeypatch.setattr(suites.fieldmod, "stop", escaped)
        assert self.run_leg(monkeypatch, results, drive, True)
        assert (results / "kitty.latency.json").read_text() == "measured"

    def test_a_failed_measurement_s_number_is_not_charted(self, tmp_path, monkeypatch):
        # An instrument can write its JSON and still exit nonzero.
        results = tmp_path

        def drive(name, _pid, out):
            (out / f"{name}.latency.json").write_text('{"samples_ms": [1]}')
            suites.record_latency_failure(results, name, "exited 1")
            return False

        assert not self.run_leg(monkeypatch, results, drive, True)
        assert not (results / "kitty.latency.json").exists()
        assert (results / "unfinished" / "kitty.latency.json").exists()
        assert (results / "kitty.env.json").exists()
        assert suites.latency_failures(results, "kitty") == ["exited 1"]

    def test_a_driver_that_raises_leaves_nothing_for_the_report(
        self, tmp_path, monkeypatch
    ):
        results = tmp_path

        def drive(name, _pid, out):
            (out / f"{name}.latency.json").write_text('{"samples_ms": [1]}')
            raise RuntimeError("the instrument vanished")

        with pytest.raises(RuntimeError):
            self.run_leg(monkeypatch, results, drive, True)
        assert sorted(p.name for p in results.iterdir()) == [".attempts"]
        assert list((results / ".attempts").iterdir()) == []


class TestLegAttempt:
    """What reaches the suite directory from one try at a leg, and when."""

    def test_a_completed_attempt_lands_with_its_resume_marker(self, tmp_path):
        results = tmp_path
        with field.attempt(results, "kitty") as tried:
            tried.path("json").write_text("{}")
            tried.path("done").touch()
            assert not (results / "kitty.json").exists()
        assert (results / "kitty.json").exists()
        assert (results / "kitty.done").exists()
        assert list((results / field.ATTEMPTS_DIR).iterdir()) == []

    def test_an_attempt_that_raised_leaves_the_suite_directory_untouched(
        self, tmp_path
    ):
        results = tmp_path
        with pytest.raises(KeyboardInterrupt):
            with field.attempt(results, "kitty") as tried:
                tried.path("json").write_text("{}")
                raise KeyboardInterrupt
        assert not (results / "kitty.json").exists()
        assert list((results / field.ATTEMPTS_DIR).iterdir()) == []

    def test_a_retry_that_raises_does_not_leave_the_earlier_row_charted(self, tmp_path):
        # An interrupted run from before attempts existed could leave a
        # row with no resume marker; the retry must not publish it even
        # when it fails before writing its own.
        results = tmp_path
        (results / "felis-warm.startup.json").write_text(
            json.dumps({"samples_ms": [5.0, 6.0]})
        )
        (results / "felis.env.json").write_text("{}")
        with pytest.raises(OSError):
            with field.attempt(results, "felis"):
                raise OSError("the priming launch could not be spawned")
        assert ld.collect(results, "startup") == []
        assert not (results / "felis.env.json").exists()

    def test_a_write_after_promotion_does_not_reach_the_promoted_copy(self, tmp_path):
        # A workload that outlived its window still holds the file it
        # was redirected into.
        results = tmp_path
        with field.attempt(results, "kitty") as tried:
            late = tried.path("doom-fire").open("w")
            late.write("fps 60\n")
            late.flush()
            tried.path("done").touch()
        with late:
            late.write("fps 1\n")
        assert (results / "kitty.doom-fire").read_text() == "fps 60\n"

    def test_a_write_after_the_leg_is_sealed_is_not_promoted(self, tmp_path):
        # A process the stop did not reach can write between the stop
        # and the promotion.
        results = tmp_path
        with field.attempt(results, "kitty") as tried:
            out = tried.dir
            (out / "kitty.dat").write_text("measured")
            (out / "kitty.done").touch()
            tried.seal()
            (out / "kitty.dat").write_text("late")
            (out / "kitty.res.json").write_text("{}")
            tried.path("env.json").write_text("{}")
        assert (results / "kitty.dat").read_text() == "measured"
        assert not (results / "kitty.res.json").exists()
        assert (results / "kitty.env.json").exists()

    def test_a_marker_touched_after_promotion_lands_nowhere(self, tmp_path):
        results = tmp_path
        with field.attempt(results, "kitty") as tried:
            marker = tried.path("done")
        with pytest.raises(FileNotFoundError):
            marker.touch()
        assert not (results / "kitty.done").exists()

    def test_the_latency_failure_log_outlives_a_retry(self, tmp_path):
        results = tmp_path
        suites.record_latency_failure(results, "kitty", "first window")
        (results / "kitty.size").write_text("40 120 0 0\n")
        with field.attempt(results, "kitty") as tried:
            tried.path("done").touch()
        assert suites.latency_failures(results, "kitty") == ["first window"]
        assert not (results / "kitty.size").exists()

    def test_felis_s_attempt_owns_both_startup_rows(self, tmp_path):
        results = tmp_path
        for stale in ("felis-cold", "felis-warm", "felis"):
            (results / f"{stale}.startup.json").write_text("{}")
        (results / "ghostty-tip.startup.json").write_text("{}")
        with field.attempt(results, "felis"):
            pass
        assert sorted(p.name for p in results.glob("*.json")) == [
            "ghostty-tip.startup.json"
        ]

    def test_a_promotion_cut_short_is_finished_before_the_suite_resumes(self, tmp_path):
        # Committed means every file was staged; finishing it moves the
        # rest, the resume marker last.
        results = tmp_path
        staged = results / field.ATTEMPTS_DIR / f"kitty{field.PROMOTING}"
        staged.mkdir(parents=True)
        (staged / "kitty.done").touch()
        (staged / "kitty.dat").write_text("data")
        (results / "kitty.res.json").write_text("{}")
        abandoned = results / field.ATTEMPTS_DIR / "kitty.abcd1234"
        abandoned.mkdir()
        (abandoned / "kitty.dat").write_text("stale")
        field.clear_attempts(results)
        assert (results / "kitty.dat").read_text() == "data"
        assert (results / "kitty.done").exists()
        assert (results / "kitty.res.json").exists()
        assert not (results / field.ATTEMPTS_DIR).exists()

    def test_a_refused_promotion_cut_short_still_lands_in_refused(self, tmp_path):
        results = tmp_path
        staged = results / field.ATTEMPTS_DIR / f"kitty{field.PROMOTING}"
        staged.mkdir(parents=True)
        (staged / field.ASIDE_MARK).write_text(field.REFUSED_DIR)
        (staged / "kitty.json").write_text("{}")
        (staged / "kitty.done").touch()
        (results / "kitty.env.json").write_text(json.dumps({"refused": "locked"}))
        field.clear_attempts(results)
        assert not (results / "kitty.done").exists()
        assert not (results / "kitty.json").exists()
        assert (results / field.REFUSED_DIR / "kitty.done").exists()

    def test_the_report_finishes_a_committed_promotion(self, tmp_path):
        suite = tmp_path / "startup"
        staged = suite / field.ATTEMPTS_DIR / f"kitty{field.PROMOTING}"
        staged.mkdir(parents=True)
        (staged / "kitty.startup.json").write_text(
            json.dumps({"samples_ms": [5.0, 6.0]})
        )
        (suite / field.ATTEMPTS_DIR / "wezterm.x").mkdir()
        (suite / field.ATTEMPTS_DIR / "wezterm.x" / "wezterm.startup.json").write_text(
            json.dumps({"samples_ms": [5.0, 6.0]})
        )
        (startup,) = [s for s in ld.collect(tmp_path, None) if s.key == "startup"]
        assert "kitty" in startup.data
        assert "wezterm" not in startup.data

    def test_an_attempt_that_returned_unfinished_lands_only_its_env_record(
        self, tmp_path
    ):
        results = tmp_path
        with field.attempt(results, "kitty") as tried:
            tried.path("dat").write_text("half a run")
            tried.path("env.json").write_text("{}")
        assert sorted(p.name for p in results.iterdir() if p.is_file()) == [
            "kitty.env.json"
        ]
        assert (results / field.UNFINISHED_DIR / "kitty.dat").exists()

    def test_the_memory_suite_finishes_on_its_record(self, tmp_path):
        results = tmp_path
        with field.attempt(results, "kitty", "mem.json") as tried:
            tried.path("mem.json").write_text("{}")
        assert (results / "kitty.mem.json").exists()

    def test_a_refused_attempt_lands_only_its_reason(self, tmp_path):
        results = tmp_path
        with field.attempt(results, "kitty") as tried:
            tried.path("json").write_text("{}")
            tried.path("done").touch()
            tried.path("env.json").write_text(json.dumps({"refused": "locked"}))
        assert sorted(p.name for p in results.iterdir() if p.is_file()) == [
            "kitty.env.json"
        ]
        assert sorted(p.name for p in (results / field.REFUSED_DIR).iterdir()) == [
            "kitty.done",
            "kitty.json",
        ]


@pytest.mark.skipif(sys.platform == "darwin", reason="pidfds are Linux's")
class TestStop:
    """Stopping a leg's processes by identity, on real processes."""

    @pytest.fixture
    def spawn(self):
        spawned = []

        def spawn(script: str) -> subprocess.Popen:
            proc = subprocess.Popen(["bash", "-c", script])
            spawned.append(proc)
            return proc

        yield spawn
        for proc in reversed(spawned):
            with contextlib.suppress(ProcessLookupError):
                proc.kill()
            proc.wait()

    @staticmethod
    def gone(pid: int) -> bool:
        def exited() -> bool:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return True
            stat = Path(f"/proc/{pid}/stat").read_text()
            return stat[stat.rfind(")") + 2] == "Z"

        return wait_until(exited, 5)

    def test_a_process_that_ignores_sigterm_is_killed(self, tmp_path, spawn):
        child = tmp_path / "child"
        proc = spawn(f"trap '' TERM; sleep 30 & echo $! > {child}; wait")
        assert wait_until(lambda: child.exists(), 5)
        lingering = field.stop([field.Held.child(proc)], grace=0.3)
        assert lingering == []
        assert self.gone(int(child.read_text()))

    def test_a_child_forked_after_the_first_walk_is_stopped_too(self, tmp_path, spawn):
        child, ready = tmp_path / "child", tmp_path / "ready"
        proc = spawn(
            f"trap '' TERM; touch {ready}; sleep 0.5; "
            f"sleep 30 & echo $! > {child}; wait"
        )
        assert wait_until(ready.exists, 5)
        lingering = field.stop([field.Held.child(proc)], grace=1.5)
        assert lingering == []
        assert child.exists()
        assert self.gone(int(child.read_text()))

    def test_a_process_that_does_not_belong_is_not_held(self, spawn):
        proc = spawn("sleep 30")
        assert field.hold(proc.pid, lambda: False) is None

    def test_a_signal_after_the_process_is_gone_reaches_no_pid(
        self, spawn, monkeypatch, request
    ):
        proc = spawn("sleep 30")
        held = field.hold(proc.pid)
        request.addfinalizer(held.close)
        proc.kill()
        proc.wait()
        kill = mock.Mock()
        monkeypatch.setattr(field.os, "kill", kill)
        held.send(signal.SIGKILL)
        kill.assert_not_called()
        assert not held.alive()


class TestDarwinHold:
    """macOS has no pidfd, so a process is held by its start time."""

    @pytest.fixture(autouse=True)
    def darwin(self, monkeypatch):
        monkeypatch.setattr(field, "DARWIN", True)

    def test_a_pid_that_restarted_while_being_held_is_refused(self, monkeypatch):
        starts = iter([100, 200])
        monkeypatch.setattr(field, "start_time", lambda _pid: next(starts))
        assert field.hold(4242) is None

    def test_a_reused_pid_is_not_signalled(self, monkeypatch):
        monkeypatch.setattr(field, "start_time", lambda _pid: 100)
        held = field.hold(4242)
        kill = mock.Mock()
        monkeypatch.setattr(field, "start_time", lambda _pid: 300)
        monkeypatch.setattr(field.os, "kill", kill)
        held.send(signal.SIGKILL)
        kill.assert_not_called()

    def test_the_process_still_running_is_signalled(self, monkeypatch):
        kill = mock.Mock()
        monkeypatch.setattr(field, "start_time", lambda _pid: 100)
        monkeypatch.setattr(field.os, "kill", kill)
        field.hold(4242).send(signal.SIGTERM)
        kill.assert_called_once_with(4242, signal.SIGTERM)


@pytest.mark.usefixtures("presenting")
class TestMemoryLeg:
    """The suite that samples its own legs, now through the shared leg site."""

    @pytest.fixture(autouse=True)
    def raise_window(self, monkeypatch):
        raise_window = mock.Mock()
        monkeypatch.setattr(suites.fieldmod, "raise_window", raise_window)
        monkeypatch.setattr(
            suites.fieldmod,
            "launch_terminal",
            lambda *_a: mock.Mock(pid=4242, poll=lambda: 0),
        )
        monkeypatch.setattr(suites.fieldmod, "wait_for", lambda *_a, **_k: False)
        return raise_window

    @staticmethod
    def run_suite(results: Path, err: io.StringIO) -> None:
        # Stand in for felis's own branch: this test is about the loop
        # over the rest of the field.
        (results / "felis.mem.json").write_text("{}")
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(err),
        ):
            suites.suite_memory(stub_field(), results, {})

    def test_its_legs_are_raised_like_every_other_suite(self, tmp_path, raise_window):
        # Sampling its own legs is not a reason to be the one suite
        # ghostty cannot appear in.
        self.run_suite(tmp_path, io.StringIO())
        raise_window.assert_called_once_with(4242)

    def test_a_leg_that_never_settles_says_so(self, tmp_path):
        # It used to return quietly, which on the chart is indistinct
        # from a terminal that was measured and had nothing to report.
        err = io.StringIO()
        self.run_suite(tmp_path, err)
        assert "kitty" in err.getvalue()

    def test_its_phase_markers_are_cleared_before_the_launch(self, tmp_path):
        # A stale `.idle` lets the sampler take its first reading before
        # the window exists and chart pre-flood numbers as flooded.
        results = tmp_path
        (results / "kitty.idle").write_text("stale\n")
        (results / "kitty.flooded").write_text("stale\n")
        self.run_suite(results, io.StringIO())
        assert not (results / "kitty.idle").exists()
        assert not (results / "kitty.flooded").exists()


# The forked child only execs, so it takes no lock a sibling thread could hold.
@pytest.mark.filterwarnings(
    "ignore:.*use of forkpty\\(\\) may lead to deadlocks:DeprecationWarning"
)
@pytest.mark.usefixtures("presenting")
class TestWrapperHandshake:
    """The wrapper's half of the pin, run for real under a PTY.

    The one asynchronous invariant in the pin: the workload must not
    start until the driver says the window is placed, and a tty pin left
    behind must reach the tty before that. No parsing test can show
    either, so this one forks a PTY, runs the generated wrapper in it
    and watches the order the files appear in.
    """

    GRID = (40, 120, 960, 640)

    @staticmethod
    def workload(name: str, out: Path) -> list[str]:
        """A stub workload that records the grid it started under."""
        return [
            f"cp {shlex.quote(str(out / f'{name}.size'))} "
            f"{shlex.quote(str(out / f'{name}.workload'))}",
            f"touch {shlex.quote(str(out / f'{name}.done'))}",
        ]

    @pytest.fixture
    def run_leg(self, monkeypatch):
        """Run the leg's wrapper inside a PTY of a known size."""
        forked = []

        def launch(_launch, wrapper):
            pid, master = pty.fork()
            if pid == 0:  # pragma: no cover - replaced by execv
                os.execv(str(wrapper), [str(wrapper)])
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", *self.GRID))
            forked.append((pid, master))
            return ForkedChild(pid)

        wrapper_script = suites.fieldmod.wrapper_script
        # The settle waits out a real window's first resize; the PTY
        # here takes its size before the wrapper's shell has started.
        monkeypatch.setattr(
            suites.fieldmod,
            "wrapper_script",
            lambda body, settle: wrapper_script(body, settle=0.1),
        )
        monkeypatch.setattr(suites.fieldmod, "raise_window", lambda _pid: True)
        monkeypatch.setattr(suites.fieldmod, "launch_terminal", launch)
        monkeypatch.setattr(
            suites.fieldmod,
            "wait_for",
            lambda path, timeout, _a=None: wait_until(path.exists, timeout),
        )

        def run(results: Path, pin) -> None:
            quiet = io.StringIO()
            with contextlib.redirect_stdout(quiet), contextlib.redirect_stderr(quiet):
                suites.run_leg(
                    stub_field(pin=pin, xpixel=960, ypixel=640),
                    "kitty",
                    results,
                    self.workload,
                    60,
                    marker="done",
                )

        yield run
        for pid, master in reversed(forked):
            with contextlib.suppress(OSError):
                os.kill(pid, signal.SIGKILL)
            with contextlib.suppress(OSError):
                os.waitpid(pid, 0)
            os.close(master)

    def test_the_workload_waits_for_the_pin(self, tmp_path, run_leg):
        class Watching(StubPin):
            def pin(self, launch_pid, target):
                # The size the pin is placing against has to be
                # readable, and the workload must not have run yet.
                self.size = wait_until_size(target.size)
                self.ran_early = target.size.with_suffix(".workload").exists()
                return super().pin(launch_pid, target)

        results = tmp_path
        pin = Watching()
        run_leg(results, pin)
        assert pin.size == field.Size(*self.GRID)
        assert not pin.ran_early
        assert field.read_size(results / "kitty.workload") == field.Size(*self.GRID)

    def test_a_tty_pin_reaches_the_tty_before_the_workload(self, tmp_path, run_leg):
        class Falling(StubPin):
            def pin(self, launch_pid, target):
                wait_until_size(target.size)
                target.stty.write_text("30 100\n")
                return wm.PinResult(wm.TTY_ONLY, None, "the window would not move")

        results = tmp_path
        run_leg(results, Falling())
        # What the workload saw, not merely what the file ended at:
        # a pin applied afterwards would leave the same `.size` and
        # a payload shaped by the wrong grid.
        seen = field.read_size(results / "kitty.workload")
        assert seen.cells() == (30, 100)

    def test_a_stale_pinned_file_does_not_release_the_next_window(
        self, tmp_path, run_leg
    ):
        # The retry case: the previous attempt left its markers behind,
        # and a wrapper that saw them would run the workload against a
        # window nobody had placed yet.
        class Watching(StubPin):
            def pin(self, launch_pid, target):
                self.ran_early = target.size.with_suffix(".workload").exists()
                wait_until_size(target.size)
                return super().pin(launch_pid, target)

        results = tmp_path
        (results / "kitty.pinned").touch()
        (results / "kitty.size").write_text("24 80 0 0\n")
        (results / "kitty.workload").write_text("24 80 0 0\n")
        pin = Watching()
        run_leg(results, pin)
        assert not pin.ran_early
        assert field.read_size(results / "kitty.workload") == field.Size(*self.GRID)


class ForkedChild:
    """The part of `Popen` a leg uses, for a child forked by hand."""

    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.returncode = None

    def poll(self):
        if self.returncode is None:
            with contextlib.suppress(ChildProcessError):
                pid, status = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    self.returncode = os.waitstatus_to_exitcode(status)
        return self.returncode

    def send_signal(self, sig):
        if self.poll() is None:
            os.kill(self.pid, sig)


def wait_until(predicate, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return predicate()


def wait_until_size(path: Path, timeout: float = 10.0) -> field.Size | None:
    wait_until(lambda: field.read_size(path) is not None, timeout)
    return field.read_size(path)


class TestToolResolution:
    def test_pinned_path_wins_over_path_lookup(self, tmp_path, monkeypatch):
        pinned = tmp_path / "kitty"
        pinned.write_text("")
        monkeypatch.setenv("FELIS_BENCH_KITTY", str(pinned))
        monkeypatch.setattr(ct.shutil, "which", lambda _name: "/usr/bin/kitty")
        tool = ct.resolve("kitty")
        assert tool.path == str(pinned)
        assert tool.pinned

    def test_use_path_ignores_the_pin(self, monkeypatch):
        monkeypatch.setenv("FELIS_BENCH_KITTY", "/nix/store/x/bin/kitty")
        monkeypatch.setattr(ct.shutil, "which", lambda _name: "/usr/bin/kitty")
        tool = ct.resolve("kitty", use_path=True)
        assert tool.path == "/usr/bin/kitty"
        assert not tool.pinned

    def test_a_stale_pin_falls_back_instead_of_failing(self, monkeypatch):
        # A store path from an older shell can be garbage-collected; the
        # run should degrade to PATH rather than launch a missing binary.
        monkeypatch.setenv("FELIS_BENCH_KITTY", "/nix/store/gone/bin/kitty")
        monkeypatch.setattr(ct.shutil, "which", lambda _name: "/usr/bin/kitty")
        tool = ct.resolve("kitty")
        assert tool.path == "/usr/bin/kitty"

    def test_tool_override_beats_the_pin(self, tmp_path, monkeypatch):
        tip = tmp_path / "ghostty"
        tip.write_text("")
        monkeypatch.setenv("FELIS_BENCH_GHOSTTY", "/nix/store/x/bin/ghostty")
        tool = ct.resolve("ghostty", override=str(tip))
        assert tool.path == str(tip)
        # An overridden build is by definition not the flake's, so the
        # report must not claim the run was reproducible.
        assert not tool.pinned

    def test_override_pointing_nowhere_is_an_error_not_a_fallback(self, monkeypatch):
        monkeypatch.setenv("FELIS_BENCH_GHOSTTY", "/nix/store/x/bin/ghostty")
        with pytest.raises(FileNotFoundError):
            ct.resolve("ghostty", override="/does/not/exist")

    def test_full_field_requires_both_ghostty_builds(self):
        assert ct.missing_ghostty(["kitty", "ghostty", "ghostty-tip"]) == []
        assert ct.missing_ghostty(["kitty", "ghostty-tip"]) == ["ghostty"]
        assert ct.missing_ghostty(["kitty"]) == ["ghostty", "ghostty-tip"]

    def test_override_parsing(self):
        assert ct.parse_overrides(["ghostty=/a/b"]) == {"ghostty": "/a/b"}
        with pytest.raises(ValueError):
            ct.parse_overrides(["ghostty"])
        with pytest.raises(KeyError):
            ct.resolve_field(False, {"nosuchterm": "/a/b"})

    def test_suite_params_carry_every_resolved_tool_path(self):
        tools = {
            "vtebench": ct.Tool("vtebench", "/vt/bin/vtebench", True),
            "tb": ct.Tool("tb", "/tb/bin/tb", True),
            "hyperfine": ct.Tool("hyperfine", None, False),
            "kitty": ct.Tool("kitty", "/k/bin/kitty", True),
            "alacritty": ct.Tool("alacritty", None, False),
            "wezterm": ct.Tool("wezterm", "/w/bin/wezterm", True),
            "ghostty": ct.Tool("ghostty", None, False),
            "foot": ct.Tool("foot", None, False),
        }
        tools |= {
            "kitten": ct.Tool("kitten", "/k/bin/kitten", True),
            "doom-fire": ct.Tool("doom-fire", "/d/bin/DOOM-fire", True),
        }
        params = ct.harness_params(tools, {"MAX_SECS": "10"}, Path("/repo"))
        assert params["VTEBENCH_BIN"] == "/vt/bin/vtebench"
        assert params["TB_BIN"] == "/tb/bin/tb"
        assert params["KITTEN_BIN"] == "/k/bin/kitten"
        assert params["DOOM_BIN"] == "/d/bin/DOOM-fire"
        assert params["PAYLOAD_DIR"] == "/repo/target/bench-payloads"
        assert params["MAX_SECS"] == "10"
        # A tool that is not installed must be absent rather than empty:
        # the suite that needs it is skipped by name, up front.
        assert "HYPERFINE_BIN" not in params

    def test_only_found_terminals_enter_the_field(self):
        tools = {
            "kitty": ct.Tool("kitty", "/k/bin/kitty", True),
            "alacritty": ct.Tool("alacritty", None, False),
        }
        found = [n for n in ct.TERMINALS if tools.get(n) and tools[n].found]
        fld = field.Field(
            binaries={n: tools[n].path for n in found},
            felis_bin="/bin/felis",
            pres=field.Presentation(),
        )
        assert fld.names() == ["kitty"]
        assert fld.launch("alacritty") is None


class TestEnvinfo:
    def test_pmset_batt_reads_source_and_charge(self):
        text = (
            "Now drawing from 'AC Power'\n"
            " -InternalBattery-0 (id=12345)\t78%; charging; 1:23 remaining present: true"
        )
        assert envinfo.parse_pmset_batt(text) == {
            "source": "AC Power",
            "battery_percent": 78,
        }

    def test_pmset_therm_reads_the_speed_limit(self):
        text = (
            "Note: No thermal warning level has been recorded\n"
            "CPU_Scheduler_Limit \t= 100\nCPU_Speed_Limit \t= 62\n"
        )
        assert envinfo.parse_pmset_therm(text) == {
            "cpu_scheduler_limit": 100,
            "cpu_speed_limit": 62,
        }

    def test_displays_json_yields_gpu_and_panel(self):
        payload = {
            "SPDisplaysDataType": [
                {
                    "sppci_model": "Apple M4 Max",
                    "sppci_cores": "40",
                    "spdisplays_mtlgpufamilysupport": "spdisplays_metal4",
                    "spdisplays_ndrvs": [
                        {
                            "_name": "LG ULTRAWIDE",
                            "_spdisplays_resolution": "3440 x 1440 @ 60.00Hz",
                        }
                    ],
                }
            ]
        }
        parsed = envinfo.parse_displays(payload)
        assert parsed["gpus"][0]["cores"] == "40"
        assert parsed["displays"][0]["resolution"] == "3440 x 1440 @ 60.00Hz"

    def test_proc_parsers(self):
        assert (
            envinfo.parse_meminfo("MemTotal:       32768000 kB\nMemFree: 100 kB\n")
            == 32768000 * 1024
        )
        assert (
            envinfo.parse_cpuinfo("processor\t: 0\nmodel name\t: AMD Ryzen 9 7950X\n")
            == "AMD Ryzen 9 7950X"
        )

    def test_missing_fields_stay_none_rather_than_guessed(self):
        assert envinfo.parse_cpuinfo("") is None
        assert envinfo.parse_meminfo("") is None
        assert envinfo.first_line(None) is None

    def test_the_revision_reported_is_the_binary_s_not_the_tree_s(self, monkeypatch):
        # A `nix build` result is the case that made this necessary: the
        # store binary was b27da75 while the checkout had moved to
        # 43a4a20b, and the report credited the newer commit.
        monkeypatch.setattr(
            envinfo,
            "run",
            lambda *cmd, **kw: (
                "cli    (b27da75)\nclient (b27da75)\ndaemon (not running)"
                if "--version" in cmd
                else "43a4a20b"
            ),
        )
        info = envinfo.felis_info(Path("/repo"), Path("/nix/store/x/bin/felis"))
        assert info["revision"] == "b27da75"
        assert info["checkout"] == "43a4a20b"
        assert "b27da75" in info["drift"]
        assert "43a4a20b" in info["drift"]

    def test_a_binary_built_from_the_checkout_reports_no_drift(self):
        assert envinfo.revision_drift("b27da75", "b27da75c") is None
        assert envinfo.revision_drift("b27da75c9f1", "b27da75") is None
        # Nothing to compare against is not a mismatch.
        assert envinfo.revision_drift(None, "b27da75") is None
        assert envinfo.revision_drift("b27da75", None) is None

    def test_uncommitted_work_is_drift_even_when_the_hashes_agree(self):
        # The binary may or may not contain the working tree's edits and
        # its hash cannot say which, so the revision does not identify it.
        drift = envinfo.revision_drift("b27da75", "b27da75-dirty")
        assert drift is not None
        assert "uncommitted" in drift

    def test_a_version_line_without_a_hash_leaves_the_checkout_standing(
        self, monkeypatch
    ):
        assert envinfo.paren_hash("felis 0.1.0") is None
        assert envinfo.paren_hash(None) is None
        monkeypatch.setattr(
            envinfo,
            "run",
            lambda *cmd, **kw: None if "--version" in cmd else "43a4a20b",
        )
        info = envinfo.felis_info(Path("/repo"), Path("/gone/felis"))
        assert info["revision"] == "43a4a20b"
        assert "drift" not in info

    def test_a_binary_stamped_unknown_is_drift(self, monkeypatch):
        # Nothing ties an unstamped cargo build to the checkout, so
        # crediting the checkout's revision to it must be called out.
        monkeypatch.setattr(
            envinfo,
            "run",
            lambda *cmd, **kw: (
                "felis 0.2.0 (unknown)" if "--version" in cmd else "43a4a20b"
            ),
        )
        info = envinfo.felis_info(Path("/repo"), Path("/repo/target/release/felis"))
        assert info["revision"] == "43a4a20b"
        assert "no revision" in info["drift"]

    def test_a_store_path_reports_no_build_time_rather_than_1970(self, tmp_path):
        # Nix normalizes store mtimes to the epoch; formatting that as a
        # date puts a field in the report that looks answered and is not.
        binary = tmp_path / "felis"
        binary.write_bytes(b"x")
        os.utime(binary, (1, 1))
        assert envinfo.build_time(binary) is None
        os.utime(binary, (1_700_000_000, 1_700_000_000))
        assert envinfo.build_time(binary) is not None

    def test_tool_row_records_where_the_binary_came_from(self, monkeypatch):
        monkeypatch.setattr(
            envinfo, "tool_version", lambda _path, _name: "kitty 0.47.4"
        )
        pinned = envinfo.describe_tool("kitty", "/nix/store/x/bin/kitty", True)
        host = envinfo.describe_tool("kitty", "/usr/bin/kitty", False)
        assert pinned["source"] == "flake"
        assert host["source"] == "path"
        assert envinfo.describe_tool("foot", None, False) == {
            "name": "foot",
            "found": False,
        }


def ioreg_root(**root) -> str:
    return plistlib.dumps(root).decode()


class TestDisplayState:
    """Whether a leg's window could have presented a frame at all."""

    def test_the_console_lock_is_read_from_the_io_registry_root(self):
        assert envinfo.parse_console_lock(ioreg_root(IOConsoleLocked=True).encode())
        assert not envinfo.parse_console_lock(
            ioreg_root(IOConsoleLocked=False).encode()
        )

    def test_without_the_root_flag_the_console_session_answers(self):
        session = {"kCGSSessionOnConsoleKey": True, "CGSSessionScreenIsLocked": True}
        other = {"kCGSSessionOnConsoleKey": False, "CGSSessionScreenIsLocked": False}
        raw = ioreg_root(IOConsoleUsers=[other, session]).encode()
        assert envinfo.parse_console_lock(raw)
        # An unlocked session omits the key rather than setting it false.
        unlocked = ioreg_root(IOConsoleUsers=[{"kCGSSessionOnConsoleKey": True}])
        assert not envinfo.parse_console_lock(unlocked.encode())

    def test_no_console_session_or_no_plist_is_unknown(self):
        assert envinfo.parse_console_lock(ioreg_root().encode()) is None
        assert envinfo.parse_console_lock(b"not a plist") is None

    def test_macos_reads_the_lock_and_the_main_display_s_sleep(self, monkeypatch):
        def run(*cmd, **_kw):
            if cmd[0] == "ioreg":
                return ioreg_root(IOConsoleLocked=False)
            return "1" if cmd[0] == "osascript" else None

        monkeypatch.setattr(envinfo.sys, "platform", "darwin")
        monkeypatch.setattr(envinfo, "run", run)
        assert envinfo.display_state() == {"locked": False, "asleep": True}

    def test_linux_reads_logind_s_hint_on_the_seat_s_active_session(self, monkeypatch):
        answers = {"show-seat": "7", "show-session": "yes"}
        seen = []

        def run(*cmd, **_kw):
            seen.append(cmd)
            return answers.get(cmd[1])

        monkeypatch.setattr(envinfo.sys, "platform", "linux")
        monkeypatch.setattr(envinfo, "run", run)
        state = envinfo.display_state()
        # Display sleep has no reading on Linux: unknown, never "awake".
        assert state == {"locked": True, "asleep": None}
        assert seen[1][2] == "7"

    def test_a_host_without_logind_is_unknown_not_unlocked(self, monkeypatch):
        monkeypatch.setattr(envinfo.sys, "platform", "linux")
        monkeypatch.setattr(envinfo, "run", lambda *_a, **_k: None)
        assert envinfo.display_state() == {"locked": None, "asleep": None}

    def test_only_a_known_dark_display_refuses(self):
        def sample(**display):
            return {"loadavg": 0.1, "display": display}

        assert envinfo.cannot_present(sample(locked=False, asleep=False)) is None
        assert envinfo.cannot_present(sample(locked=None, asleep=None)) is None
        assert envinfo.cannot_present({"loadavg": 0.1}) is None
        assert (
            envinfo.cannot_present(sample(locked=True, asleep=True))
            == "the screen was locked and the display was asleep"
        )

    @pytest.mark.usefixtures("presenting")
    def test_every_leg_sample_carries_the_display(self):
        assert envinfo.leg_env()["display"] == PRESENTING


class TestScale:
    def two_rounds(self, better: str, felis: list[float], kitty: list[float]):
        suite = report.Suite("k", "t", "s", "ms", better, rounds=2)
        for index, value in enumerate(felis, 1):
            suite.put("felis", "x", report.Point(value), index)
        for index, value in enumerate(kitty, 1):
            suite.put("kitty", "x", report.Point(value), index)
        return suite

    def test_ratio_direction_follows_the_metric(self):
        lower = self.two_rounds("lower", [50, 50], [100, 100])
        assert report.ratio_text(lower, "x") == "2.00x"

        higher = self.two_rounds("higher", [50, 50], [100, 100])
        assert report.ratio_text(higher, "x") == "0.50x"

    def test_overlapping_ranges_print_no_ratio(self):
        # 95-105 against 100-110 is a difference the run did not
        # resolve, and two decimals would present it as one.
        overlapping = self.two_rounds("lower", [95, 105], [100, 110])
        assert report.ratio(overlapping, "x") == ("—", report.OVERLAP)
        apart = self.two_rounds("lower", [95, 99], [100, 110])
        assert report.ratio(apart, "x")[0] == "1.08x"

    def test_one_round_has_no_noise_floor_to_clear(self):
        single = report.Suite("k", "t", "s", "ms", "lower")
        single.put("felis", "x", report.Point(50))
        single.put("kitty", "x", report.Point(100))
        assert report.ratio(single, "x") == ("—", report.SINGLE_ROUND)
        # A terminal that produced one round of three is the same case,
        # whichever side of the comparison it is on.
        partial = self.two_rounds("lower", [50, 50], [100])
        partial.rounds = 3
        assert report.ratio(partial, "x") == ("—", report.SINGLE_ROUND)

    def test_a_point_is_the_median_of_its_rounds_and_the_range_its_whisker(self):
        suite = self.two_rounds("lower", [50, 90, 70], [100, 100, 100])
        point = suite.data["felis"]["x"]
        assert (point.value, point.lo, point.hi, point.rounds) == (70, 50, 90, 3)

    def test_a_single_round_point_keeps_its_within_leg_spread(self):
        suite = report.Suite("k", "t", "s", "ms", "lower")
        suite.put("felis", "x", report.Point(50, 4))
        point = suite.data["felis"]["x"]
        assert (point.err, point.lo, point.hi, point.rounds) == (4, None, None, 1)

    def test_the_err_of_a_multi_round_point_is_the_median_within_leg_spread(self):
        suite = report.Suite("k", "t", "s", "ms", "lower", rounds=3)
        for index, (value, err) in enumerate([(50, 1), (60, 9), (70, 5)], 1):
            suite.put("felis", "x", report.Point(value, err), index)
        assert suite.data["felis"]["x"].err == 5

    def test_a_bar_with_one_round_draws_no_whisker(self):
        suite = self.two_rounds("lower", [50, 90], [100])
        suite.rounds = 2
        points = [suite.data["felis"]["x"], suite.data["kitty"]["x"]]
        lows, highs = report.whiskers(suite, points)
        assert (lows, highs) == ([20.0, 0.0], [20.0, 0.0])
        assert "1/2 rounds" in report.bar_label(suite, points[1])

    def test_the_subtitle_names_the_whisker_it_drew(self):
        suite = self.two_rounds("lower", [50, 90], [100, 110])
        ld.whisker_note(suite)
        assert "median over 2 rounds" in suite.subtitle
        single = report.Suite("k", "t", "Sub.", "ms", "lower")
        single.put("felis", "x", report.Point(50, 4))
        ld.whisker_note(single)
        assert "within-run spread" in single.subtitle


class TestRender:
    def build(self) -> report.Suite:
        suite = report.Suite("k", "Title", "Sub", "ms", "lower")
        suite.put("felis", "cat", report.Point(50, 5))
        suite.put("kitty", "cat", report.Point(200, 40))
        return suite

    def test_the_page_carries_the_numbers_and_the_lead(self):
        suite = report.Suite("k", "Title", "Sub", "ms", "lower", rounds=2)
        for index, (felis, kitty) in enumerate([(50, 200), (60, 210)], 1):
            suite.put("felis", "cat", report.Point(felis, 5), index)
            suite.put("kitty", "cat", report.Point(kitty, 40), index)
        page = report.render_markdown(
            [suite], {"felis": {"revision": "abc1234"}}, "T", {}
        )
        assert "55.0 ms [50.0–60.0] ± 5.0 · 2/2 rounds" in page
        assert "felis vs best other" in page
        assert "3.73x" in page
        assert "abc1234" in page

    def test_a_single_round_page_says_why_its_ratio_column_is_empty(self):
        page = report.render_markdown([self.build()], {}, "T", {})
        assert "50.0 ms ± 5.0" in page
        assert "single round, no noise floor" in page

    def test_a_chart_is_linked_only_when_one_was_drawn(self):
        # A report that renders without matplotlib must not point at
        # images that were never written.
        suite = self.build()
        assert "![" not in report.render_markdown([suite], {}, "T", {})
        linked = report.render_markdown([suite], {}, "T", {"k": "png/k.png"})
        assert "![Title](png/k.png)" in linked

    def test_a_terminal_that_was_not_measured_is_a_dash(self):
        suite = self.build()
        suite.put("wezterm", "other", report.Point(9))
        page = report.render_markdown([suite], {}, "T", {})
        assert "| — |" in page

    def test_a_pipe_in_a_value_does_not_end_its_column(self):
        suite = self.build()
        suite.put("felis", "a|b", report.Point(1))
        row = [
            line
            for line in report.render_markdown([suite], {}, "T", {}).splitlines()
            if "a\\|b" in line
        ]
        assert row, "the category name must survive as one cell"

    def test_provenance_block_summarizes_the_machine(self):
        meta = {
            "host": "bench-box",
            "machine": {
                "cpu": "Apple M4 Max",
                "cpu_performance_cores": 12,
                "cpu_efficiency_cores": 4,
                "memory_bytes": 137438953472,
                "gpus": [{"model": "Apple M4 Max", "cores": "40"}],
                "displays": [{"resolution": "3440 x 1440 @ 60.00Hz"}],
                "power": {"source": "AC Power", "cpu_speed_limit": 62},
            },
            "os": {"name": "macOS", "version": "26.4.1", "build": "25E253"},
            "tools": [
                {
                    "name": "kitty",
                    "found": True,
                    "version": "kitty 0.47.4",
                    "source": "flake",
                },
                {
                    "name": "ghostty",
                    "found": True,
                    "version": "Ghostty 1.3.1",
                    "source": "path",
                },
                {"name": "foot", "found": False},
            ],
            "pinned_shell": False,
        }
        page = report.render_markdown([self.build()], meta, "T", {})
        assert "12P + 4E cores" in page
        assert "128 GB" in page
        assert "3440 x 1440 @ 60.00Hz" in page
        # A throttled run and an unpinned field are the two facts that
        # invalidate a comparison, so both must be visible in the report.
        assert "CPU speed limit 62%" in page
        assert "nix develop .#bench" in page
        assert "unpinned" in page

    def test_a_binary_that_is_not_the_checkout_s_is_flagged_in_the_page(self):
        meta = {
            "felis": {
                "revision": "b27da75",
                "checkout": "43a4a20b",
                "drift": "binary is b27da75, the checkout is 43a4a20b",
            }
        }
        page = report.render_markdown([self.build()], meta, "T", {})
        assert "b27da75" in page
        assert "the checkout is 43a4a20b" in page

    def test_a_missing_chart_library_still_produces_the_tables(
        self, tmp_path, monkeypatch
    ):
        # The tables are the report; the charts are how it is read. A
        # run that measured for two hours must not lose both.
        out = tmp_path / "report.md"
        monkeypatch.setattr(
            report, "draw", mock.Mock(side_effect=ModuleNotFoundError("matplotlib"))
        )
        with contextlib.redirect_stdout(io.StringIO()):
            report.write(tmp_path, [self.build()], out, tmp_path / "png", "T")
        assert "50.0 ms" in out.read_text()
        assert "![" not in out.read_text()


@pytest.mark.skipif(
    not importlib.util.find_spec("matplotlib"), reason="charts need the .#bench shell"
)
class TestChart:
    def test_a_facet_is_drawn_per_benchmark(self, tmp_path):
        suite = report.Suite("k", "Title", "Sub", "ms", "lower")
        for cat in ("fast", "slow"):
            suite.put("felis", cat, report.Point(50, 5))
            suite.put("kitty", cat, report.Point(200, 40))
        path = tmp_path / "k.png"
        report.draw(suite, path)
        assert path.exists()
        assert path.stat().st_size > 0
        assert path.read_bytes()[1:4] == b"PNG"


class TestHarnessTable:
    @pytest.mark.parametrize("name", list(ld.HARNESSES))
    def test_every_suite_has_something_to_run(self, name):
        assert name in suites.SUITES, f"{name} has no measuring function"

    # A suite with no implementation must report itself skipped rather
    # than pretend the platform is unsupported wholesale.
    @pytest.mark.parametrize(
        "name", ["vtebench", "termbench", "doom-fire", "startup", "latency"]
    )
    def test_platforms_match_the_available_suite_implementations(self, name):
        assert ld.HARNESSES[name].runs_on("linux")
        assert ld.HARNESSES[name].runs_on("darwin")

    def test_each_platform_is_told_its_own_instrument(self):
        # Asking Linux for `typometer` would skip the suite for a missing
        # tool it is never going to use, and asking macOS for the Wayland
        # one would do the same in reverse.
        latency = ld.HARNESSES["latency"]
        assert latency.needs("darwin") == ["typometer"]
        assert latency.needs("linux") == ["wl-latency"]

    def test_a_suite_that_needs_one_tool_everywhere_says_so_once(self):
        assert ld.HARNESSES["cat"].needs("linux") == ["hyperfine"]
        assert ld.HARNESSES["cat"].needs("darwin") == ["hyperfine"]


class TestWindowPinSelection:
    def test_the_guard_matches_the_process_name_not_a_command_line(self, monkeypatch):
        # The predecessor matched `pgrep -f "paneru launch"`, a command
        # line paneru does not have, so it passed while paneru was
        # running and the field came out at the WM's sizes.
        run = mock.Mock(return_value=mock.Mock(returncode=1))
        monkeypatch.setattr(mactiler.subprocess, "run", run)
        mactiler.running_window_manager()
        for call in run.call_args_list:
            assert call.args[0][:2] == ["pgrep", "-x"]

    def test_the_first_running_manager_is_named(self, monkeypatch):
        monkeypatch.setattr(
            mactiler.subprocess,
            "run",
            lambda argv, **_: mock.Mock(returncode=0 if argv[2] == "yabai" else 1),
        )
        assert mactiler.running_window_manager() == "yabai"

    @pytest.mark.parametrize("name", mactiler.TILING_WMS)
    def test_no_tiler_reaches_the_floating_pin(self, name, monkeypatch):
        # A registry naming one of the three would let the other two
        # fall through to the pin that believes every size flag, and
        # silently un-pin the field.
        monkeypatch.setattr(mactiler, "running_window_manager", lambda: name)
        pin = wm.select()
        assert isinstance(pin, wm.MacTilerPin)
        with pytest.raises(wm.PinRefused):
            pin.prepare()

    def test_niri_wins_over_the_floating_pin_when_it_is_running(self, monkeypatch):
        monkeypatch.setattr(mactiler, "running_window_manager", lambda: None)
        monkeypatch.setenv("NIRI_SOCKET", "/run/niri.sock")
        monkeypatch.setattr(wm.niri.shutil, "which", lambda _: "/bin/niri")
        assert isinstance(wm.select(), wm.NiriPin)

    def test_a_desktop_with_no_tiler_floats(self, monkeypatch):
        monkeypatch.setattr(mactiler, "running_window_manager", lambda: None)
        monkeypatch.delenv("NIRI_SOCKET", raising=False)
        pin = wm.select()
        assert isinstance(pin, wm.FloatingPin)
        assert not pin.can_resize


class TestLaunch:
    """The pinned launch argv — what "the same conditions" actually means."""

    PRES = field.Presentation(
        family="DejaVu Sans Mono", pt=9, scale=2.0, scrollback=5000, rows=40, cols=120
    )

    # ghostty's scrollback key was renamed after 1.3.1, so build_launch
    # probes the binary. Tests supply the answer instead of needing one
    # installed; TIP_CAPS is what a post-1.3.1 build reports.
    TIP_CAPS = frozenset({"scrollback-limit-lines", "scrollback-limit-bytes"})

    TERMINALS = ("kitty", "alacritty", "wezterm", "ghostty", "foot")

    @pytest.fixture
    def build(self):
        launches = []

        def build(name, caps=frozenset()):
            launch = field.build_launch(name, f"/bin/{name}", self.PRES, caps=caps)
            launches.append(launch)
            return launch

        yield build
        for launch in launches:
            for p in launch.tmp:
                p.unlink(missing_ok=True)

    @pytest.fixture
    def rendered(self, build):
        def rendered(name, caps=frozenset()) -> str:
            """argv plus any config file it writes, as one searchable string."""
            launch = build(name, caps)
            bodies = [p.read_text() for p in launch.tmp]
            return " ".join([*launch.argv, *launch.env.values(), *bodies])

        return rendered

    # The failure this prevents: alacritty and ghostty read ~/.config
    # unless told otherwise, so a developer's own ligature font and
    # scrollback were being benchmarked against felis's defaults. Each
    # entry is that terminal's opt-out.
    @pytest.mark.parametrize(
        ("name", "flag"),
        [
            ("kitty", "--config NONE"),
            ("alacritty", "--config-file"),
            ("wezterm", "WEZTERM_CONFIG_FILE"),
            ("ghostty", "--config-default-files=false"),
            ("foot", "-c /dev/null"),
        ],
    )
    def test_every_terminal_starts_from_a_pristine_config(self, build, name, flag):
        launch = build(name)
        assert flag in " ".join(launch.argv) + " ".join(launch.env), (
            f"{name} would read the developer's own config"
        )

    @pytest.mark.parametrize("name", TERMINALS)
    def test_every_terminal_is_told_the_same_font(self, rendered, name):
        assert "DejaVu Sans Mono" in rendered(name)
        assert re.search(r"\b9\b", rendered(name))

    @pytest.mark.parametrize("name", TERMINALS)
    def test_every_terminal_is_told_the_same_grid_in_cells(self, rendered, name):
        assert "120" in rendered(name)
        assert "40" in rendered(name)

    def test_the_grid_flags_are_the_cell_flags_not_the_pixel_ones(self, build):
        # kitty's flag is pixels without the `c` suffix, and foot's -w is
        # pixels where -W is cells; both would silently mean something else.
        assert "initial_window_width=120c" in build("kitty").argv
        assert "-W" in build("foot").argv

    # The defaults span an order of magnitude — foot 1000, kitty 2000,
    # wezterm 3500, alacritty 10000 — and the flooded half of the memory
    # suite is decided by whichever cap binds first.
    @pytest.mark.parametrize(
        ("name", "setting"),
        [
            ("kitty", "scrollback_lines=5000"),
            ("alacritty", "scrolling.history=5000"),
            ("wezterm", "scrollback_lines = 5000"),
            ("foot", "scrollback.lines=5000"),
        ],
    )
    def test_every_terminal_retains_the_same_scrollback_depth(
        self, rendered, name, setting
    ):
        assert setting in rendered(name)

    def test_ghostty_takes_the_line_cap_only_where_the_build_has_one(self, build):
        # 1.3.1 caps by bytes alone and no byte count honestly maps to a
        # row count, so it keeps its default; passing the key anyway
        # would make ghostty open an error dialog instead of the wrapper.
        assert "--scrollback-limit-lines=5000" in build("ghostty", self.TIP_CAPS).argv
        assert "scrollback" not in " ".join(build("ghostty").argv)

    def test_a_terminal_that_escaped_the_scrollback_pin_is_named(self, monkeypatch):
        # A pin that silently failed to apply is worse than no pin: the
        # chart lines the bars up either way, so the caveat has to
        # travel with the results.
        monkeypatch.setattr(field, "ghostty_config_keys", lambda _bin: frozenset())
        assert field.unpinned_scrollback({"ghostty": "/bin/ghostty"}) == [
            "ghostty: byte-capped only, ran at its default"
        ]
        monkeypatch.setattr(field, "ghostty_config_keys", lambda _bin: self.TIP_CAPS)
        assert field.unpinned_scrollback({"ghostty": "/bin/ghostty"}) == []

    # felis, kitty and ghostty blink out of the box and the other three
    # do not. A blink is a repaint twice a second forever, which is
    # invisible in a throughput number and is most of an idle CPU one.
    @pytest.mark.parametrize(
        ("name", "setting"),
        [
            ("kitty", "cursor_blink_interval=0"),
            ("alacritty", 'cursor.style.blinking="Never"'),
            ("wezterm", "cursor_blink_rate = 0"),
            ("ghostty", "--cursor-style-blink=false"),
            ("foot", "cursor.blink=no"),
        ],
    )
    def test_no_terminal_blinks_its_cursor(self, rendered, name, setting):
        assert setting in rendered(name)

    def test_felis_does_not_blink_its_cursor(self):
        assert 'blink = "never"' in self.PRES.felis_config()

    # The line between the two kinds of setting: how much work there is
    # gets pinned, how a terminal chooses to serve it does not.
    # Equalising vsync or a repaint delay would measure a configuration
    # none of these terminals ship.
    @pytest.mark.parametrize("name", TERMINALS)
    @pytest.mark.parametrize(
        "knob", ["repaint_delay", "input_delay", "sync_to_monitor", "vsync", "fps"]
    )
    def test_frame_pacing_is_never_pinned(self, rendered, name, knob):
        assert knob not in rendered(name, self.TIP_CAPS)

    def test_an_unprobed_grid_leaves_the_size_flags_off(self):
        # Better an unpinned window than one pinned to a guess: the
        # report reads the grid back from the run either way.
        pres = field.Presentation(rows=None, cols=None)
        argv = field.build_launch("kitty", "/bin/kitty", pres).argv
        assert "initial_window_width=None c" not in " ".join(argv)
        assert "initial_window_height" not in " ".join(argv)

    # alacritty and ghostty take `-e CMD`; a wrapper appended before the
    # flag would be read as a positional and ignored.
    @pytest.mark.parametrize("name", ["alacritty", "ghostty"])
    def test_the_child_command_lands_after_the_exec_flag(self, build, name):
        assert build(name).command("/tmp/w.sh")[-2:] == ["-e", "/tmp/w.sh"]

    def test_a_font_family_with_spaces_survives_the_hyperfine_string(self, build):
        # hyperfine takes one shell string, so the argv has to be quoted
        # back into one; unquoted, "DejaVu Sans Mono" becomes three args.
        command = build("ghostty").shell_command("/tmp/w.sh")
        assert "'--font-family=DejaVu Sans Mono'" in command

    def test_an_unknown_terminal_is_an_error_not_an_empty_launch(self):
        with pytest.raises(KeyError):
            field.build_launch("xterm", "/bin/xterm", self.PRES)

    def test_a_variant_launches_exactly_like_the_build_it_stands_beside(self):
        # The point of the ghostty-tip bar is that the only difference
        # from the ghostty bar is the binary; a flag that drifted between
        # the two would turn the comparison into a different question.
        stable = field.build_launch("ghostty", "/bin/g", self.PRES).argv
        tip = field.build_launch("ghostty-tip", "/bin/g", self.PRES).argv
        assert stable == tip

    def test_the_variant_sits_next_to_its_release_in_the_field(self):
        order = list(field.FIELD_ORDER)
        assert order[order.index("ghostty") + 1] == "ghostty-tip"
        # And in the palette, so the pair reads as a pair.
        hues = report.TERM_ORDER
        assert hues[hues.index("ghostty") + 1] == "ghostty-tip"


class TestVariant:
    def test_a_variant_is_never_picked_up_from_path_or_the_flake(self, monkeypatch):
        # A nightly a lockfile or a PATH entry supplies is not a nightly;
        # it enters the field only when someone names a build.
        monkeypatch.setenv("FELIS_BENCH_GHOSTTY_TIP", "/nix/store/x/bin/ghostty")
        monkeypatch.setattr(ct.shutil, "which", lambda _name: None)
        tool = ct.resolve("ghostty-tip")
        # The env pin exists but points nowhere, so nothing is found —
        # and the flake never sets it in the first place.
        assert not tool.found

    def test_naming_a_build_puts_it_in_the_field_beside_the_release(self, tmp_path):
        tip = tmp_path / "ghostty"
        tip.write_text("")
        tools = ct.resolve_field(False, {"ghostty-tip": str(tip)})
        assert tools["ghostty-tip"].path == str(tip)
        assert not tools["ghostty-tip"].pinned

    def test_an_unpinned_tool_is_recorded_by_digest_not_only_by_name(self, tmp_path):
        # ghostty-tip prints the same "Ghostty 1.3.1" banner as the
        # release and its file is replaced daily, so a chart is only
        # attributable to one build if the digest is written down.
        binary = tmp_path / "ghostty"
        binary.write_bytes(b"tip")
        row = envinfo.describe_tool("ghostty-tip", str(binary), pinned=False)
        assert (
            row["sha256"]
            == "97380187a878903ffe722b7bfd8d8ba92457ef77de0216cfe9261c72c2b87397"
        )
        assert "built" in row

    def test_a_flake_pinned_tool_needs_no_digest(self, tmp_path):
        # The store path already names its contents; hashing a 50 MB
        # binary on every run to restate that is waste.
        binary = tmp_path / "kitty"
        binary.write_bytes(b"pinned")
        row = envinfo.describe_tool("kitty", str(binary), pinned=True)
        assert "sha256" not in row

    def test_a_vanished_binary_is_reported_without_a_digest_not_a_crash(self):
        row = envinfo.describe_tool("ghostty-tip", "/does/not/exist", pinned=False)
        assert row["found"]
        assert "sha256" not in row

    def test_the_child_process_of_a_variant_is_matched_by_its_real_name(self):
        # `ps -o comm=` says "ghostty" for a ghostty-tip leg, so memory
        # sampling has to look for the alias or it counts no children.
        assert field.LAUNCH_ALIAS["ghostty-tip"] == "ghostty"


class TestFelisConfig:
    def test_the_config_writes_the_key_felis_actually_reads(self):
        pres = field.Presentation(family="Menlo", pt=9, scale=2.0)
        body = pres.felis_config()
        assert 'family = "Menlo"' in body
        # `font.size` is not a key: felis logs "unknown key ignored" for
        # it and runs its 14px default, which is the whole field's grid
        # since the others are pinned to whatever felis probed.
        assert "size_px = " in body
        assert "\nsize = " not in body

    def test_the_backing_scale_stays_out_of_the_pin(self):
        # Not 18: the client multiplies `font.size_px` by the window's
        # scale factor itself, so a pre-multiplied pin scales twice.
        expect = "size_px = 9" if field.DARWIN else "size_px = 12"
        assert expect in field.Presentation(pt=9, scale=2.0).felis_config()

    def test_features_are_pinned_off(self):
        # A ligature font with `calt` on shapes differently from one
        # without, so the feature list is pinned rather than defaulted.
        assert "features = []" in field.Presentation().felis_config()


class TestBackingScale:
    def test_scale_comes_from_native_over_logical(self):
        retina = {
            "_spdisplays_pixels": "3456 x 2234",
            "_spdisplays_resolution": "1728 x 1117 @ 120.00Hz",
        }
        assert envinfo.backing_scale(retina) == 2.0

    def test_a_one_to_one_panel_is_1x(self):
        plain = {
            "_spdisplays_pixels": "3440 x 1440",
            "_spdisplays_resolution": "3440 x 1440 @ 60.00Hz",
        }
        assert envinfo.backing_scale(plain) == 1.0

    def test_unparseable_resolution_is_unknown_not_1(self):
        # A run at 2x paints four times the pixels of one at 1x, so a
        # guessed 1x would file a Retina run's numbers under the wrong
        # conditions; "unknown" has to survive as far as the caller.
        assert envinfo.backing_scale({"_spdisplays_pixels": "3456 x 2234"}) is None
        assert envinfo.parse_wh("built-in") is None

    def test_main_display_wins_over_the_first_one(self):
        machine = {
            "displays": [
                {"name": "external", "backing_scale": 1.0},
                {"name": "built-in", "backing_scale": 2.0, "main": True},
            ]
        }
        assert envinfo.main_backing_scale(machine) == 2.0

    def test_no_display_data_reports_unknown(self):
        assert envinfo.main_backing_scale({"displays": []}) is None


class TestFontPin:
    def test_the_pin_is_the_point_size_in_the_platform_s_logical_pixels(self):
        # A point is a logical pixel on macOS and 3/4 of one under the
        # Wayland convention every other terminal converts against, so
        # an unconverted pin would run felis 25% smaller than the field.
        pt = field.Presentation(pt=9, scale=1.0).px
        assert pt == (9 if field.DARWIN else 12)

    def test_the_backing_scale_is_not_a_conversion_factor(self):
        assert (
            field.Presentation(pt=9, scale=2.0).px
            == field.Presentation(pt=9, scale=1.0).px
        )

    def test_the_point_size_reaches_the_terminals_without_a_stray_decimal(self):
        # kitty and ghostty take the value verbatim, so `9.0` from a
        # float would be a different string than the recorded `9`.
        assert field.Presentation(pt=9.0).pt_str == "9"
        assert field.Presentation(pt=9.5).pt_str == "9.5"

    def test_the_orchestrator_and_the_field_share_one_default(self):
        # Two defaults that disagree would mean a suite run on its own
        # measured a different font than one run through the orchestrator.
        assert ct.DEFAULT_PARAMS["FONT_FAMILY"] == field.DEFAULT_FONT_FAMILY
        assert ct.DEFAULT_PARAMS["FONT_PT"] == f"{field.DEFAULT_FONT_PT:g}"

    def test_the_pinned_condition_is_its_own_report_row(self):
        line = report.pinned_field(
            {
                "GRID_COLS": "212",
                "GRID_ROWS": "56",
                "FONT_FAMILY": "Menlo",
                "FONT_PT": "9",
                "FONT_PX": "9",
            }
        )
        assert line == "212x56 cells · Menlo 9pt"

    def test_a_run_whose_felis_pin_differed_still_reports_both_sizes(self):
        # Runs recorded before the pin dropped its backing-scale
        # conversion carry FONT_PX != FONT_PT, and their reports have to
        # keep saying which size felis actually ran.
        line = report.pinned_field(
            {"FONT_FAMILY": "Menlo", "FONT_PT": "9", "FONT_PX": "18"}
        )
        assert line == "Menlo 9pt (felis 18px)"

    def test_an_unpinned_run_says_nothing_rather_than_half_a_condition(self):
        assert report.pinned_field({}) == ""


class TestGridVerification:
    def test_one_shared_grid_reads_as_comparable(self, tmp_path):
        for name in ("felis", "kitty", "ghostty"):
            (tmp_path / f"{name}.size").write_text("56 212\n")
        (note,) = ld.grid_note(tmp_path)
        assert "same grid: 212x56" in note

    def test_a_terminal_that_missed_the_pin_is_called_out(self, tmp_path):
        (tmp_path / "felis.size").write_text("56 212\n")
        (tmp_path / "kitty.size").write_text("50 212\n")
        (note,) = ld.grid_note(tmp_path)
        assert "GRIDS DIFFERED" in note
        assert "kitty 212x50" in note

    def test_a_recorded_mismatch_is_carried_into_the_note(self, tmp_path):
        (tmp_path / "felis.size").write_text("56 212\n")
        (tmp_path / "grid-mismatch").write_text(
            "felis fell back from 'Menlo' to monospace\n"
        )
        (note,) = ld.grid_note(tmp_path)
        assert "fell back" in note

    def test_mismatches_are_collected_across_suites(self, tmp_path):
        (tmp_path / "memory").mkdir()
        (tmp_path / "memory" / "grid-mismatch").write_text("kitty got '50 212'\n")
        assert ct.mismatches(tmp_path) == ["memory: kitty got '50 212'"]

    def test_a_clean_run_reports_no_mismatch(self, tmp_path):
        assert ct.mismatches(tmp_path) == []


class TestSizeParsing:
    def test_the_four_fields_are_cells_then_pixels(self, tmp_path):
        path = tmp_path / "felis.size"
        path.write_text("56 212 1696 1344\n")
        assert field.read_size(path) == field.Size(56, 212, 1696, 1344)

    def test_a_two_field_file_still_parses(self, tmp_path):
        # What `stty size` wrote before the pixel columns existed; a
        # results root taken then still has to render.
        path = tmp_path / "felis.size"
        path.write_text("56 212\n")
        size = field.read_size(path)
        assert size.cells() == (56, 212)
        assert not size.has_pixels

    def test_a_torn_or_missing_file_is_not_a_grid(self, tmp_path):
        path = tmp_path / "felis.size"
        assert field.read_size(path) is None
        path.write_text("56 212 169")
        assert field.read_size(path) is None
        path.write_text("56 x 1696 1344")
        assert field.read_size(path) is None


class TestPixelVerification:
    """The second column: the same cells drawn in a different area."""

    PRES = field.Presentation(rows=40, cols=100, xpixel=1000, ypixel=800)

    def test_padding_inside_one_cell_is_not_a_mismatch(self):
        got = field.Size(40, 100, 1006, 793)
        assert field.verify_grid("kitty", got, self.PRES) is None

    def test_a_font_that_did_not_apply_is_caught_by_width(self):
        # Same cell count, a wider cell: exactly what a missed font pin
        # looks like, and what a cell count cannot see.
        got = field.Size(40, 100, 1200, 800)
        problem = field.verify_grid("kitty", got, self.PRES)
        assert "1200x800" in problem

    def test_height_is_compared_on_its_own(self):
        # One area against another would accept a wide-short window as a
        # narrow-tall one; they are not the same picture.
        got = field.Size(40, 100, 1000, 700)
        assert field.verify_grid("kitty", got, self.PRES) is not None

    def test_a_terminal_that_reports_no_pixels_is_checked_by_cells(self):
        got = field.Size(40, 100)
        assert field.verify_grid("foot", got, self.PRES) is None

    def test_the_tolerance_is_one_reference_cell_per_axis(self):
        assert field.pixel_tolerance(self.PRES) == (10.0, 20.0)

    def test_a_note_names_the_terminals_that_reported_no_pixels(self, tmp_path):
        (tmp_path / "felis.size").write_text("40 100 1000 800\n")
        (tmp_path / "foot.size").write_text("40 100 0 0\n")
        (note,) = ld.grid_note(tmp_path)
        assert "Pixels unreported by foot" in note

    def test_both_pixel_notes_stand_on_their_own(self, tmp_path):
        # One terminal reporting nothing says nothing about whether the
        # ones that did report drew the same area.
        (tmp_path / "felis.size").write_text("40 100 1000 800\n")
        (tmp_path / "foot.size").write_text("40 100 0 0\n")
        (tmp_path / "kitty.size").write_text("40 100 1200 800\n")
        (note,) = ld.grid_note(tmp_path)
        assert "Pixels unreported by foot" in note
        assert "kitty 1200x800 px" in note

    def test_a_note_names_cells_of_a_different_size(self, tmp_path):
        (tmp_path / "felis.size").write_text("40 100 1000 800\n")
        (tmp_path / "kitty.size").write_text("40 100 1200 800\n")
        (note,) = ld.grid_note(tmp_path)
        assert "not the same size" in note
        assert "kitty 1200x800 px" in note


class TestNiriPin:
    """The mechanics, verified against niri 26.04 before they were built on."""

    def test_a_window_is_matched_over_the_launch_s_process_tree(self):
        # `wezterm start` spawns wezterm-gui, which owns the window, so
        # the launch pid alone matches nothing.
        windows = [{"id": 3, "pid": 99}, {"id": 4, "pid": 4242}]
        assert wm.niri.match_window(windows, {40, 41, 4242}) == (4, "")

    def test_no_mapped_window_is_reported_rather_than_guessed(self):
        found, detail = wm.niri.match_window([{"id": 3, "pid": 99}], {40})
        assert found is None
        assert "no window" in detail

    def test_several_windows_of_one_launch_are_refused(self):
        windows = [{"id": 3, "pid": 40}, {"id": 4, "pid": 41}]
        found, detail = wm.niri.match_window(windows, {40, 41})
        assert found is None
        assert "2 windows" in detail

    def test_a_physical_target_is_asked_for_in_logical_pixels(self):
        # niri's set-window-* take logical pixels and TIOCGWINSZ reports
        # physical ones; at 2x a 1400 px window is asked for as 700.
        assert wm.niri.to_logical(1400, 2.0) == 700
        assert wm.niri.to_logical(1400, 1.0) == 1400

    def test_the_step_moves_by_the_cells_that_are_missing(self):
        # 100 cells in 800 physical px is an 8 px cell; ten more cells
        # at 1x is 80 logical px more.
        assert wm.niri.cell_step(800, 100, 800, 110, 1.0) == 880

    def test_the_step_crosses_the_scale_once(self):
        # The same window at 2x: 400 logical px wide, an 8 px physical
        # cell, so ten more cells is 40 logical px more.
        assert wm.niri.cell_step(400, 100, 800, 110, 2.0) == 440

    def test_a_terminal_reporting_no_pixels_steps_by_the_reference_cell(self):
        # Standing still would spend every attempt asking for the size
        # the window already has, and end tty-pinned for want of a cell
        # size the reference window knows.
        assert wm.niri.cell_step(800, 100, 0, 110, 1.0, 8.0) == 880

    def test_a_step_with_no_cell_size_at_all_leaves_the_window_alone(self):
        assert wm.niri.cell_step(800, 100, 0, 110, 1.0) == 800

    def test_one_output_is_the_one_the_scale_comes_from(self):
        outputs = {"DP-1": {"logical": {"scale": 2.0}}}
        assert wm.niri.output_scale(outputs, None) == ("DP-1", 2.0)

    def test_with_several_outputs_the_focused_one_decides(self):
        outputs = {
            "DP-1": {"logical": {"scale": 1.0}},
            "DP-2": {"logical": {"scale": 2.0}},
        }
        assert wm.niri.output_scale(outputs, "DP-2") == ("DP-2", 2.0)

    def test_an_unreadable_scale_refuses_rather_than_assuming_one(self):
        # Every pixel target crosses this number, so assuming 1.0 would
        # halve or double the whole field silently.
        assert wm.niri.output_scale({}, None) is None
        outputs = {
            "DP-1": {"logical": {"scale": 1.0}},
            "DP-2": {"logical": {"scale": 2.0}},
        }
        assert wm.niri.output_scale(outputs, None) is None


class TestNiriResizeLoop:
    """The loop that walks a tiled window onto the reference grid."""

    @staticmethod
    def resize(monkeypatch, results: Path, lands_on_attempt: int, pixels: bool = True):
        size = results / "kitty.size"
        size.write_text("30 100 800 600\n" if pixels else "30 100 0 0\n")
        target = wm.Target(
            size=size,
            stty=results / "kitty.stty",
            rows=40,
            cols=120,
            xpixel=960,
            ypixel=640,
        )
        window = {
            "id": 7,
            "pid": 4242,
            "is_floating": True,
            "layout": {"window_size": [800, 600]},
        }
        asked = []

        def action(*args):
            if args[0] == "set-window-height":
                asked.append(args)
                if len(asked) >= lands_on_attempt:
                    size.write_text("40 120 960 640\n" if pixels else "40 120 0 0\n")
            return True

        pin = wm.NiriPin()
        pin.scale = 1.0
        monkeypatch.setattr(wm.niri.field, "process_tree", lambda _pid: {4242})
        monkeypatch.setattr(wm.niri, "msg", lambda *_a: json.dumps([window]))
        monkeypatch.setattr(wm.niri, "action", action)
        monkeypatch.setattr(
            wm.niri, "wait_settled", lambda path, _t: field.read_size(path)
        )
        monkeypatch.setattr(
            wm.NiriPin,
            "wait_for_change",
            lambda _s, path, _p, timeout=0: field.read_size(path),
        )
        return pin.pin(4242, target), asked

    def test_the_window_is_walked_onto_the_reference_grid(self, tmp_path, monkeypatch):
        result, asked = self.resize(monkeypatch, tmp_path, lands_on_attempt=1)
        assert result.status == wm.PINNED
        # 100 cells in 800 px is an 8 px cell, so twenty more
        # columns is 160 logical pixels more.
        assert asked[0] == ("set-window-height", "--id", "7", "800")

    def test_the_last_attempt_is_checked_before_the_tty_fallback(
        self, tmp_path, monkeypatch
    ):
        # The match is tested at the top of the loop, so a window that
        # only arrives on the final resize would otherwise be handed a
        # tty pin it does not need.
        results = tmp_path
        result, asked = self.resize(
            monkeypatch, results, lands_on_attempt=wm.niri.MAX_ATTEMPTS
        )
        assert result.status == wm.PINNED
        assert not (results / "kitty.stty").exists()

    def test_a_terminal_that_reports_no_pixels_is_still_resized(
        self, tmp_path, monkeypatch
    ):
        # `cell_step` has no measured cell to work from, so the pin has
        # to carry the reference one or this leg can never be placed.
        results = tmp_path
        result, asked = self.resize(
            monkeypatch, results, lands_on_attempt=1, pixels=False
        )
        assert result.status == wm.PINNED
        assert asked[0] == ("set-window-height", "--id", "7", "760")

    def test_a_window_that_never_arrives_leaves_a_tty_pin(self, tmp_path, monkeypatch):
        results = tmp_path
        result, asked = self.resize(monkeypatch, results, lands_on_attempt=99)
        assert result.status == wm.TTY_ONLY
        assert len(asked) == wm.niri.MAX_ATTEMPTS
        assert (results / "kitty.stty").read_text() == "40 120\n"


class TestFloatingPin:
    def test_a_leg_that_missed_the_grid_leaves_a_tty_pin(self, tmp_path, monkeypatch):
        (tmp_path / "kitty.size").write_text("30 100 800 600\n")
        target = wm.Target(
            size=tmp_path / "kitty.size",
            stty=tmp_path / "kitty.stty",
            rows=40,
            cols=120,
            xpixel=960,
            ypixel=640,
        )
        monkeypatch.setattr(
            wm.floating, "wait_settled", lambda path, _t: field.read_size(path)
        )
        result = wm.FloatingPin().pin(1, target)
        assert result.status == wm.TTY_ONLY
        assert (tmp_path / "kitty.stty").read_text() == "40 120\n"

    def test_a_leg_that_took_the_grid_is_pinned_without_a_tty_pin(
        self, tmp_path, monkeypatch
    ):
        (tmp_path / "kitty.size").write_text("40 120 960 640\n")
        target = wm.Target(
            size=tmp_path / "kitty.size",
            stty=tmp_path / "kitty.stty",
            rows=40,
            cols=120,
        )
        monkeypatch.setattr(
            wm.floating, "wait_settled", lambda path, _t: field.read_size(path)
        )
        result = wm.FloatingPin().pin(1, target)
        assert result.status == wm.PINNED
        assert not (tmp_path / "kitty.stty").exists()


class TestRoundLayout:
    def test_a_root_without_round_directories_is_one_implicit_round(self, tmp_path):
        (tmp_path / "kitty.kitten").write_text("ascii : 1s @ 100.0 MB/s\n")
        assert ld.rounds(tmp_path) == [tmp_path]

    def test_round_directories_come_back_in_run_order(self, tmp_path):
        for index in (2, 10, 1):
            (tmp_path / f"round-{index}").mkdir()
        assert [p.name for p in ld.rounds(tmp_path)] == [
            "round-1",
            "round-2",
            "round-10",
        ]

    def test_a_root_holding_both_layouts_is_refused_by_name(self, tmp_path):
        (tmp_path / "round-1").mkdir()
        (tmp_path / "kitty.kitten").write_text("")
        with pytest.raises(ValueError) as caught:
            ld.rounds(tmp_path)
        assert "kitty.kitten" in str(caught.value)

    def test_a_run_log_does_not_make_a_rounds_root_mixed(self, tmp_path):
        (tmp_path / "round-1").mkdir()
        (tmp_path / "run.log").write_text("narration\n")
        assert [p.name for p in ld.rounds(tmp_path)] == ["round-1"]

    def kitten_rounds(self, d: Path, rates: dict[str, list[float]]) -> None:
        for index in range(1, 1 + max(len(v) for v in rates.values())):
            (d / f"round-{index}").mkdir()
            for name, values in rates.items():
                if index <= len(values):
                    (d / f"round-{index}" / f"{name}.kitten").write_text(
                        f"ascii : 1s @ {values[index - 1]} MB/s\n"
                    )

    def test_a_bar_is_the_median_over_the_rounds_that_produced_one(self, tmp_path):
        self.kitten_rounds(tmp_path, {"felis": [100.0, 140.0, 120.0], "kitty": [90.0]})
        (suite,) = ld.load_kittenbench(tmp_path)
        assert suite.rounds == 3
        felis = suite.data["felis"]["ascii"]
        assert (felis.value, felis.lo, felis.hi, felis.rounds) == (120, 100, 140, 3)
        assert suite.data["kitty"]["ascii"].rounds == 1

    def test_a_mismatch_in_one_round_only_is_reported_with_its_round(self, tmp_path):
        self.kitten_rounds(tmp_path, {"felis": [100.0, 140.0], "kitty": [90.0, 95.0]})
        for index in (1, 2):
            for name in ("felis", "kitty"):
                (tmp_path / f"round-{index}" / f"{name}.size").write_text(
                    "40 120 960 640\n"
                )
        (tmp_path / "round-2" / "grid-mismatch").write_text(
            "kitty: tty pinned, window unpinned (niri)\n"
        )
        (note,) = ld.grid_note(tmp_path)
        assert "Every terminal ran at the same grid: 120x40." in note
        assert "round 2: kitty: tty pinned, window unpinned (niri)" in note

    def test_a_leg_that_changed_grid_between_rounds_is_listed_per_round(self, tmp_path):
        for index, cols in ((1, 120), (2, 100)):
            (tmp_path / f"round-{index}").mkdir()
            (tmp_path / f"round-{index}" / "felis.size").write_text("40 120 960 640\n")
            (tmp_path / f"round-{index}" / "kitty.size").write_text(
                f"40 {cols} {cols * 8} 640\n"
            )
        (note,) = ld.grid_note(tmp_path)
        assert "GRIDS DIFFERED" in note
        assert "kitty round 2 100x40" in note

    def test_meta_json_is_found_from_inside_a_round_directory(self, tmp_path):
        (tmp_path / "meta.json").write_text(json.dumps({"params": {"CAT_MB": "8"}}))
        (tmp_path / "cat" / "round-1").mkdir(parents=True)
        assert ld.run_params(tmp_path / "cat" / "round-1").get("CAT_MB") == "8"


class TestRotation:
    def test_every_leg_moves_in_every_later_round(self):
        legs = ["felis", "kitty", "alacritty", "wezterm", "foot"]
        for index in range(1, len(legs)):
            rotated = field.rotate(legs, index)
            assert sorted(rotated) == sorted(legs)
            assert not [a for a, b in zip(legs, rotated, strict=True) if a == b], (
                f"round {index + 1} left a terminal in place"
            )

    def test_the_first_round_is_chart_order(self):
        legs = ["felis", "kitty"]
        assert field.rotate(legs, 0) == legs

    def test_a_suite_iterates_the_order_it_was_handed(self):
        fld = field.Field(
            {"kitty": "/bin/true", "foot": "/bin/true"},
            "/bin/true",
            field.Presentation(family="Menlo", pt=9.0),
            leg_order=["foot", "felis", "kitty"],
        )
        assert fld.legs() == ["foot", "felis", "kitty"]


class TestResumePreflight:
    def test_a_rounds_root_is_refused_for_a_single_round_run(self, tmp_path):
        (tmp_path / "kittenbench" / "round-1").mkdir(parents=True)
        (tmp_path / "kittenbench" / "round-2").mkdir()
        with pytest.raises(ValueError) as caught:
            ct.check_resume(tmp_path, 1)
        assert "2 round directories" in str(caught.value)
        assert "ROUNDS=1" in str(caught.value)

    def test_a_single_round_root_is_refused_for_a_repeated_run(self, tmp_path):
        (tmp_path / "kittenbench").mkdir(parents=True)
        (tmp_path / "kittenbench" / "kitty.kitten").write_text("")
        with pytest.raises(ValueError) as caught:
            ct.check_resume(tmp_path, 3)
        assert "ROUNDS=1" in str(caught.value)
        assert "ROUNDS=3" in str(caught.value)

    def test_round_directories_without_a_recorded_count_are_their_own_count(
        self, tmp_path
    ):
        # No meta.json to state it, so the directories are the record:
        # ROUNDS=3 against two of them would leave a round nobody ran.
        (tmp_path / "kittenbench" / "round-1").mkdir(parents=True)
        (tmp_path / "kittenbench" / "round-2").mkdir()
        with pytest.raises(ValueError) as caught:
            ct.check_resume(tmp_path, 3)
        assert "ROUNDS=2" in str(caught.value)
        assert "ROUNDS=3" in str(caught.value)
        ct.check_resume(tmp_path, 2)

    def test_a_partly_finished_run_resumes_at_its_recorded_count(self, tmp_path):
        (tmp_path / "meta.json").write_text(json.dumps({"params": {"ROUNDS": "3"}}))
        (tmp_path / "kittenbench" / "round-1").mkdir(parents=True)
        ct.check_resume(tmp_path, 3)

    def test_a_recorded_round_count_must_match_the_requested_one(self, tmp_path):
        (tmp_path / "meta.json").write_text(json.dumps({"params": {"ROUNDS": "3"}}))
        with pytest.raises(ValueError) as caught:
            ct.check_resume(tmp_path, 2)
        assert "ROUNDS=3" in str(caught.value)
        assert "ROUNDS=2" in str(caught.value)
        ct.check_resume(tmp_path, 3)

    def test_a_root_written_before_rounds_existed_resumes_as_one_round(self, tmp_path):
        (tmp_path / "meta.json").write_text(json.dumps({"params": {"CAT_MB": "8"}}))
        (tmp_path / "cat").mkdir()
        (tmp_path / "cat" / "kitty.json").write_text("{}")
        ct.check_resume(tmp_path, 1)

    def test_the_refusal_lands_before_a_window_opens(self, tmp_path, monkeypatch):
        root = tmp_path / "root"
        (root / "kittenbench" / "round-1").mkdir(parents=True)
        felis_bin = tmp_path / "felis"
        felis_bin.write_text("#!/bin/sh\n")
        felis_bin.chmod(0o755)
        args = ct.parse_args(
            [
                "run",
                "kittenbench",
                "--out",
                str(root),
                "--felis-bin",
                str(felis_bin),
                "--use-path",
            ]
        )

        def opened(*_args, **_kwargs):
            raise AssertionError("a window was opened before the preflight")

        tools = {
            name: ct.Tool(name, "/bin/true", False)
            for name in ("kitten", "kitty", *ct.TERMINALS)
        }
        monkeypatch.setattr(ct, "resolve_field", lambda *_a, **_k: tools)
        monkeypatch.setattr(ct.wm, "select", lambda *_a: wm.FloatingPin())
        monkeypatch.setattr(ct, "cached_grid", opened)
        monkeypatch.setattr(suites, "probe_grid", opened)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = ct.cmd_run(args, tmp_path)
        assert code == 2
        assert "ROUNDS=1" in err.getvalue()

    def test_the_order_file_records_the_run_that_measured_the_legs(
        self, tmp_path, monkeypatch
    ):
        # A resume skips the done legs, so rewriting the file would
        # replace the order they ran in with one nothing was measured
        # under — and the environment note reads that order.
        results = tmp_path / "cat"
        pres = field.Presentation(family="Menlo", pt=9.0)
        first = field.Field({"kitty": "/bin/true"}, "/bin/true", pres)
        resumed = field.Field(
            {"kitty": "/bin/true"},
            "/bin/true",
            pres,
            leg_order=["kitty", "felis"],
        )
        monkeypatch.setitem(suites.SUITES, "cat", lambda *_args: True)
        with contextlib.redirect_stdout(io.StringIO()):
            ct.run_suite("cat", first, results, {})
            ct.run_suite("cat", resumed, results, {})
        assert (results / "order").read_text() == "felis\nkitty\n"

    def test_one_round_writes_the_suite_directory_itself(self):
        root = Path("/results")
        assert ct.suite_dir(root, "cat", 1, 1) == root / "cat"
        assert ct.suite_dir(root, "cat", 2, 3) == root / "cat" / "round-2"

    def test_rounds_default_to_one_and_read_a_count(self):
        assert ct.requested_rounds({}) == 1
        assert ct.requested_rounds({"ROUNDS": "3"}) == 3

    @pytest.mark.parametrize("bad", ["0", "-1", "two", "2.5"])
    def test_rounds_that_are_not_a_count_are_refused(self, bad):
        with pytest.raises(ValueError):
            ct.requested_rounds({"ROUNDS": bad})


class TestLegEnvironment:
    def write(self, directory: Path, name: str, before: dict, after: dict) -> None:
        (directory / f"{name}.env.json").write_text(
            json.dumps({"before": before, "after": after})
        )

    def linux(self, load: float, events: int) -> dict:
        return {"loadavg": load, "throttle": {"kind": "counters", "events": events}}

    def darwin(self, load: float, limit: int) -> dict:
        return {
            "loadavg": load,
            "throttle": {"kind": "speed_limit", "cpu_speed_limit": limit},
        }

    def test_a_leg_taken_under_more_load_than_the_first_one_is_named(self, tmp_path):
        (tmp_path / "order").write_text("felis\nkitty\n")
        self.write(tmp_path, "felis", self.linux(0.4, 0), self.linux(0.5, 0))
        self.write(tmp_path, "kitty", self.linux(0.6, 0), self.linux(2.1, 0))
        (note,) = ld.env_notes(tmp_path)
        assert "kitty 2.1" in note
        assert "felis" not in note

    def test_load_within_one_of_the_first_leg_says_nothing(self, tmp_path):
        self.write(tmp_path, "felis", self.linux(0.4, 0), self.linux(0.5, 0))
        self.write(tmp_path, "kitty", self.linux(1.3, 0), self.linux(1.4, 0))
        assert ld.env_notes(tmp_path) == []

    def test_a_linux_throttle_counter_that_advanced_is_reported_as_events(
        self, tmp_path
    ):
        self.write(tmp_path, "felis", self.linux(0.4, 7), self.linux(0.4, 7))
        self.write(tmp_path, "kitty", self.linux(0.4, 7), self.linux(0.4, 9))
        (note,) = ld.env_notes(tmp_path)
        assert note == "The CPU was throttled during: kitty (+2 throttling events)."

    def test_a_macos_speed_limit_below_100_at_either_end_is_reported(self, tmp_path):
        self.write(tmp_path, "felis", self.darwin(0.4, 100), self.darwin(0.4, 100))
        self.write(tmp_path, "kitty", self.darwin(0.4, 100), self.darwin(0.4, 70))
        (note,) = ld.env_notes(tmp_path)
        assert "kitty (CPU speed limit 70%)" in note

    def test_a_machine_with_no_throttle_reading_claims_nothing(self, tmp_path):
        unavailable = {"loadavg": 0.4, "throttle": "unavailable"}
        self.write(tmp_path, "felis", unavailable, unavailable)
        assert ld.env_notes(tmp_path) == []

    def test_the_baseline_is_the_first_leg_of_the_first_round(self, tmp_path):
        for index in (1, 2):
            (tmp_path / f"round-{index}").mkdir()
        (tmp_path / "round-1" / "order").write_text("kitty\nfelis\n")
        self.write(
            tmp_path / "round-1", "kitty", self.linux(0.2, 0), self.linux(0.3, 0)
        )
        self.write(
            tmp_path / "round-1", "felis", self.linux(0.3, 0), self.linux(0.3, 0)
        )
        self.write(
            tmp_path / "round-2", "felis", self.linux(1.9, 0), self.linux(2.0, 0)
        )
        (note,) = ld.env_notes(tmp_path)
        assert "was 0.2 at the first leg on record" in note
        assert "felis round 2 2" in note

    def test_the_baseline_is_the_load_the_run_started_under(self, tmp_path):
        # A background job that arrives during an earlier suite is
        # already in this suite's first leg, so a per-suite baseline
        # would take the raised load for the new normal.
        (tmp_path / "meta.json").write_text(
            json.dumps({"machine": {"load_average": 0.3}})
        )
        suite = tmp_path / "cat"
        suite.mkdir()
        (suite / "order").write_text("felis\nkitty\n")
        self.write(suite, "felis", self.linux(2.0, 0), self.linux(2.1, 0))
        self.write(suite, "kitty", self.linux(2.2, 0), self.linux(2.3, 0))
        (note,) = ld.env_notes(suite)
        assert "was 0.3 when the run started" in note
        assert "felis 2.1" in note
        assert "kitty 2.3" in note

    def test_a_refused_leg_is_named_with_its_reason(self, tmp_path):
        (tmp_path / "order").write_text("felis\nkitty\n")
        self.write(tmp_path, "felis", self.linux(0.4, 0), self.linux(0.4, 0))
        (tmp_path / "kitty.env.json").write_text(
            json.dumps(
                {
                    "before": self.linux(0.4, 0),
                    "refused": "the screen was locked before the leg",
                }
            )
        )
        (note,) = ld.env_notes(tmp_path)
        assert "kitty (the screen was locked before the leg)" in note
        assert "felis" not in note

    def test_a_missing_throttle_file_is_unavailable_rather_than_zero(self, tmp_path):
        assert envinfo.linux_throttle(tmp_path) == "unavailable"
        cpu = tmp_path / "cpu0" / "thermal_throttle"
        cpu.mkdir(parents=True)
        (cpu / "core_throttle_count").write_text("3\n")
        (cpu / "package_throttle_count").write_text("1\n")
        assert envinfo.linux_throttle(tmp_path) == {"kind": "counters", "events": 4}


class FakeWatcher:
    """Maps a window for the pids it is told to, 50 ms after being asked."""

    name = "fake"

    def __init__(self, maps: set[int]) -> None:
        self.maps = maps
        self.waits: list[int] = []

    def settle(self):
        pass

    def wait(self, pid, _timeout):
        self.waits.append(pid)
        return time.monotonic() + 0.05 if pid in self.maps else None

    def close(self):
        pass


KITTY_PID, FELIS_PID = 100, 200


class FakeHeld:
    """A held process that exits on the first signal it is sent."""

    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.signals: list[int] = []

    def alive(self) -> bool:
        return not self.signals

    def send(self, sig: int) -> None:
        self.signals.append(sig)

    def close(self) -> None:
        pass


class FakeDaemon:
    """The bench socket's daemon as macOS runs it: a child of the launch.

    A felis launch autospawns one when none is up, and stopping the
    launch kills it unless it is spared. `stubborn` answers SIGTERM by
    leaving a daemon on the socket anyway.
    """

    def __init__(self, stubborn: bool = False) -> None:
        self.pid: int | None = None
        self.spawned = 0
        self.stubborn = stubborn
        self.spared: list[list[int]] = []
        self.escalations: list[bool] = []

    def launch(self, *_a, **_k):
        if self.pid is None:
            self.spawned += 1
            self.pid = 1000 + self.spawned
        return mock.Mock(pid=FELIS_PID)

    def stop(self, proc, spare=()):
        if proc.pid == FELIS_PID:
            self.spared.append(list(spare))
            if self.pid not in spare:
                self.pid = None

    def hold(self, _socket):
        return None if self.pid is None else mock.Mock(pid=self.pid)

    def terminate(self, roots, escalate=True):
        self.escalations.append(escalate)
        if [r.pid for r in roots] == [self.pid] and not self.stubborn:
            self.pid = None

    def bound(self, _socket):
        return self.pid


@pytest.mark.usefixtures("presenting")
class TestStartupSuite:
    """`startup` never opens a leg through `open_leg`, so it owns its own marker."""

    @pytest.fixture
    def drive(self, monkeypatch, tmp_path_factory):
        def drive(
            results: Path,
            maps: set[int],
            runs=2,
            warmup=1,
            daemon: FakeDaemon | None = None,
            err: io.StringIO | None = None,
        ) -> FakeWatcher:
            watcher = FakeWatcher(maps)
            daemon = daemon or FakeDaemon()
            fld = field.Field(
                {"kitty": "/bin/true"},
                "/bin/true",
                field.Presentation(family="Menlo", pt=9.0),
                leg_order=["kitty", "felis"],
            )
            monkeypatch.setattr(suites.firstwindow, "select", lambda: watcher)
            monkeypatch.setattr(
                suites.fieldmod,
                "launch_terminal",
                lambda *_a: mock.Mock(pid=KITTY_PID),
            )
            monkeypatch.setattr(suites.subprocess, "Popen", daemon.launch)
            monkeypatch.setattr(suites, "stop_launch", daemon.stop)
            monkeypatch.setattr(suites.fieldmod, "hold_felis_daemon", daemon.hold)
            monkeypatch.setattr(suites.fieldmod, "stop", daemon.terminate)
            monkeypatch.setattr(suites, "felis_kill_sessions", lambda *_a: None)
            monkeypatch.setattr(
                suites.fieldmod,
                "felis_home",
                lambda _pres: (
                    tmp_path_factory.mktemp("felis-home"),
                    tmp_path_factory.mktemp("felis-socket"),
                ),
            )
            monkeypatch.setattr(suites.fieldmod, "felis_daemon_pid", daemon.bound)
            with (
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(err or io.StringIO()),
            ):
                suites.suite_startup(
                    fld, results, {"RUNS": str(runs), "WARMUP": str(warmup)}
                )
            return watcher

        return drive

    def test_warmup_launches_are_discarded(self, tmp_path, drive):
        results = tmp_path
        drive(results, {KITTY_PID, FELIS_PID}, runs=3, warmup=2)
        record = json.loads((results / "kitty.startup.json").read_text())
        assert len(record["samples_ms"]) == 3
        assert "failed" not in record
        assert record["samples_ms"][0] > 0

    def test_a_measured_leg_leaves_a_marker_and_is_not_run_twice(self, tmp_path, drive):
        results = tmp_path
        drive(results, {KITTY_PID, FELIS_PID})
        assert (results / "kitty.done").exists()
        assert (results / "felis.done").exists()
        assert drive(results, {KITTY_PID}).waits == []

    def test_felis_writes_a_cold_and_a_warm_row(self, tmp_path, drive):
        results = tmp_path
        drive(results, {KITTY_PID, FELIS_PID})
        for row in ("felis-cold", "felis-warm"):
            record = json.loads((results / f"{row}.startup.json").read_text())
            assert len(record["samples_ms"]) == 2

    def test_every_warm_launch_finds_the_daemon_the_first_one_spawned(
        self, tmp_path, drive
    ):
        results = tmp_path
        daemon = FakeDaemon()
        drive(results, {KITTY_PID, FELIS_PID}, runs=2, warmup=1, daemon=daemon)
        warm = json.loads((results / "felis-warm.startup.json").read_text())
        cold = json.loads((results / "felis-cold.startup.json").read_text())
        assert "failed" not in warm
        assert warm["daemon_checked"] == 3
        assert cold["daemon_checked"] == 3
        # Three cold launches, then the one that primed the warm row.
        assert daemon.spawned == 4
        # The priming launch and all three warm ones spare that daemon.
        assert daemon.spared[-4:] == [[1004]] * 4

    def test_a_warm_row_whose_daemon_died_with_its_launch_fails(self, tmp_path, drive):
        # What happened before the daemon was spared: on macOS the stop
        # killed it, and every "warm" launch spawned a fresh one.
        class Unspared(FakeDaemon):
            def stop(self, proc, spare=()):
                super().stop(proc, ())

        results = tmp_path
        drive(results, {KITTY_PID, FELIS_PID}, daemon=Unspared())
        warm = json.loads((results / "felis-warm.startup.json").read_text())
        assert "warm daemon" in warm["failed"]
        assert "before launch 1 of 3" in warm["failed"]
        assert warm["samples_ms"] == []

    def test_a_cold_launch_that_would_find_a_daemon_fails(self, tmp_path, drive):
        results = tmp_path
        daemon = FakeDaemon(stubborn=True)
        daemon.pid = 999
        drive(results, {KITTY_PID, FELIS_PID}, daemon=daemon)
        cold = json.loads((results / "felis-cold.startup.json").read_text())
        assert "survived SIGTERM" in cold["failed"]
        # A daemon that ignores SIGTERM is a finding, not a process
        # to SIGKILL out of the way.
        assert True not in daemon.escalations
        # Nor is the survivor mistaken for a daemon the warm row primed.
        warm = json.loads((results / "felis-warm.startup.json").read_text())
        assert "before the priming launch" in warm["failed"]

    def test_a_leg_whose_display_cannot_present_is_never_launched(
        self, tmp_path, drive, monkeypatch
    ):
        results = tmp_path
        asleep = {"locked": False, "asleep": True}
        err = io.StringIO()
        monkeypatch.setattr(envinfo, "display_state", lambda: asleep)
        watcher = drive(results, {KITTY_PID, FELIS_PID}, err=err)
        assert watcher.waits == []
        assert "REFUSED: felis" in err.getvalue()
        for name in ("kitty", "felis"):
            assert not (results / f"{name}.done").exists()
            record = json.loads((results / f"{name}.env.json").read_text())
            assert record["refused"] == "the display was asleep before the leg"

    def test_a_result_a_crashed_attempt_left_is_not_charted_beside_a_refusal(
        self, tmp_path, drive, monkeypatch
    ):
        results = tmp_path
        (results / "kitty.startup.json").write_text("{}")
        (results / "felis-warm.startup.json").write_text("{}")
        locked = {"locked": True, "asleep": False}
        monkeypatch.setattr(envinfo, "display_state", lambda: locked)
        drive(results, {KITTY_PID, FELIS_PID})
        assert ld.collect(results, "startup") == []
        assert list(results.glob("*.startup.json")) == []

    def test_a_priming_launch_that_raises_leaves_no_earlier_warm_row(
        self, tmp_path, drive
    ):
        class Unspawnable(FakeDaemon):
            def launch(self, *a, **k):
                if self.spawned == 3:
                    raise OSError("fork failed")
                return super().launch(*a, **k)

        results = tmp_path
        (results / "felis-warm.startup.json").write_text(
            json.dumps({"samples_ms": [5.0, 6.0]})
        )
        with pytest.raises(OSError):
            drive(results, {KITTY_PID, FELIS_PID}, daemon=Unspawnable())
        (startup,) = ld.collect(results, "startup")
        assert sorted(startup.data) == ["kitty"]
        assert not (results / "felis.done").exists()

    def test_a_leg_that_ended_unable_to_present_lands_in_refused(
        self, tmp_path, drive, monkeypatch
    ):
        # kitty runs first: before, after; then felis: before, after.
        states = iter(
            [PRESENTING, {"locked": True, "asleep": False}, PRESENTING, PRESENTING]
        )
        results = tmp_path
        monkeypatch.setattr(envinfo, "display_state", lambda: next(states))
        drive(results, {KITTY_PID, FELIS_PID})
        assert not (results / "kitty.startup.json").exists()
        assert (results / "refused" / "kitty.startup.json").exists()
        assert not (results / "kitty.done").exists()
        assert (results / "felis-warm.startup.json").exists()
        assert (results / "felis.done").exists()

    def test_stopping_a_launch_leaves_the_spared_process_alone(self, monkeypatch):
        tree = {10: [20], 20: [30]}
        held = {pid: FakeHeld(pid) for pid in (10, 20, 30)}
        monkeypatch.setattr(
            suites.fieldmod,
            "held_children",
            lambda parent, known: [
                held[pid]
                for pid in tree.get(parent.pid, [])
                if held[pid].alive() and pid not in known
            ],
        )
        monkeypatch.setattr(suites.time, "sleep", lambda _s: None)
        monkeypatch.setattr(suites.fieldmod.Held, "child", lambda _p: held[10])
        suites.stop_launch(mock.Mock(pid=10), spare=[20])
        assert held[20].signals == []
        assert held[10].signals == [signal.SIGTERM]
        assert held[30].signals == [signal.SIGTERM]

    def test_a_terminal_that_never_maps_a_window_is_a_failure_not_a_number(
        self, tmp_path, drive
    ):
        results = tmp_path
        watcher = drive(results, {FELIS_PID})
        record = json.loads((results / "kitty.startup.json").read_text())
        assert "no window" in record["failed"]
        assert record["samples_ms"] == []
        # The first silent launch ends the leg rather than timing out
        # once per run.
        assert watcher.waits.count(KITTY_PID) == 1
        # Recorded, so a resume does not spend the timeout again.
        assert (results / "kitty.done").exists()

    def test_each_leg_records_the_machine_it_ran_on(self, tmp_path, drive):
        results = tmp_path
        drive(results, {KITTY_PID})
        record = json.loads((results / "kitty.env.json").read_text())
        assert "loadavg" in record["before"]
        assert "loadavg" in record["after"]

    def test_the_felis_roster_is_read_for_its_ids(self):
        roster = json.dumps({"sessions": [{"id": "ab12"}, {"id": "cd34"}]})
        assert suites.session_ids(roster) == ["ab12", "cd34"]
        assert suites.session_ids("not json") == []


class TestFirstWindow:
    def test_the_roster_niri_sends_first_replaces_what_was_known(self):
        windows, full = firstwindow.niri_windows(
            {"WindowsChanged": {"windows": [{"id": 1, "pid": 5}]}}
        )
        assert full
        assert [w["id"] for w in windows] == [1]

    def test_an_opened_window_is_one_window(self):
        windows, full = firstwindow.niri_windows(
            {"WindowOpenedOrChanged": {"window": {"id": 7, "pid": 9}}}
        )
        assert not full
        assert windows == [{"id": 7, "pid": 9}]

    def test_other_events_name_no_window(self):
        assert firstwindow.niri_windows({"WorkspaceActivated": {"id": 1}}) == (
            [],
            False,
        )

    def test_a_window_is_the_launch_s_when_its_pid_is_in_the_tree(self, monkeypatch):
        monkeypatch.setattr(firstwindow.field, "process_tree", lambda _pid: {10, 11})
        mine = firstwindow.TreeMatcher(10)
        assert mine(10)
        assert mine(11)
        assert not mine(12)
        assert not mine(None)


class TestExplicitGrid:
    def test_an_explicit_pair_needs_a_pin_that_can_resize(self):
        # felis has no size flag, so under a floating desktop the
        # request would reach every terminal except the one the rest are
        # being compared against.
        with pytest.raises(ValueError):
            ct.explicit_grid({"GRID_ROWS": "40", "GRID_COLS": "120"}, wm.FloatingPin())

    def test_half_a_pair_is_a_usage_error(self):
        with pytest.raises(ValueError):
            ct.explicit_grid({"GRID_ROWS": "40"}, wm.NiriPin())

    def test_neither_leaves_the_reference_in_charge(self):
        assert ct.explicit_grid({}, wm.FloatingPin()) is None

#!/usr/bin/env python3
"""Unit tests for the cross-terminal suite (loaders.py, report.py, envinfo.py).

Hermetic — synthetic harness artifacts and recorded command output, no
terminal is launched and nothing is shelled out — so a parser, scale or
provenance break is caught here rather than after a 30-minute suite run
has produced a wrong-looking chart.

Run: python3 tools/bench/crossterm_test.py
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
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
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


def presenting(cls):
    """Sample every leg's machine from fixtures: presentable, unthrottled."""
    cls = mock.patch.object(envinfo, "display_state", lambda: dict(PRESENTING))(cls)
    return mock.patch.object(envinfo, "throttle_state", lambda: "unavailable")(cls)


class VtebenchLoaderTest(unittest.TestCase):
    def test_mean_and_stddev_per_benchmark(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.dat").write_text("scrolling unicode\n10 100\n20 120\n30 140\n")
            (suite,) = ld.load_vtebench(d)
            point = suite.data["felis"]["scrolling"]
            self.assertAlmostEqual(point.value, 20.0)
            self.assertAlmostEqual(point.err, 8.16496580927726)
            self.assertAlmostEqual(suite.data["felis"]["unicode"].value, 120.0)

    def test_underscore_samples_are_dropped_not_zeroed(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "kitty.dat").write_text("a b\n10 _\n30 _\n")
            (suite,) = ld.load_vtebench(d)
            self.assertAlmostEqual(suite.data["kitty"]["a"].value, 20.0)
            self.assertNotIn("b", suite.data["kitty"])
            # A benchmark no terminal produced must not leave an empty row.
            self.assertEqual(suite.categories, ["a"])

    def test_no_dat_files_yields_no_suite(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(ld.load_vtebench(Path(tmp)), [])


class TermbenchLoaderTest(unittest.TestCase):
    def test_stalled_category_keeps_the_marker(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "ghostty.json").write_text(
                json.dumps(
                    [
                        {"name": "binary", "MB/s": "stalled"},
                        {"name": "many_lines", "MB/s": 512.5},
                    ]
                )
            )
            (suite,) = ld.load_termbench(d)
            self.assertEqual(suite.data["ghostty"]["binary"].value, 0.0)
            self.assertEqual(suite.data["ghostty"]["binary"].note, "stalled")
            self.assertEqual(suite.data["ghostty"]["many_lines"].value, 512.5)


class CatLoaderTest(unittest.TestCase):
    def test_one_bar_per_payload_named_by_hyperfine(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.json").write_text(
                json.dumps(
                    {
                        "results": [
                            {"command": "ascii", "mean": 1.25, "stddev": 0.03},
                            {"command": "unicode", "mean": 2.5, "stddev": 0.1},
                        ]
                    }
                )
            )
            (suite,) = ld.load_cat(d)
            self.assertEqual(suite.categories, ["ascii", "unicode"])
            self.assertAlmostEqual(suite.data["felis"]["ascii"].value, 1.25)
            self.assertAlmostEqual(suite.data["felis"]["unicode"].err, 0.1)

    def test_the_pty_drain_caveat_travels_with_the_chart(self):
        # The number is not parse time, and a reader who does not know
        # that will read the cat and kittenbench charts as the same
        # measurement taken twice.
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "kitty.json").write_text(
                json.dumps({"results": [{"command": "ascii", "mean": 1.0}]})
            )
            (suite,) = ld.load_cat(d)
            self.assertTrue(any("kittenbench" in n for n in suite.notes))


class KittenbenchLoaderTest(unittest.TestCase):
    def test_rate_survives_the_color_kitten_prints_it_in(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.kitten").write_text(
                "  Only ASCII chars : 1.234s    @ \x1b[32m812.5  \x1b[m MB/s\n"
                "These results measure the time it takes to parse.\n"
                "  Unicode chars    : 2.5s      @ \x1b[32m120.25 \x1b[m MB/s\n"
            )
            (suite,) = ld.load_kittenbench(d)
            self.assertEqual(suite.categories, ["Only ASCII chars", "Unicode chars"])
            self.assertAlmostEqual(suite.data["felis"]["Only ASCII chars"].value, 812.5)

    def test_prose_lines_are_not_benchmarks(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "kitty.kitten").write_text(
                "Note that rendering is suppressed, so MB/s is parse only.\n"
            )
            self.assertEqual(ld.load_kittenbench(d), [])


class DoomFireLoaderTest(unittest.TestCase):
    def test_fps_and_the_byte_rate_it_implies(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.doom-fire").write_text(
                "doom-fire frames 2827 secs 20.000 fps 141.350 bytes_avg 1048576\n"
            )
            (suite,) = ld.load_doom_fire(d)
            self.assertAlmostEqual(suite.data["felis"]["DOOM-fire"].value, 141.35)
            self.assertTrue(any("141 MB/s" in n for n in suite.notes))

    def test_a_run_that_never_reported_is_left_out(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "wezterm.doom-fire").write_text("")
            self.assertEqual(ld.load_doom_fire(d), [])


class PayloadTest(unittest.TestCase):
    def test_the_same_seed_gives_the_same_bytes(self):
        # The whole point of generating rather than downloading: two runs
        # on two machines must flood the terminals with identical files.
        import payloads

        with tempfile.TemporaryDirectory() as tmp:
            a = payloads.build("csi", 1, Path(tmp) / "a")
            b = payloads.build("csi", 1, Path(tmp) / "b")
            self.assertEqual(a.read_bytes(), b.read_bytes())
            self.assertEqual(payloads.digest(a), payloads.digest(b))

    def test_size_is_exact_and_kinds_differ(self):
        import payloads

        with tempfile.TemporaryDirectory() as tmp:
            built = {k: payloads.build(k, 1, Path(tmp)) for k in payloads.KINDS}
            for path in built.values():
                self.assertEqual(path.stat().st_size, 1 << 20)
            self.assertEqual(len({p.read_bytes() for p in built.values()}), 3)

    def test_a_cached_payload_of_the_wrong_size_is_rebuilt(self):
        import payloads

        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            stale = d / "ascii-1mb.bin"
            d.mkdir(exist_ok=True)
            stale.write_bytes(b"truncated")
            self.assertEqual(payloads.build("ascii", 1, d).stat().st_size, 1 << 20)


class StartupLoaderTest(unittest.TestCase):
    def test_felis_legs_fold_into_one_terminal(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            for name, mean in (
                ("felis-cold", 0.2),
                ("felis-warm", 0.08),
                ("kitty", 0.14),
            ):
                (d / f"{name}.json").write_text(
                    json.dumps({"results": [{"mean": mean, "stddev": 0.01}]})
                )
            (suite,) = ld.load_startup(d)
            self.assertEqual(suite.terminals(), ["felis", ld.WARM, "kitty"])
            self.assertEqual(suite.categories, ["launch"])
            self.assertAlmostEqual(suite.data["felis"]["launch"].value, 200.0)
            self.assertAlmostEqual(suite.data[ld.WARM]["launch"].value, 80.0)
            self.assertEqual(suite.label("felis"), "felis (cold)")

    def test_felis_warm_is_never_the_best_other_terminal(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            for name, mean in (
                ("felis-cold", 0.2),
                ("felis-warm", 0.08),
                ("kitty", 0.3),
            ):
                (d / f"{name}.json").write_text(
                    json.dumps({"results": [{"mean": mean}]})
                )
            (suite,) = ld.load_startup(d)
            for term in suite.samples:
                for per_round in suite.samples[term].values():
                    per_round[2] = per_round[1]
            suite.rounds = 2
            self.assertEqual(report.ratio(suite, "launch")[0], "1.50x")

    def test_tiler_timing_caveat_is_in_the_subtitle(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "kitty.json").write_text(json.dumps({"results": [{"mean": 0.1}]}))
            (suite,) = ld.load_startup(d)
            self.assertIn("animation completion is not timed", suite.subtitle)
            self.assertIn("work delaying first configure is included", suite.subtitle)


class FirstWindowLoaderTest(unittest.TestCase):
    @staticmethod
    def write(directory: Path, stem: str, samples, **extra) -> None:
        payload = {"watcher": "niri", "samples_ms": samples} | extra
        (directory / f"{stem}.startup.json").write_text(json.dumps(payload))

    def test_the_bar_is_the_median_launch(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "kitty", [100.0, 110.0, 300.0, 120.0, 105.0])
            self.write(d, "felis-cold", [80.0, 90.0])
            self.write(d, "felis-warm", [40.0, 50.0])
            (suite,) = ld.load_startup(d)
            self.assertEqual(suite.terminals(), ["felis", ld.WARM, "kitty"])
            self.assertEqual(suite.categories, ["first window"])
            self.assertAlmostEqual(suite.data["kitty"]["first window"].value, 110.0)
            self.assertAlmostEqual(suite.data[ld.WARM]["first window"].value, 45.0)
            self.assertIn("first window", suite.title)

    def test_a_terminal_that_showed_no_window_has_no_bar_and_says_why(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "kitty", [100.0])
            self.write(d, "foot", [], failed="launch 1 of 13 put no window on screen")
            (suite,) = ld.load_startup(d)
            self.assertNotIn("foot", suite.data)
            self.assertIn("foot: launch 1 of 13", " ".join(suite.notes))

    def test_macos_is_not_claimed_to_prove_a_drawn_frame(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "kitty", [100.0])
            (suite,) = ld.load_startup(d)
            self.assertIn("does not prove", suite.subtitle)
            self.assertIn("first buffer committed", suite.subtitle)

    def test_an_older_root_s_lifetimes_are_labelled_as_such(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "kitty.json").write_text(json.dumps({"results": [{"mean": 0.1}]}))
            (suite,) = ld.load_startup(d)
            self.assertIn("process lifetime", suite.subtitle)


class MemoryLoaderTest(unittest.TestCase):
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

    def test_memory_in_mb_and_cpu_in_seconds(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "felis")
            rss, cpu = ld.load_memory(d)
            self.assertAlmostEqual(rss.data["felis"]["idle"].value, 100.0)
            self.assertAlmostEqual(
                rss.data["felis"]["after 200k-line flood"].value, 200.0
            )
            # 4.12, not 5.12: the CPU already spent when the flood began
            # is startup and the idle wait, which the flood did not do
            # and which the field does not spend equally.
            self.assertAlmostEqual(cpu.data["felis"]["200k-line flood"].value, 4.12)

    def test_the_metric_is_named_on_the_chart_it_was_drawn_for(self):
        # Two runs under different accountings do not belong on one axis,
        # so the axis has to say which one it is.
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "felis", metric="phys_footprint")
            rss, _cpu = ld.load_memory(d)
            self.assertIn("Physical footprint", rss.subtitle)

    def test_the_peak_says_it_is_not_the_metric_above_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "felis", peak_kb=307200)
            rss, _cpu = ld.load_memory(d)
            note = " ".join(rss.notes)
            self.assertIn("felis 300", note)
            self.assertIn("VmHWM", note)

    def test_a_run_from_before_the_metric_changed_still_charts(self):
        # Dropping it would read as a suite that was never run, and the
        # bars are still the numbers that run took.
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.mem").write_text("102400 204800 412\n")
            (rss,) = ld.load_memory(d)
            self.assertAlmostEqual(rss.data["felis"]["idle"].value, 100.0)
            self.assertIn("superseded", rss.subtitle)

    def test_the_memory_note_names_the_ledger_not_a_decomposition(self):
        # The note claims only what the ledger defines: in vs. out of
        # the OS's own charge to the process. It must not claim a
        # breakdown (heap, mapping, atlas) the harness cannot check.
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "felis")
            rss, _cpu = ld.load_memory(d)
            self.assertIn("what the OS charges to the processes", rss.subtitle)
            self.assertIn("GPU driver allocates", rss.subtitle)
            for claim in ("GPU-side", "heap-side", "outside every one"):
                self.assertNotIn(claim, rss.subtitle)

    def test_the_flood_cpu_note_says_sampled_not_all(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "felis")
            _rss, cpu = ld.load_memory(d)
            self.assertIn("utime + stime", cpu.subtitle)
            self.assertIn("named after it or its helpers", cpu.subtitle)
            self.assertNotIn("parse plus render", cpu.subtitle)


class MemorySamplerTest(unittest.TestCase):
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

    def test_pss_is_read_and_pss_anon_is_not_mistaken_for_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self.fake_proc(Path(tmp), 42, pss=1000, hwm=4000)
            sample = field.linux_sample(42, root)
            self.assertEqual(sample.metric, "pss")
            self.assertEqual(sample.kb, 1000)
            self.assertEqual(sample.peak_kb, 4000)

    def test_a_process_that_cannot_be_read_is_not_a_zero(self):
        # Summing a missing half would undercount by exactly the quantity
        # the chart is about, and nothing on the bar would say so.
        with tempfile.TemporaryDirectory() as tmp:
            self.assertIsNone(field.linux_sample(42, Path(tmp)))
            self.assertIsNone(field.memory_sample([]))

    def test_a_leg_is_the_sum_of_its_processes(self):
        client = field.MemorySample("pss", 1000, 1500)
        daemon = field.MemorySample("pss", 3000, 3500)
        summed = field.total([client, daemon])
        self.assertEqual(summed.kb, 4000)
        self.assertEqual(summed.peak_kb, 5000)
        self.assertIsNone(field.total([client, None]))

    def test_a_peak_only_some_processes_report_is_not_a_total(self):
        pair = [
            field.MemorySample("pss", 1000, 1500),
            field.MemorySample("pss", 10, None),
        ]
        self.assertIsNone(field.total(pair).peak_kb)

    def test_linux_cpu_comes_from_proc_not_whole_second_ps(self):
        # procps prints `cputime` as `00:00:04`; a flood that costs a
        # fast terminal a third of a second charts as zero through it.
        with tempfile.TemporaryDirectory() as tmp:
            proc = Path(tmp) / "42"
            proc.mkdir(parents=True)
            # A comm with a space and a `)` in it: the fields are counted
            # from the last `)`, not the first.
            proc.joinpath("stat").write_text(
                "42 (we ird) S "
                + " ".join(str(n) for n in range(4, 14))
                + " 30 70 5 5\n"
            )
            ticks = field.os.sysconf("SC_CLK_TCK")
            self.assertEqual(
                field.linux_cpu_centiseconds(42, Path(tmp)), 100 * 100 // ticks
            )

    def test_a_pid_that_cannot_be_read_is_not_folded_in_as_zero(self):
        # Summing an unreadable pid as 0 would undercount by exactly the
        # quantity the chart is about, so the leg fails the way an
        # unreadable memory sample already does (see `field.total`).
        with tempfile.TemporaryDirectory() as tmp:
            self.assertIsNone(field.linux_cpu_centiseconds(42, Path(tmp)))
        with (
            mock.patch.object(field, "DARWIN", False),
            mock.patch.object(field, "linux_cpu_centiseconds", return_value=None),
        ):
            self.assertIsNone(field.cpu_centiseconds([42]))

    def test_macos_is_sampled_through_its_own_ledger(self):
        # `ps -o rss` on macOS also loses whatever the memory compressor
        # took, so the idle bar would record what else the machine was
        # doing.
        with (
            mock.patch.object(field, "DARWIN", True),
            mock.patch.object(
                field, "darwin_sample", return_value=field.MemorySample("x", 1, None)
            ) as darwin,
        ):
            field.memory_sample([7])
        darwin.assert_called_once_with(7)

    def test_the_v4_rusage_layout_matches_the_c_struct(self):
        # A field out of place reads a neighbouring counter as the
        # footprint, which is a plausible-looking wrong number.
        self.assertEqual(field.ctypes.sizeof(field.RusageInfoV4), 296)
        self.assertEqual(field.RusageInfoV4.ri_phys_footprint.offset, 72)
        self.assertEqual(field.RusageInfoV4.ri_lifetime_max_phys_footprint.offset, 240)

    def test_macos_cpu_time_is_converted_from_mach_ticks(self):
        # Apple Silicon counts 125/3 ns per tick; read as nanoseconds the
        # same counter is 42 times too small.
        info = field.RusageInfoV4(ri_user_time=24_000_000, ri_system_time=0)
        self.assertAlmostEqual(field.rusage_cpu_seconds(info, 125 / 3), 1.0)

    def test_macos_cpu_goes_through_rusage_not_ps(self):
        info = field.RusageInfoV4(ri_user_time=1_200_000, ri_system_time=1_200_000)
        with (
            mock.patch.object(field, "DARWIN", True),
            mock.patch.object(field, "darwin_rusage", return_value=info),
            mock.patch.object(field, "mach_ns_per_tick", return_value=125 / 3),
            mock.patch.object(field.subprocess, "run") as run,
        ):
            self.assertEqual(field.cpu_centiseconds([7, 8]), 20)
        run.assert_not_called()

    def test_macos_reports_no_peak_rather_than_its_startup_peak(self):
        info = field.RusageInfoV4(
            ri_phys_footprint=50 << 20, ri_lifetime_max_phys_footprint=400 << 20
        )
        with mock.patch.object(field, "darwin_rusage", return_value=info):
            sample = field.darwin_sample(7)
        self.assertEqual(sample.kb, 50 << 10)
        self.assertIsNone(sample.peak_kb)

    def test_a_child_is_matched_by_its_executable_not_its_install_path(self):
        # macOS prints the whole path; a shell installed under a
        # directory named after the terminal is still the shell.
        self.assertTrue(field.comm_matches("wezterm-gui", "wezterm"))
        self.assertFalse(field.comm_matches("/opt/kitty-tools/bin/zsh", "kitty"))
        self.assertFalse(
            field.comm_matches("/Applications/kitty.app/bin/bash", "kitty")
        )

    def test_kitty_s_helper_counts_as_kitty(self):
        path = "/Applications/kitty.app/Contents/MacOS/kitten"
        self.assertTrue(field.comm_matches(path, "kitty"))
        self.assertTrue(field.comm_matches("kitten", "kitty"))
        self.assertFalse(field.comm_matches("kitten", "ghostty"))


class UsageSamplerTest(unittest.TestCase):
    def test_cpu_is_the_growth_over_the_window_and_memory_its_samples(self):
        summary = usage.summarize([(10.0, 100), (10.5, 300), (11.25, 200)], 2.0)
        self.assertEqual(summary["cpu_s"], 1.25)
        self.assertEqual(summary["wall_s"], 2.0)
        self.assertEqual(summary["peak_kb"], 300)
        self.assertEqual(summary["mean_kb"], 200)

    def test_felis_is_summed_sample_by_sample_and_kept_apart(self):
        # The client peaks at the first sample and the daemon at the
        # second; the leg never held both peaks at once.
        series = {
            "client": [(1.0, 500), (2.0, 100)],
            "daemon": [(0.0, 100), (0.5, 400)],
        }
        record = usage.record({"client": [1], "daemon": [2]}, series, 1.0)
        self.assertEqual(record["peak_kb"], 600)
        self.assertEqual(record["cpu_s"], 1.5)
        self.assertEqual(record["parts"]["daemon"]["peak_kb"], 400)

    def test_one_part_is_recorded_flat(self):
        record = usage.record({"terminal": [1, 2]}, {"terminal": [(0, 1), (1, 2)]}, 1)
        self.assertNotIn("parts", record)
        self.assertEqual(record["cpu_s"], 1)

    def run_sampler(self, results: Path, readings, end_after: int):
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
        with (
            mock.patch.object(usage, "reading", fake_reading),
            mock.patch.object(usage, "INTERVAL", 0.01),
        ):
            sampler.start()
            outcome = sampler.finish()
        return outcome, calls

    def test_the_window_runs_from_start_marker_to_end_marker(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            outcome, calls = self.run_sampler(
                results, lambda n: (float(min(n, 4)), 100 * min(n, 4)), end_after=3
            )
            self.assertIsNone(outcome)
            record = json.loads((results / "kitty.res.json").read_text())
            # The baseline comes before `.started` releases the workload,
            # and the window runs past `.end` until a reading shows the
            # terminal idle.
            self.assertTrue((results / "kitty.started").exists())
            self.assertEqual(record["samples"], 5)
            self.assertEqual(record["cpu_s"], 3.0)
            self.assertEqual(record["peak_kb"], 400)

    def test_the_window_stays_open_while_the_terminal_still_works(self):
        # `cat` returns before an eager reader has parsed what it wrote;
        # the CPU it spends after the end marker is the workload's.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            outcome, _calls = self.run_sampler(
                results, lambda n: (float(min(n, 6)), 1), end_after=2
            )
            self.assertIsNone(outcome)
            record = json.loads((results / "kitty.res.json").read_text())
            self.assertEqual(record["cpu_s"], 5.0)

    def test_the_wrapper_waits_for_the_baseline_before_the_workload(self):
        lines = usage.wrap(Path("/r"), "kitty", ["seq 1 10"])
        self.assertEqual(lines[0], "touch /r/kitty.start")
        self.assertIn("/r/kitty.started", lines[1])
        self.assertEqual(lines[2:], ["seq 1 10", "touch /r/kitty.end"])

    def test_a_process_lost_mid_window_leaves_no_record(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            outcome, _calls = self.run_sampler(
                results, lambda n: None if n == 2 else (1.0, 1), end_after=5
            )
            self.assertIn("unreadable", outcome)
            self.assertFalse((results / "kitty.res.json").exists())

    def test_a_workload_that_never_starts_is_named(self):
        with tempfile.TemporaryDirectory() as tmp:
            sampler = usage.Sampler(Path(tmp), "kitty", lambda: {}, lambda: False, 10)
            sampler.start()
            self.assertEqual(sampler.finish(), "the workload never started")

    def test_felis_is_charged_for_its_daemon_too(self):
        run = mock.Mock()
        run.daemon_pid.return_value = 99
        leg = field.Leg("felis", mock.Mock(pid=5), Path("x"), None, run)
        self.assertEqual(suites.leg_processes(leg), {"client": [5], "daemon": [99]})
        run.daemon_pid.return_value = None
        with self.assertRaises(RuntimeError):
            suites.leg_processes(leg)

    def test_a_variant_is_sampled_under_its_real_process_name(self):
        leg = field.Leg("ghostty-tip", mock.Mock(pid=5), Path("x"), None, None)
        with mock.patch.object(
            suites.fieldmod, "child_pids", return_value=[5, 6]
        ) as children:
            self.assertEqual(suites.leg_processes(leg), {"terminal": [5, 6]})
        children.assert_called_once_with(5, "ghostty")


class UsageLoaderTest(unittest.TestCase):
    def test_each_sampled_suite_is_a_panel_in_both_charts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for suite, cpu in (("vtebench", 2.0), ("cat", 1.0)):
                (root / suite).mkdir()
                (root / suite / "kitty.res.json").write_text(
                    json.dumps(
                        {"metric": "pss", "cpu_s": cpu, "wall_s": 4.0, "peak_kb": 2048}
                    )
                )
            cpu, memory = ld.load_usage(root)
            self.assertEqual(cpu.categories, ["vtebench", "cat"])
            self.assertAlmostEqual(cpu.data["kitty"]["vtebench"].value, 2.0)
            self.assertAlmostEqual(memory.data["kitty"]["cat"].value, 2.0)
            self.assertIn("vtebench: kitty 50%", " ".join(cpu.notes))

    def test_the_notes_say_what_the_cpu_number_leaves_out(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "cat").mkdir()
            (root / "cat" / "felis.res.json").write_text(
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
            cpu, memory = ld.load_usage(root)
            self.assertIn("client and its daemon summed", cpu.subtitle)
            self.assertIn("GPU driver", cpu.subtitle)
            self.assertIn("client 1.00 s", " ".join(cpu.notes))
            self.assertIn("Not a kernel high-water mark", memory.subtitle)

    def test_a_root_with_no_records_has_no_resource_charts(self):
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "vtebench").mkdir()
            self.assertEqual(ld.load_usage(Path(tmp)), [])


class LatencyLoaderTest(unittest.TestCase):
    @staticmethod
    def write(directory: Path, term: str, samples: list[float]) -> None:
        (directory / f"{term}.latency.json").write_text(
            json.dumps({"count": len(samples), "delay_ms": 150, "samples_ms": samples})
        )

    def test_mean_carries_the_spread_as_its_whisker(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "felis", [10.0, 20.0, 30.0])
            (suite,) = ld.load_latency(d)
            point = suite.data["felis"]["mean"]
            self.assertAlmostEqual(point.value, 20.0)
            self.assertAlmostEqual(point.err, 8.16496580927726)

    def test_the_tail_is_an_observed_sample_not_an_interpolation(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            # 100 samples: nearest-rank p95 is the 95th in order, 95.0.
            self.write(d, "kitty", [float(i) for i in range(1, 101)])
            (suite,) = ld.load_latency(d)
            self.assertAlmostEqual(suite.data["kitty"]["95th percentile"].value, 95.0)

    def test_the_terminal_name_survives_the_double_suffix(self):
        # `Path.stem` would leave "ghostty-tip.latency" here.
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "ghostty-tip", [12.0])
            (suite,) = ld.load_latency(d)
            self.assertEqual(list(suite.data), ["ghostty-tip"])

    def test_a_retried_leg_is_named_with_its_reason(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "alacritty", [10.0, 20.0])
            (d / "alacritty.done").touch()
            (d / "alacritty.latency-failed").write_text("the line would not clear\n")
            (d / "ghostty-tip.latency-failed").write_text("no row\nno row\n")
            (suite,) = ld.load_latency(d)
            note = " ".join(suite.notes)
            self.assertIn(
                "alacritty (measured on a later attempt): the line would not clear",
                note,
            )
            self.assertIn("ghostty-tip (not measured): no row; no row", note)

    def test_cells_the_terminal_repainted_on_its_own_are_counted(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "ghostty-tip.latency.json").write_text(
                json.dumps({"samples_ms": [10.0], "covered_before_key": 3})
            )
            (suite,) = ld.load_latency(d)
            self.assertIn("ghostty-tip 3", " ".join(suite.notes))

    def test_a_leg_that_produced_no_samples_is_not_a_bar(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "wezterm", [])
            self.assertEqual(ld.load_latency(d), [])


class LatencyRetryTest(unittest.TestCase):
    def attempts(self, outcomes: list[bool | str]) -> tuple[bool, int]:
        """Drive `with_retry`; a string outcome is an instrument failure."""
        calls = []
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)

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

    def test_an_instrument_failure_gets_one_more_window(self):
        self.assertEqual(self.attempts(["line would not clear", True]), (True, 2))

    def test_a_terminal_that_fails_twice_stays_failed(self):
        self.assertEqual(self.attempts(["no pattern", "no pattern"]), (False, 2))

    def test_a_skipped_leg_is_not_retried(self):
        # No focus or no quiet desktop is not a transient of the
        # measurement, and a second window would be skipped the same way.
        self.assertEqual(self.attempts([False]), (False, 1))

    def test_the_reason_is_the_instrument_s_last_line(self):
        self.assertEqual(
            suites.last_line("  wl-latency: inserting...\nwl-latency: no row\n\n"),
            "wl-latency: no row",
        )


class FrontmostWindowTest(unittest.TestCase):
    """Refusing to type into the wrong window is the whole guard."""

    def test_the_pid_is_read_out_of_lsappinfo(self):
        with mock.patch.object(field.subprocess, "run") as run:
            run.side_effect = [
                mock.Mock(stdout="ASN:0x0-0x51051:\n"),
                mock.Mock(stdout='"pid"=2012\n'),
            ]
            self.assertEqual(field.frontmost_pid(), 2012)

    def test_no_frontmost_application_is_not_a_pid(self):
        with mock.patch.object(field.subprocess, "run") as run:
            run.return_value = mock.Mock(stdout="\n")
            self.assertIsNone(field.frontmost_pid())

    def test_a_locked_screen_is_named_rather_than_guessed_at(self):
        # It fails every leg the same way and the fix is a password, not
        # a re-run, so the suite has to say so.
        with mock.patch.object(field, "frontmost_pid", lambda: 442):
            with mock.patch.object(field.subprocess, "run") as run:
                run.return_value = mock.Mock(
                    stdout="/System/Library/CoreServices/loginwindow.app/"
                    "Contents/MacOS/loginwindow\n"
                )
                self.assertTrue(field.screen_locked())

    def test_the_tree_reaches_a_gui_process_the_cli_spawned(self):
        # `wezterm start` is the launched pid; wezterm-gui owns the
        # window, and matching only the launch pid would read as "not
        # frontmost" for a window that is.
        children = {"10": "20\n", "20": "30\n"}
        with mock.patch.object(field.subprocess, "run") as run:
            run.side_effect = lambda argv, **_: mock.Mock(
                stdout=children.get(argv[2], "")
            )
            self.assertEqual(field.process_tree(10), {10, 20, 30})


class PlatformEscapesTest(unittest.TestCase):
    """The two AppKit detours every leg and every run go through."""

    def test_off_macos_the_raise_is_already_done(self):
        with mock.patch.object(field, "DARWIN", False):
            with mock.patch.object(field.subprocess, "run") as run:
                self.assertTrue(field.raise_window(4242))
            run.assert_not_called()

    def test_linux_holds_the_display_through_logind(self):
        with mock.patch.object(field, "DARWIN", False):
            with mock.patch.object(field.shutil, "which", lambda _: "/bin/inhibit"):
                with mock.patch.object(field.subprocess, "Popen") as popen:
                    field.keep_display_awake()
        argv = popen.call_args.args[0]
        self.assertEqual(argv[0], "systemd-inhibit")
        self.assertIn("--what=idle:sleep", argv)

    def test_no_logind_leaves_the_run_without_one(self):
        with mock.patch.object(field, "DARWIN", False):
            with mock.patch.object(field.shutil, "which", lambda _: None):
                self.assertIsNone(field.keep_display_awake())


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


@presenting
class DrivenLegTest(unittest.TestCase):
    """The `drive` hook the latency suite steers its window through."""

    def run_leg(self, results: Path, drive, opens_marker: bool, fld=None):
        def launch(_launch, _wrapper):
            if opens_marker:
                (attempt_dir(results) / "kitty.ready").touch()
            return mock.Mock(pid=4242, poll=lambda: 0)

        quiet = io.StringIO()
        with (
            mock.patch.object(suites.fieldmod, "raise_window", lambda _pid: True),
            mock.patch.object(suites.fieldmod, "launch_terminal", side_effect=launch),
            mock.patch.object(
                suites.fieldmod, "wait_for", lambda path, _t, _a=None: path.exists()
            ),
            contextlib.redirect_stdout(quiet),
            contextlib.redirect_stderr(quiet),
        ):
            return suites.run_leg(
                fld or stub_field(),
                "kitty",
                results,
                lambda _n, _o: [],
                1,
                drive,
                "ready",
            )

    def test_the_driver_is_handed_the_launch_pid(self):
        with tempfile.TemporaryDirectory() as tmp:
            seen = []
            ok = self.run_leg(
                Path(tmp), lambda name, pid, _o: seen.append((name, pid)) or True, True
            )
            self.assertTrue(ok)
            self.assertEqual(seen, [("kitty", 4242)])

    def test_a_marker_from_a_failed_attempt_does_not_stand_in(self):
        # Left behind by a previous run, it would otherwise be taken for
        # this window's — and satisfied before the window even opened,
        # so the driver would type into whatever was in front.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            (results / "kitty.ready").touch()
            seen = []
            ok = self.run_leg(
                results, lambda name, _pid, _o: seen.append(name) or True, False
            )
            self.assertFalse(ok)
            self.assertEqual(seen, [])

    def test_the_window_is_raised_before_it_is_pinned(self):
        # ghostty opens no window at all until its application is
        # activated, so a pin that came first would have nothing to
        # place and a wait would sit out its whole timeout.
        order = []
        quiet = io.StringIO()

        class Ordered(StubPin):
            def pin(self, launch_pid, target):
                order.append("pin")
                return super().pin(launch_pid, target)

        with tempfile.TemporaryDirectory() as tmp:
            with (
                mock.patch.object(
                    suites.fieldmod,
                    "raise_window",
                    lambda _pid: order.append("raise") or True,
                ),
                mock.patch.object(suites.fieldmod, "launch_terminal") as launch,
                mock.patch.object(
                    suites.fieldmod,
                    "wait_for",
                    lambda *_a, **_k: order.append("wait") or True,
                ),
                contextlib.redirect_stdout(quiet),
                contextlib.redirect_stderr(quiet),
            ):
                launch.return_value = mock.Mock(pid=1, poll=lambda: 0)
                suites.run_leg(
                    stub_field(pin=Ordered()),
                    "kitty",
                    Path(tmp),
                    lambda _n, _o: [],
                    1,
                    lambda _n, _p, _o: True,
                    "ready",
                )
        self.assertEqual(order, ["raise", "pin", "wait"])

    def test_an_undriven_suite_is_raised_too(self):
        # Not only the suite that types: ghostty's window-on-activation
        # behavior costs an undriven leg its whole timeout just the
        # same, which reads on the chart as a terminal that was never
        # measured.
        quiet = io.StringIO()
        with tempfile.TemporaryDirectory() as tmp:
            with (
                mock.patch.object(suites.fieldmod, "raise_window") as raise_window,
                mock.patch.object(suites.fieldmod, "launch_terminal") as launch,
                mock.patch.object(suites.fieldmod, "wait_for", lambda *_a, **_k: True),
                contextlib.redirect_stdout(quiet),
                contextlib.redirect_stderr(quiet),
            ):
                launch.return_value = mock.Mock(pid=1, poll=lambda: 0)
                suites.run_leg(stub_field(), "kitty", Path(tmp), lambda _n, _o: [], 1)
            raise_window.assert_called_once_with(1)

    def test_a_refused_measurement_leaves_the_leg_to_be_retried(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            self.assertFalse(self.run_leg(results, lambda _n, _p, _o: False, True))
            # No `.done`: the next run launches this terminal again
            # rather than reporting it measured.
            self.assertFalse((results / "kitty.done").exists())

    def test_a_leg_the_pin_could_not_place_is_recorded_before_it_runs(self):
        # After the stty fallback the cell grid matches, so a check on
        # the final `.size` alone would call this leg pinned.
        with tempfile.TemporaryDirectory() as tmp:
            fld = stub_field(pin=StubPin(wm.TTY_ONLY, "niri left it at 100x30"))
            self.run_leg(Path(tmp), lambda _n, _p, _o: True, True, fld)
            self.assertEqual(
                fld.mismatches,
                ["kitty: tty pinned, window unpinned (niri left it at 100x30)"],
            )

    def test_an_unpinned_leg_is_on_disk_before_its_workload_runs(self):
        # A run killed mid-suite would otherwise leave `.size` files
        # that agree in cells and nothing saying the window behind one
        # of them was never placed.
        recorded = []

        def seen(results: Path) -> str:
            path = results / "grid-mismatch"
            return path.read_text() if path.exists() else ""

        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            fld = stub_field(pin=StubPin(wm.TTY_ONLY, "niri left it at 100x30"))

            def peek(_launch, _wrapper):
                recorded.append(seen(results))
                (attempt_dir(results) / "kitty.ready").touch()
                return mock.Mock(pid=4242, poll=lambda: 0)

            quiet = io.StringIO()
            with (
                mock.patch.object(suites.fieldmod, "raise_window", lambda _pid: True),
                mock.patch.object(suites.fieldmod, "launch_terminal", side_effect=peek),
                mock.patch.object(
                    suites.fieldmod,
                    "wait_for",
                    lambda path, _t, _a=None: (
                        recorded.append(seen(results)) or path.exists()
                    ),
                ),
                contextlib.redirect_stdout(quiet),
                contextlib.redirect_stderr(quiet),
            ):
                suites.run_leg(
                    fld, "kitty", results, lambda _n, _o: [], 1, marker="ready"
                )
            # The wait is the workload's start: the note is already there.
            self.assertIn("tty pinned, window unpinned", recorded[-1])

    def test_a_note_is_not_written_twice(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            field.write_mismatch(results, "kitty: unpinned (no window)")
            field.write_mismatch(results, "kitty: unpinned (no window)")
            self.assertEqual(
                (results / "grid-mismatch").read_text(),
                "kitty: unpinned (no window)\n",
            )

    def test_the_attempt_local_files_are_cleared_before_the_launch(self):
        # A `.pinned` from a failed attempt releases the next window
        # before it has been placed.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            for suffix in ("pinned", "size", "stty", "ready"):
                (results / f"kitty.{suffix}").write_text("stale\n")
            self.run_leg(results, lambda _n, _p, _o: True, True)
            self.assertFalse((results / "kitty.stty").exists())
            self.assertFalse((results / "kitty.size").exists())

    def test_a_leg_whose_display_cannot_present_is_never_launched(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            locked = {"locked": True, "asleep": False}
            with (
                mock.patch.object(envinfo, "display_state", lambda: locked),
                mock.patch.object(suites.fieldmod, "launch_terminal") as launch,
                contextlib.redirect_stderr(io.StringIO()) as err,
            ):
                ok = suites.run_leg(
                    stub_field(), "kitty", results, lambda _n, _o: [], 1
                )
            self.assertFalse(ok)
            launch.assert_not_called()
            self.assertIn("REFUSED: kitty", err.getvalue())
            record = json.loads((results / "kitty.env.json").read_text())
            self.assertEqual(record["refused"], "the screen was locked before the leg")
            self.assertFalse((results / "kitty.done").exists())

    def test_a_refused_leg_leaves_no_wrapper_script_behind(self):
        with tempfile.TemporaryDirectory() as tmp:
            locked = {"locked": True, "asleep": False}
            with (
                mock.patch.object(envinfo, "display_state", lambda: locked),
                mock.patch.object(suites.fieldmod, "wrapper_script") as wrapper,
                contextlib.redirect_stderr(io.StringIO()),
            ):
                suites.run_leg(stub_field(), "kitty", Path(tmp), lambda _n, _o: [], 1)
            wrapper.assert_not_called()

    def test_a_leg_that_ended_unable_to_present_lands_in_refused(self):
        states = iter([PRESENTING, {"locked": False, "asleep": True}, PRESENTING])
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)

            def drive(name, _pid, out):
                (out / f"{name}.done").touch()
                (out / f"{name}.latency.json").write_text("{}")
                return True

            with mock.patch.object(envinfo, "display_state", lambda: next(states)):
                ok = self.run_leg(results, drive, True)
            self.assertFalse(ok)
            # Out of the loaders' reach and the resume marker with them.
            self.assertFalse((results / "kitty.done").exists())
            self.assertFalse((results / "kitty.latency.json").exists())
            self.assertTrue((results / "refused" / "kitty.latency.json").exists())
            self.assertTrue((results / "refused" / "kitty.done").exists())
            record = json.loads((results / "kitty.env.json").read_text())
            self.assertEqual(
                record["refused"], "the display was asleep when the leg ended"
            )
            self.assertIn("after", record)

    def test_a_write_the_stop_did_not_prevent_is_not_charted(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)

            def drive(name, _pid, out):
                (out / f"{name}.latency.json").write_text("measured")
                (out / f"{name}.done").touch()
                return True

            def escaped(_roots):
                (attempt_dir(results) / "kitty.latency.json").write_text("late")
                return [4243]

            with mock.patch.object(suites.fieldmod, "stop", escaped):
                self.assertTrue(self.run_leg(results, drive, True))
            self.assertEqual((results / "kitty.latency.json").read_text(), "measured")

    def test_a_failed_measurement_s_number_is_not_charted(self):
        # An instrument can write its JSON and still exit nonzero.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)

            def drive(name, _pid, out):
                (out / f"{name}.latency.json").write_text('{"samples_ms": [1]}')
                suites.record_latency_failure(results, name, "exited 1")
                return False

            self.assertFalse(self.run_leg(results, drive, True))
            self.assertFalse((results / "kitty.latency.json").exists())
            self.assertTrue((results / "unfinished" / "kitty.latency.json").exists())
            self.assertTrue((results / "kitty.env.json").exists())
            self.assertEqual(suites.latency_failures(results, "kitty"), ["exited 1"])

    def test_a_driver_that_raises_leaves_nothing_for_the_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)

            def drive(name, _pid, out):
                (out / f"{name}.latency.json").write_text('{"samples_ms": [1]}')
                raise RuntimeError("the instrument vanished")

            with self.assertRaises(RuntimeError):
                self.run_leg(results, drive, True)
            self.assertEqual(sorted(p.name for p in results.iterdir()), [".attempts"])
            self.assertEqual(list((results / ".attempts").iterdir()), [])


class LegAttemptTest(unittest.TestCase):
    """What reaches the suite directory from one try at a leg, and when."""

    def test_a_completed_attempt_lands_with_its_resume_marker(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            with field.attempt(results, "kitty") as tried:
                tried.path("json").write_text("{}")
                tried.path("done").touch()
                self.assertFalse((results / "kitty.json").exists())
            self.assertTrue((results / "kitty.json").exists())
            self.assertTrue((results / "kitty.done").exists())
            self.assertEqual(list((results / field.ATTEMPTS_DIR).iterdir()), [])

    def test_an_attempt_that_raised_leaves_the_suite_directory_untouched(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            with self.assertRaises(KeyboardInterrupt):
                with field.attempt(results, "kitty") as tried:
                    tried.path("json").write_text("{}")
                    raise KeyboardInterrupt
            self.assertFalse((results / "kitty.json").exists())
            self.assertEqual(list((results / field.ATTEMPTS_DIR).iterdir()), [])

    def test_a_retry_that_raises_does_not_leave_the_earlier_row_charted(self):
        # An interrupted run from before attempts existed could leave a
        # row with no resume marker; the retry must not publish it even
        # when it fails before writing its own.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            (results / "felis-warm.startup.json").write_text(
                json.dumps({"samples_ms": [5.0, 6.0]})
            )
            (results / "felis.env.json").write_text("{}")
            with self.assertRaises(OSError):
                with field.attempt(results, "felis"):
                    raise OSError("the priming launch could not be spawned")
            self.assertEqual(ld.collect(results, "startup"), [])
            self.assertFalse((results / "felis.env.json").exists())

    def test_a_write_after_promotion_does_not_reach_the_promoted_copy(self):
        # A workload that outlived its window still holds the file it
        # was redirected into.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            with field.attempt(results, "kitty") as tried:
                late = tried.path("doom-fire").open("w")
                late.write("fps 60\n")
                late.flush()
                tried.path("done").touch()
            with late:
                late.write("fps 1\n")
            self.assertEqual((results / "kitty.doom-fire").read_text(), "fps 60\n")

    def test_a_write_after_the_leg_is_sealed_is_not_promoted(self):
        # A process the stop did not reach can write between the stop
        # and the promotion.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            with field.attempt(results, "kitty") as tried:
                out = tried.dir
                (out / "kitty.dat").write_text("measured")
                (out / "kitty.done").touch()
                tried.seal()
                (out / "kitty.dat").write_text("late")
                (out / "kitty.res.json").write_text("{}")
                tried.path("env.json").write_text("{}")
            self.assertEqual((results / "kitty.dat").read_text(), "measured")
            self.assertFalse((results / "kitty.res.json").exists())
            self.assertTrue((results / "kitty.env.json").exists())

    def test_a_marker_touched_after_promotion_lands_nowhere(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            with field.attempt(results, "kitty") as tried:
                marker = tried.path("done")
            with self.assertRaises(FileNotFoundError):
                marker.touch()
            self.assertFalse((results / "kitty.done").exists())

    def test_the_latency_failure_log_outlives_a_retry(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            suites.record_latency_failure(results, "kitty", "first window")
            (results / "kitty.size").write_text("40 120 0 0\n")
            with field.attempt(results, "kitty") as tried:
                tried.path("done").touch()
            self.assertEqual(
                suites.latency_failures(results, "kitty"), ["first window"]
            )
            self.assertFalse((results / "kitty.size").exists())

    def test_felis_s_attempt_owns_both_startup_rows(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            for stale in ("felis-cold", "felis-warm", "felis"):
                (results / f"{stale}.startup.json").write_text("{}")
            (results / "ghostty-tip.startup.json").write_text("{}")
            with field.attempt(results, "felis"):
                pass
            self.assertEqual(
                sorted(p.name for p in results.glob("*.json")),
                ["ghostty-tip.startup.json"],
            )

    def test_a_promotion_cut_short_is_finished_before_the_suite_resumes(self):
        # Committed means every file was staged; finishing it moves the
        # rest, the resume marker last.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            staged = results / field.ATTEMPTS_DIR / f"kitty{field.PROMOTING}"
            staged.mkdir(parents=True)
            (staged / "kitty.done").touch()
            (staged / "kitty.dat").write_text("data")
            (results / "kitty.res.json").write_text("{}")
            abandoned = results / field.ATTEMPTS_DIR / "kitty.abcd1234"
            abandoned.mkdir()
            (abandoned / "kitty.dat").write_text("stale")
            field.clear_attempts(results)
            self.assertEqual((results / "kitty.dat").read_text(), "data")
            self.assertTrue((results / "kitty.done").exists())
            self.assertTrue((results / "kitty.res.json").exists())
            self.assertFalse((results / field.ATTEMPTS_DIR).exists())

    def test_a_refused_promotion_cut_short_still_lands_in_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            staged = results / field.ATTEMPTS_DIR / f"kitty{field.PROMOTING}"
            staged.mkdir(parents=True)
            (staged / field.ASIDE_MARK).write_text(field.REFUSED_DIR)
            (staged / "kitty.json").write_text("{}")
            (staged / "kitty.done").touch()
            (results / "kitty.env.json").write_text(json.dumps({"refused": "locked"}))
            field.clear_attempts(results)
            self.assertFalse((results / "kitty.done").exists())
            self.assertFalse((results / "kitty.json").exists())
            self.assertTrue((results / field.REFUSED_DIR / "kitty.done").exists())

    def test_the_report_finishes_a_committed_promotion(self):
        with tempfile.TemporaryDirectory() as tmp:
            suite = Path(tmp) / "startup"
            staged = suite / field.ATTEMPTS_DIR / f"kitty{field.PROMOTING}"
            staged.mkdir(parents=True)
            (staged / "kitty.startup.json").write_text(
                json.dumps({"samples_ms": [5.0, 6.0]})
            )
            (suite / field.ATTEMPTS_DIR / "wezterm.x").mkdir()
            (
                suite / field.ATTEMPTS_DIR / "wezterm.x" / "wezterm.startup.json"
            ).write_text(json.dumps({"samples_ms": [5.0, 6.0]}))
            (startup,) = [s for s in ld.collect(Path(tmp), None) if s.key == "startup"]
            self.assertIn("kitty", startup.data)
            self.assertNotIn("wezterm", startup.data)

    def test_an_attempt_that_returned_unfinished_lands_only_its_env_record(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            with field.attempt(results, "kitty") as tried:
                tried.path("dat").write_text("half a run")
                tried.path("env.json").write_text("{}")
            self.assertEqual(
                sorted(p.name for p in results.iterdir() if p.is_file()),
                ["kitty.env.json"],
            )
            self.assertTrue((results / field.UNFINISHED_DIR / "kitty.dat").exists())

    def test_the_memory_suite_finishes_on_its_record(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            with field.attempt(results, "kitty", "mem.json") as tried:
                tried.path("mem.json").write_text("{}")
            self.assertTrue((results / "kitty.mem.json").exists())

    def test_a_refused_attempt_lands_only_its_reason(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            with field.attempt(results, "kitty") as tried:
                tried.path("json").write_text("{}")
                tried.path("done").touch()
                tried.path("env.json").write_text(json.dumps({"refused": "locked"}))
            self.assertEqual(
                sorted(p.name for p in results.iterdir() if p.is_file()),
                ["kitty.env.json"],
            )
            self.assertEqual(
                sorted(p.name for p in (results / field.REFUSED_DIR).iterdir()),
                ["kitty.done", "kitty.json"],
            )


@unittest.skipIf(sys.platform == "darwin", "pidfds are Linux's")
class StopTest(unittest.TestCase):
    """Stopping a leg's processes by identity, on real processes."""

    def spawn(self, script: str) -> subprocess.Popen:
        proc = subprocess.Popen(["bash", "-c", script])
        self.addCleanup(self.reap, proc)
        return proc

    def reap(self, proc: subprocess.Popen) -> None:
        with contextlib.suppress(ProcessLookupError):
            proc.kill()
        proc.wait()

    def gone(self, pid: int) -> bool:
        def exited() -> bool:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return True
            stat = Path(f"/proc/{pid}/stat").read_text()
            return stat[stat.rfind(")") + 2] == "Z"

        return wait_until(exited, 5)

    def test_a_process_that_ignores_sigterm_is_killed(self):
        with tempfile.TemporaryDirectory() as tmp:
            child = Path(tmp) / "child"
            proc = self.spawn(f"trap '' TERM; sleep 30 & echo $! > {child}; wait")
            self.assertTrue(wait_until(lambda: child.exists(), 5))
            lingering = field.stop([field.Held.child(proc)], grace=0.3)
            self.assertEqual(lingering, [])
            self.assertTrue(self.gone(int(child.read_text())))

    def test_a_child_forked_after_the_first_walk_is_stopped_too(self):
        with tempfile.TemporaryDirectory() as tmp:
            child, ready = Path(tmp) / "child", Path(tmp) / "ready"
            proc = self.spawn(
                f"trap '' TERM; touch {ready}; sleep 0.5; "
                f"sleep 30 & echo $! > {child}; wait"
            )
            self.assertTrue(wait_until(ready.exists, 5))
            lingering = field.stop([field.Held.child(proc)], grace=1.5)
            self.assertEqual(lingering, [])
            self.assertTrue(child.exists())
            self.assertTrue(self.gone(int(child.read_text())))

    def test_a_process_that_does_not_belong_is_not_held(self):
        proc = self.spawn("sleep 30")
        self.assertIsNone(field.hold(proc.pid, lambda: False))

    def test_a_signal_after_the_process_is_gone_reaches_no_pid(self):
        proc = self.spawn("sleep 30")
        held = field.hold(proc.pid)
        self.addCleanup(held.close)
        proc.kill()
        proc.wait()
        with mock.patch.object(field.os, "kill") as kill:
            held.send(signal.SIGKILL)
        kill.assert_not_called()
        self.assertFalse(held.alive())


class DarwinHoldTest(unittest.TestCase):
    """macOS has no pidfd, so a process is held by its start time."""

    def test_a_pid_that_restarted_while_being_held_is_refused(self):
        starts = iter([100, 200])
        with (
            mock.patch.object(field, "DARWIN", True),
            mock.patch.object(field, "start_time", lambda _pid: next(starts)),
        ):
            self.assertIsNone(field.hold(4242))

    def test_a_reused_pid_is_not_signalled(self):
        with (
            mock.patch.object(field, "DARWIN", True),
            mock.patch.object(field, "start_time", lambda _pid: 100),
        ):
            held = field.hold(4242)
        with (
            mock.patch.object(field, "start_time", lambda _pid: 300),
            mock.patch.object(field.os, "kill") as kill,
        ):
            held.send(signal.SIGKILL)
        kill.assert_not_called()

    def test_the_process_still_running_is_signalled(self):
        with (
            mock.patch.object(field, "DARWIN", True),
            mock.patch.object(field, "start_time", lambda _pid: 100),
            mock.patch.object(field.os, "kill") as kill,
        ):
            field.hold(4242).send(signal.SIGTERM)
        kill.assert_called_once_with(4242, signal.SIGTERM)


@presenting
class MemoryLegTest(unittest.TestCase):
    """The suite that samples its own legs, now through the shared leg site."""

    def run_suite(self, results: Path, err: io.StringIO):
        # Stand in for felis's own branch: this test is about the loop
        # over the rest of the field.
        (results / "felis.mem.json").write_text("{}")
        with (
            mock.patch.object(suites.fieldmod, "raise_window") as raise_window,
            mock.patch.object(
                suites.fieldmod,
                "launch_terminal",
                return_value=mock.Mock(pid=4242, poll=lambda: 0),
            ),
            mock.patch.object(suites.fieldmod, "wait_for", lambda *_a, **_k: False),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(err),
        ):
            suites.suite_memory(stub_field(), results, {})
        return raise_window

    def test_its_legs_are_raised_like_every_other_suite(self):
        # Sampling its own legs is not a reason to be the one suite
        # ghostty cannot appear in.
        with tempfile.TemporaryDirectory() as tmp:
            raise_window = self.run_suite(Path(tmp), io.StringIO())
            raise_window.assert_called_once_with(4242)

    def test_a_leg_that_never_settles_says_so(self):
        # It used to return quietly, which on the chart is indistinct
        # from a terminal that was measured and had nothing to report.
        with tempfile.TemporaryDirectory() as tmp:
            err = io.StringIO()
            self.run_suite(Path(tmp), err)
            self.assertIn("kitty", err.getvalue())

    def test_its_phase_markers_are_cleared_before_the_launch(self):
        # A stale `.idle` lets the sampler take its first reading before
        # the window exists and chart pre-flood numbers as flooded.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            (results / "kitty.idle").write_text("stale\n")
            (results / "kitty.flooded").write_text("stale\n")
            self.run_suite(results, io.StringIO())
            self.assertFalse((results / "kitty.idle").exists())
            self.assertFalse((results / "kitty.flooded").exists())


@presenting
class WrapperHandshakeTest(unittest.TestCase):
    """The wrapper's half of the pin, run for real under a PTY.

    The one asynchronous invariant in the pin: the workload must not
    start until the driver says the window is placed, and a tty pin left
    behind must reach the tty before that. No parsing test can show
    either, so this one forks a PTY, runs the generated wrapper in it
    and watches the order the files appear in.
    """

    GRID = (40, 120, 960, 640)

    def workload(self, name: str, out: Path) -> list[str]:
        """A stub workload that records the grid it started under."""
        return [
            f"cp {shlex.quote(str(out / f'{name}.size'))} "
            f"{shlex.quote(str(out / f'{name}.workload'))}",
            f"touch {shlex.quote(str(out / f'{name}.done'))}",
        ]

    def launcher(self, results: Path, name: str):
        """Run the leg's wrapper inside a PTY of a known size."""

        def launch(_launch, wrapper):
            pid, master = pty.fork()
            if pid == 0:  # pragma: no cover - replaced by execv
                os.execv(str(wrapper), [str(wrapper)])
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", *self.GRID))
            self.addCleanup(self.reap, pid, master)
            return ForkedChild(pid)

        return launch

    def reap(self, pid: int, master: int) -> None:
        with contextlib.suppress(OSError):
            os.kill(pid, signal.SIGKILL)
        with contextlib.suppress(OSError):
            os.waitpid(pid, 0)
        os.close(master)

    def run_leg(self, results: Path, pin) -> None:
        quiet = io.StringIO()
        with (
            mock.patch.object(suites.fieldmod, "raise_window", lambda _pid: True),
            mock.patch.object(
                suites.fieldmod, "launch_terminal", self.launcher(results, "kitty")
            ),
            mock.patch.object(
                suites.fieldmod,
                "wait_for",
                lambda path, timeout, _a=None: wait_until(path.exists, timeout),
            ),
            contextlib.redirect_stdout(quiet),
            contextlib.redirect_stderr(quiet),
        ):
            suites.run_leg(
                stub_field(pin=pin, xpixel=960, ypixel=640),
                "kitty",
                results,
                self.workload,
                60,
                marker="done",
            )

    def test_the_workload_waits_for_the_pin(self):
        class Watching(StubPin):
            def pin(self, launch_pid, target):
                # The size the pin is placing against has to be
                # readable, and the workload must not have run yet.
                self.size = wait_until_size(target.size)
                self.ran_early = target.size.with_suffix(".workload").exists()
                return super().pin(launch_pid, target)

        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            pin = Watching()
            self.run_leg(results, pin)
            self.assertEqual(pin.size, field.Size(*self.GRID))
            self.assertFalse(pin.ran_early)
            self.assertEqual(
                field.read_size(results / "kitty.workload"), field.Size(*self.GRID)
            )

    def test_a_tty_pin_reaches_the_tty_before_the_workload(self):
        class Falling(StubPin):
            def pin(self, launch_pid, target):
                wait_until_size(target.size)
                target.stty.write_text("30 100\n")
                return wm.PinResult(wm.TTY_ONLY, None, "the window would not move")

        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            self.run_leg(results, Falling())
            # What the workload saw, not merely what the file ended at:
            # a pin applied afterwards would leave the same `.size` and
            # a payload shaped by the wrong grid.
            seen = field.read_size(results / "kitty.workload")
            self.assertEqual(seen.cells(), (30, 100))

    def test_a_stale_pinned_file_does_not_release_the_next_window(self):
        # The retry case: the previous attempt left its markers behind,
        # and a wrapper that saw them would run the workload against a
        # window nobody had placed yet.
        class Watching(StubPin):
            def pin(self, launch_pid, target):
                self.ran_early = target.size.with_suffix(".workload").exists()
                wait_until_size(target.size)
                return super().pin(launch_pid, target)

        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            (results / "kitty.pinned").touch()
            (results / "kitty.size").write_text("24 80 0 0\n")
            (results / "kitty.workload").write_text("24 80 0 0\n")
            pin = Watching()
            self.run_leg(results, pin)
            self.assertFalse(pin.ran_early)
            self.assertEqual(
                field.read_size(results / "kitty.workload"), field.Size(*self.GRID)
            )


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


class ToolResolutionTest(unittest.TestCase):
    def test_pinned_path_wins_over_path_lookup(self):
        with tempfile.TemporaryDirectory() as tmp:
            pinned = Path(tmp) / "kitty"
            pinned.write_text("")
            with mock.patch.dict(os.environ, {"FELIS_BENCH_KITTY": str(pinned)}):
                with mock.patch.object(
                    ct.shutil, "which", return_value="/usr/bin/kitty"
                ):
                    tool = ct.resolve("kitty")
        self.assertEqual(tool.path, str(pinned))
        self.assertTrue(tool.pinned)

    def test_use_path_ignores_the_pin(self):
        with mock.patch.dict(
            os.environ, {"FELIS_BENCH_KITTY": "/nix/store/x/bin/kitty"}
        ):
            with mock.patch.object(ct.shutil, "which", return_value="/usr/bin/kitty"):
                tool = ct.resolve("kitty", use_path=True)
        self.assertEqual(tool.path, "/usr/bin/kitty")
        self.assertFalse(tool.pinned)

    def test_a_stale_pin_falls_back_instead_of_failing(self):
        # A store path from an older shell can be garbage-collected; the
        # run should degrade to PATH rather than launch a missing binary.
        with mock.patch.dict(
            os.environ, {"FELIS_BENCH_KITTY": "/nix/store/gone/bin/kitty"}
        ):
            with mock.patch.object(ct.shutil, "which", return_value="/usr/bin/kitty"):
                tool = ct.resolve("kitty")
        self.assertEqual(tool.path, "/usr/bin/kitty")

    def test_tool_override_beats_the_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            tip = Path(tmp) / "ghostty"
            tip.write_text("")
            with mock.patch.dict(
                os.environ, {"FELIS_BENCH_GHOSTTY": "/nix/store/x/bin/ghostty"}
            ):
                tool = ct.resolve("ghostty", override=str(tip))
        self.assertEqual(tool.path, str(tip))
        # An overridden build is by definition not the flake's, so the
        # report must not claim the run was reproducible.
        self.assertFalse(tool.pinned)

    def test_override_pointing_nowhere_is_an_error_not_a_fallback(self):
        with mock.patch.dict(
            os.environ, {"FELIS_BENCH_GHOSTTY": "/nix/store/x/bin/ghostty"}
        ):
            with self.assertRaises(FileNotFoundError):
                ct.resolve("ghostty", override="/does/not/exist")

    def test_full_field_requires_both_ghostty_builds(self):
        self.assertEqual(ct.missing_ghostty(["kitty", "ghostty", "ghostty-tip"]), [])
        self.assertEqual(ct.missing_ghostty(["kitty", "ghostty-tip"]), ["ghostty"])
        self.assertEqual(ct.missing_ghostty(["kitty"]), ["ghostty", "ghostty-tip"])

    def test_override_parsing(self):
        self.assertEqual(ct.parse_overrides(["ghostty=/a/b"]), {"ghostty": "/a/b"})
        with self.assertRaises(ValueError):
            ct.parse_overrides(["ghostty"])
        with self.assertRaises(KeyError):
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
        self.assertEqual(params["VTEBENCH_BIN"], "/vt/bin/vtebench")
        self.assertEqual(params["TB_BIN"], "/tb/bin/tb")
        self.assertEqual(params["KITTEN_BIN"], "/k/bin/kitten")
        self.assertEqual(params["DOOM_BIN"], "/d/bin/DOOM-fire")
        self.assertEqual(params["PAYLOAD_DIR"], "/repo/target/bench-payloads")
        self.assertEqual(params["MAX_SECS"], "10")
        # A tool that is not installed must be absent rather than empty:
        # the suite that needs it is skipped by name, up front.
        self.assertNotIn("HYPERFINE_BIN", params)

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
        self.assertEqual(fld.names(), ["kitty"])
        self.assertIsNone(fld.launch("alacritty"))


class EnvinfoTest(unittest.TestCase):
    def test_pmset_batt_reads_source_and_charge(self):
        text = (
            "Now drawing from 'AC Power'\n"
            " -InternalBattery-0 (id=12345)\t78%; charging; 1:23 remaining present: true"
        )
        self.assertEqual(
            envinfo.parse_pmset_batt(text),
            {"source": "AC Power", "battery_percent": 78},
        )

    def test_pmset_therm_reads_the_speed_limit(self):
        text = (
            "Note: No thermal warning level has been recorded\n"
            "CPU_Scheduler_Limit \t= 100\nCPU_Speed_Limit \t= 62\n"
        )
        self.assertEqual(
            envinfo.parse_pmset_therm(text),
            {"cpu_scheduler_limit": 100, "cpu_speed_limit": 62},
        )

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
        self.assertEqual(parsed["gpus"][0]["cores"], "40")
        self.assertEqual(parsed["displays"][0]["resolution"], "3440 x 1440 @ 60.00Hz")

    def test_proc_parsers(self):
        self.assertEqual(
            envinfo.parse_meminfo("MemTotal:       32768000 kB\nMemFree: 100 kB\n"),
            32768000 * 1024,
        )
        self.assertEqual(
            envinfo.parse_cpuinfo("processor\t: 0\nmodel name\t: AMD Ryzen 9 7950X\n"),
            "AMD Ryzen 9 7950X",
        )

    def test_missing_fields_stay_none_rather_than_guessed(self):
        self.assertIsNone(envinfo.parse_cpuinfo(""))
        self.assertIsNone(envinfo.parse_meminfo(""))
        self.assertIsNone(envinfo.first_line(None))

    def test_the_revision_reported_is_the_binary_s_not_the_tree_s(self):
        # A `nix build` result is the case that made this necessary: the
        # store binary was b27da75 while the checkout had moved to
        # 43a4a20b, and the report credited the newer commit.
        with mock.patch.object(
            envinfo,
            "run",
            side_effect=lambda *cmd, **kw: (
                "cli    (b27da75)\nclient (b27da75)\ndaemon (not running)"
                if "--version" in cmd
                else "43a4a20b"
            ),
        ):
            info = envinfo.felis_info(Path("/repo"), Path("/nix/store/x/bin/felis"))
        self.assertEqual(info["revision"], "b27da75")
        self.assertEqual(info["checkout"], "43a4a20b")
        self.assertIn("b27da75", info["drift"])
        self.assertIn("43a4a20b", info["drift"])

    def test_a_binary_built_from_the_checkout_reports_no_drift(self):
        self.assertIsNone(envinfo.revision_drift("b27da75", "b27da75c"))
        self.assertIsNone(envinfo.revision_drift("b27da75c9f1", "b27da75"))
        # Nothing to compare against is not a mismatch.
        self.assertIsNone(envinfo.revision_drift(None, "b27da75"))
        self.assertIsNone(envinfo.revision_drift("b27da75", None))

    def test_uncommitted_work_is_drift_even_when_the_hashes_agree(self):
        # The binary may or may not contain the working tree's edits and
        # its hash cannot say which, so the revision does not identify it.
        drift = envinfo.revision_drift("b27da75", "b27da75-dirty")
        self.assertIsNotNone(drift)
        self.assertIn("uncommitted", drift)

    def test_a_version_line_without_a_hash_leaves_the_checkout_standing(self):
        self.assertIsNone(envinfo.paren_hash("felis 0.1.0"))
        self.assertIsNone(envinfo.paren_hash(None))
        with mock.patch.object(
            envinfo,
            "run",
            side_effect=lambda *cmd, **kw: None if "--version" in cmd else "43a4a20b",
        ):
            info = envinfo.felis_info(Path("/repo"), Path("/gone/felis"))
        self.assertEqual(info["revision"], "43a4a20b")
        self.assertNotIn("drift", info)

    def test_a_store_path_reports_no_build_time_rather_than_1970(self):
        # Nix normalizes store mtimes to the epoch; formatting that as a
        # date puts a field in the report that looks answered and is not.
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "felis"
            binary.write_bytes(b"x")
            os.utime(binary, (1, 1))
            self.assertIsNone(envinfo.build_time(binary))
            os.utime(binary, (1_700_000_000, 1_700_000_000))
            self.assertIsNotNone(envinfo.build_time(binary))

    def test_tool_row_records_where_the_binary_came_from(self):
        with mock.patch.object(envinfo, "tool_version", return_value="kitty 0.47.4"):
            pinned = envinfo.describe_tool("kitty", "/nix/store/x/bin/kitty", True)
            host = envinfo.describe_tool("kitty", "/usr/bin/kitty", False)
        self.assertEqual(pinned["source"], "flake")
        self.assertEqual(host["source"], "path")
        self.assertEqual(
            envinfo.describe_tool("foot", None, False), {"name": "foot", "found": False}
        )


def ioreg_root(**root) -> str:
    return plistlib.dumps(root).decode()


class DisplayStateTest(unittest.TestCase):
    """Whether a leg's window could have presented a frame at all."""

    def test_the_console_lock_is_read_from_the_io_registry_root(self):
        self.assertTrue(
            envinfo.parse_console_lock(ioreg_root(IOConsoleLocked=True).encode())
        )
        self.assertFalse(
            envinfo.parse_console_lock(ioreg_root(IOConsoleLocked=False).encode())
        )

    def test_without_the_root_flag_the_console_session_answers(self):
        session = {"kCGSSessionOnConsoleKey": True, "CGSSessionScreenIsLocked": True}
        other = {"kCGSSessionOnConsoleKey": False, "CGSSessionScreenIsLocked": False}
        raw = ioreg_root(IOConsoleUsers=[other, session]).encode()
        self.assertTrue(envinfo.parse_console_lock(raw))
        # An unlocked session omits the key rather than setting it false.
        unlocked = ioreg_root(IOConsoleUsers=[{"kCGSSessionOnConsoleKey": True}])
        self.assertFalse(envinfo.parse_console_lock(unlocked.encode()))

    def test_no_console_session_or_no_plist_is_unknown(self):
        self.assertIsNone(envinfo.parse_console_lock(ioreg_root().encode()))
        self.assertIsNone(envinfo.parse_console_lock(b"not a plist"))

    def test_macos_reads_the_lock_and_the_main_display_s_sleep(self):
        def run(*cmd, **_kw):
            if cmd[0] == "ioreg":
                return ioreg_root(IOConsoleLocked=False)
            return "1" if cmd[0] == "osascript" else None

        with (
            mock.patch.object(envinfo.sys, "platform", "darwin"),
            mock.patch.object(envinfo, "run", run),
        ):
            self.assertEqual(envinfo.display_state(), {"locked": False, "asleep": True})

    def test_linux_reads_logind_s_hint_on_the_seat_s_active_session(self):
        answers = {"show-seat": "7", "show-session": "yes"}
        seen = []

        def run(*cmd, **_kw):
            seen.append(cmd)
            return answers.get(cmd[1])

        with (
            mock.patch.object(envinfo.sys, "platform", "linux"),
            mock.patch.object(envinfo, "run", run),
        ):
            state = envinfo.display_state()
        # Display sleep has no reading on Linux: unknown, never "awake".
        self.assertEqual(state, {"locked": True, "asleep": None})
        self.assertEqual(seen[1][2], "7")

    def test_a_host_without_logind_is_unknown_not_unlocked(self):
        with (
            mock.patch.object(envinfo.sys, "platform", "linux"),
            mock.patch.object(envinfo, "run", lambda *_a, **_k: None),
        ):
            self.assertEqual(envinfo.display_state(), {"locked": None, "asleep": None})

    def test_only_a_known_dark_display_refuses(self):
        def sample(**display):
            return {"loadavg": 0.1, "display": display}

        self.assertIsNone(envinfo.cannot_present(sample(locked=False, asleep=False)))
        self.assertIsNone(envinfo.cannot_present(sample(locked=None, asleep=None)))
        self.assertIsNone(envinfo.cannot_present({"loadavg": 0.1}))
        self.assertEqual(
            envinfo.cannot_present(sample(locked=True, asleep=True)),
            "the screen was locked and the display was asleep",
        )

    def test_every_leg_sample_carries_the_display(self):
        with (
            mock.patch.object(envinfo, "display_state", lambda: dict(PRESENTING)),
            mock.patch.object(envinfo, "throttle_state", lambda: "unavailable"),
        ):
            self.assertEqual(envinfo.leg_env()["display"], PRESENTING)


class ScaleTest(unittest.TestCase):
    def two_rounds(self, better: str, felis: list[float], kitty: list[float]):
        suite = report.Suite("k", "t", "s", "ms", better, rounds=2)
        for index, value in enumerate(felis, 1):
            suite.put("felis", "x", report.Point(value), index)
        for index, value in enumerate(kitty, 1):
            suite.put("kitty", "x", report.Point(value), index)
        return suite

    def test_ratio_direction_follows_the_metric(self):
        lower = self.two_rounds("lower", [50, 50], [100, 100])
        self.assertEqual(report.ratio_text(lower, "x"), "2.00x")

        higher = self.two_rounds("higher", [50, 50], [100, 100])
        self.assertEqual(report.ratio_text(higher, "x"), "0.50x")

    def test_overlapping_ranges_print_no_ratio(self):
        # 95-105 against 100-110 is a difference the run did not
        # resolve, and two decimals would present it as one.
        overlapping = self.two_rounds("lower", [95, 105], [100, 110])
        self.assertEqual(report.ratio(overlapping, "x"), ("—", report.OVERLAP))
        apart = self.two_rounds("lower", [95, 99], [100, 110])
        self.assertEqual(report.ratio(apart, "x")[0], "1.08x")

    def test_one_round_has_no_noise_floor_to_clear(self):
        single = report.Suite("k", "t", "s", "ms", "lower")
        single.put("felis", "x", report.Point(50))
        single.put("kitty", "x", report.Point(100))
        self.assertEqual(report.ratio(single, "x"), ("—", report.SINGLE_ROUND))
        # A terminal that produced one round of three is the same case,
        # whichever side of the comparison it is on.
        partial = self.two_rounds("lower", [50, 50], [100])
        partial.rounds = 3
        self.assertEqual(report.ratio(partial, "x"), ("—", report.SINGLE_ROUND))

    def test_a_point_is_the_median_of_its_rounds_and_the_range_its_whisker(self):
        suite = self.two_rounds("lower", [50, 90, 70], [100, 100, 100])
        point = suite.data["felis"]["x"]
        self.assertEqual(
            (point.value, point.lo, point.hi, point.rounds), (70, 50, 90, 3)
        )

    def test_a_single_round_point_keeps_its_within_leg_spread(self):
        suite = report.Suite("k", "t", "s", "ms", "lower")
        suite.put("felis", "x", report.Point(50, 4))
        point = suite.data["felis"]["x"]
        self.assertEqual(
            (point.err, point.lo, point.hi, point.rounds), (4, None, None, 1)
        )

    def test_the_err_of_a_multi_round_point_is_the_median_within_leg_spread(self):
        suite = report.Suite("k", "t", "s", "ms", "lower", rounds=3)
        for index, (value, err) in enumerate([(50, 1), (60, 9), (70, 5)], 1):
            suite.put("felis", "x", report.Point(value, err), index)
        self.assertEqual(suite.data["felis"]["x"].err, 5)

    def test_a_bar_with_one_round_draws_no_whisker(self):
        suite = self.two_rounds("lower", [50, 90], [100])
        suite.rounds = 2
        points = [suite.data["felis"]["x"], suite.data["kitty"]["x"]]
        lows, highs = report.whiskers(suite, points)
        self.assertEqual((lows, highs), ([20.0, 0.0], [20.0, 0.0]))
        self.assertIn("1/2 rounds", report.bar_label(suite, points[1]))

    def test_the_subtitle_names_the_whisker_it_drew(self):
        suite = self.two_rounds("lower", [50, 90], [100, 110])
        ld.whisker_note(suite)
        self.assertIn("median over 2 rounds", suite.subtitle)
        single = report.Suite("k", "t", "Sub.", "ms", "lower")
        single.put("felis", "x", report.Point(50, 4))
        ld.whisker_note(single)
        self.assertIn("within-run spread", single.subtitle)


class RenderTest(unittest.TestCase):
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
        self.assertIn("55.0 ms [50.0–60.0] ± 5.0 · 2/2 rounds", page)
        self.assertIn("felis vs best other", page)
        self.assertIn("3.73x", page)
        self.assertIn("abc1234", page)

    def test_a_single_round_page_says_why_its_ratio_column_is_empty(self):
        page = report.render_markdown([self.build()], {}, "T", {})
        self.assertIn("50.0 ms ± 5.0", page)
        self.assertIn("single round, no noise floor", page)

    def test_a_chart_is_linked_only_when_one_was_drawn(self):
        # A report that renders without matplotlib must not point at
        # images that were never written.
        suite = self.build()
        self.assertNotIn("![", report.render_markdown([suite], {}, "T", {}))
        linked = report.render_markdown([suite], {}, "T", {"k": "png/k.png"})
        self.assertIn("![Title](png/k.png)", linked)

    def test_a_terminal_that_was_not_measured_is_a_dash(self):
        suite = self.build()
        suite.put("wezterm", "other", report.Point(9))
        page = report.render_markdown([suite], {}, "T", {})
        self.assertIn("| — |", page)

    def test_a_pipe_in_a_value_does_not_end_its_column(self):
        suite = self.build()
        suite.put("felis", "a|b", report.Point(1))
        row = [
            line
            for line in report.render_markdown([suite], {}, "T", {}).splitlines()
            if "a\\|b" in line
        ]
        self.assertTrue(row, "the category name must survive as one cell")

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
        self.assertIn("12P + 4E cores", page)
        self.assertIn("128 GB", page)
        self.assertIn("3440 x 1440 @ 60.00Hz", page)
        # A throttled run and an unpinned field are the two facts that
        # invalidate a comparison, so both must be visible in the report.
        self.assertIn("CPU speed limit 62%", page)
        self.assertIn("nix develop .#bench", page)
        self.assertIn("unpinned", page)

    def test_a_binary_that_is_not_the_checkout_s_is_flagged_in_the_page(self):
        meta = {
            "felis": {
                "revision": "b27da75",
                "checkout": "43a4a20b",
                "drift": "binary is b27da75, the checkout is 43a4a20b",
            }
        }
        page = report.render_markdown([self.build()], meta, "T", {})
        self.assertIn("b27da75", page)
        self.assertIn("the checkout is 43a4a20b", page)

    def test_a_missing_chart_library_still_produces_the_tables(self):
        # The tables are the report; the charts are how it is read. A
        # run that measured for two hours must not lose both.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            out = root / "report.md"
            with (
                mock.patch.object(
                    report, "draw", side_effect=ModuleNotFoundError("matplotlib")
                ),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                report.write(root, [self.build()], out, root / "png", "T")
            self.assertIn("50.0 ms", out.read_text())
            self.assertNotIn("![", out.read_text())


@unittest.skipUnless(
    importlib.util.find_spec("matplotlib"), "charts need the .#bench shell"
)
class ChartTest(unittest.TestCase):
    def test_a_facet_is_drawn_per_benchmark(self):
        suite = report.Suite("k", "Title", "Sub", "ms", "lower")
        for cat in ("fast", "slow"):
            suite.put("felis", cat, report.Point(50, 5))
            suite.put("kitty", cat, report.Point(200, 40))
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "k.png"
            report.draw(suite, path)
            self.assertTrue(path.exists())
            self.assertTrue(path.stat().st_size > 0)
            self.assertEqual(path.read_bytes()[1:4], b"PNG")


class HarnessTableTest(unittest.TestCase):
    def test_every_suite_has_something_to_run(self):
        for name in ld.HARNESSES:
            self.assertIn(name, suites.SUITES, f"{name} has no measuring function")

    def test_platforms_match_the_available_suite_implementations(self):
        # A suite with no implementation must report itself skipped rather
        # than pretend the platform is unsupported wholesale.
        for name in ("vtebench", "termbench", "doom-fire", "startup", "latency"):
            self.assertTrue(ld.HARNESSES[name].runs_on("linux"), name)
            self.assertTrue(ld.HARNESSES[name].runs_on("darwin"), name)

    def test_each_platform_is_told_its_own_instrument(self):
        # Asking Linux for `typometer` would skip the suite for a missing
        # tool it is never going to use, and asking macOS for the Wayland
        # one would do the same in reverse.
        latency = ld.HARNESSES["latency"]
        self.assertEqual(latency.needs("darwin"), ["typometer"])
        self.assertEqual(latency.needs("linux"), ["wl-latency"])

    def test_a_suite_that_needs_one_tool_everywhere_says_so_once(self):
        self.assertEqual(ld.HARNESSES["cat"].needs("linux"), ["hyperfine"])
        self.assertEqual(ld.HARNESSES["cat"].needs("darwin"), ["hyperfine"])


class WindowPinSelectionTest(unittest.TestCase):
    def test_the_guard_matches_the_process_name_not_a_command_line(self):
        # The predecessor matched `pgrep -f "paneru launch"`, a command
        # line paneru does not have, so it passed while paneru was
        # running and the field came out at the WM's sizes.
        with mock.patch.object(mactiler.subprocess, "run") as run:
            run.return_value.returncode = 1
            mactiler.running_window_manager()
        for call in run.call_args_list:
            self.assertEqual(call.args[0][:2], ["pgrep", "-x"])

    def test_the_first_running_manager_is_named(self):
        with mock.patch.object(mactiler.subprocess, "run") as run:
            run.side_effect = lambda argv, **_: mock.Mock(
                returncode=0 if argv[2] == "yabai" else 1
            )
            self.assertEqual(mactiler.running_window_manager(), "yabai")

    def test_no_tiler_reaches_the_floating_pin(self):
        # A registry naming one of the three would let the other two
        # fall through to the pin that believes every size flag, and
        # silently un-pin the field.
        for name in mactiler.TILING_WMS:
            with mock.patch.object(
                mactiler,
                "running_window_manager",
                lambda captured=name: captured,
            ):
                pin = wm.select()
                self.assertIsInstance(pin, wm.MacTilerPin)
                with self.assertRaises(wm.PinRefused):
                    pin.prepare()

    def test_niri_wins_over_the_floating_pin_when_it_is_running(self):
        with (
            mock.patch.object(mactiler, "running_window_manager", lambda: None),
            mock.patch.dict(os.environ, {"NIRI_SOCKET": "/run/niri.sock"}),
            mock.patch.object(wm.niri.shutil, "which", lambda _: "/bin/niri"),
        ):
            self.assertIsInstance(wm.select(), wm.NiriPin)

    def test_a_desktop_with_no_tiler_floats(self):
        with (
            mock.patch.object(mactiler, "running_window_manager", lambda: None),
            mock.patch.dict(os.environ, {}, clear=True),
        ):
            pin = wm.select()
            self.assertIsInstance(pin, wm.FloatingPin)
            self.assertFalse(pin.can_resize)


class LaunchTest(unittest.TestCase):
    """The pinned launch argv — what "the same conditions" actually means."""

    PRES = field.Presentation(
        family="DejaVu Sans Mono", pt=9, scale=2.0, scrollback=5000, rows=40, cols=120
    )

    # ghostty's scrollback key was renamed after 1.3.1, so build_launch
    # probes the binary. Tests supply the answer instead of needing one
    # installed; TIP_CAPS is what a post-1.3.1 build reports.
    TIP_CAPS = frozenset({"scrollback-limit-lines", "scrollback-limit-bytes"})

    def build(self, name, caps=frozenset()):
        launch = field.build_launch(name, f"/bin/{name}", self.PRES, caps=caps)
        self.addCleanup(lambda: [p.unlink(missing_ok=True) for p in launch.tmp])
        return launch

    def rendered(self, name, caps=frozenset()) -> str:
        """argv plus any config file it writes, as one searchable string."""
        launch = self.build(name, caps)
        bodies = [p.read_text() for p in launch.tmp]
        return " ".join([*launch.argv, *launch.env.values(), *bodies])

    def test_every_terminal_starts_from_a_pristine_config(self):
        # The failure this prevents: alacritty and ghostty read
        # ~/.config unless told otherwise, so a developer's own ligature
        # font and scrollback were being benchmarked against felis's
        # defaults. Each entry is that terminal's opt-out.
        pristine = {
            "kitty": "--config NONE",
            "alacritty": "--config-file",
            "wezterm": "WEZTERM_CONFIG_FILE",
            "ghostty": "--config-default-files=false",
            "foot": "-c /dev/null",
        }
        for name, flag in pristine.items():
            self.assertIn(
                flag,
                " ".join(self.build(name).argv) + " ".join(self.build(name).env),
                f"{name} would read the developer's own config",
            )

    def test_every_terminal_is_told_the_same_font(self):
        for name in ("kitty", "alacritty", "wezterm", "ghostty", "foot"):
            self.assertIn("DejaVu Sans Mono", self.rendered(name), name)
            self.assertRegex(self.rendered(name), r"\b9\b", name)

    def test_every_terminal_is_told_the_same_grid_in_cells(self):
        for name in ("kitty", "alacritty", "wezterm", "ghostty", "foot"):
            rendered = self.rendered(name)
            self.assertIn("120", rendered, name)
            self.assertIn("40", rendered, name)
        # kitty's flag is pixels without the `c` suffix, and foot's -w is
        # pixels where -W is cells; both would silently mean something else.
        self.assertIn("initial_window_width=120c", self.build("kitty").argv)
        self.assertIn("-W", self.build("foot").argv)

    def test_every_terminal_retains_the_same_scrollback_depth(self):
        # The defaults span an order of magnitude — foot 1000, kitty
        # 2000, wezterm 3500, alacritty 10000 — and the flooded half of
        # the memory suite is decided by whichever cap binds first.
        depth = {
            "kitty": "scrollback_lines=5000",
            "alacritty": "scrolling.history=5000",
            "wezterm": "scrollback_lines = 5000",
            "foot": "scrollback.lines=5000",
        }
        for name, setting in depth.items():
            self.assertIn(setting, self.rendered(name), name)

    def test_ghostty_takes_the_line_cap_only_where_the_build_has_one(self):
        # 1.3.1 caps by bytes alone and no byte count honestly maps to a
        # row count, so it keeps its default; passing the key anyway
        # would make ghostty open an error dialog instead of the wrapper.
        self.assertIn(
            "--scrollback-limit-lines=5000", self.build("ghostty", self.TIP_CAPS).argv
        )
        self.assertNotIn("scrollback", " ".join(self.build("ghostty").argv))

    def test_a_terminal_that_escaped_the_scrollback_pin_is_named(self):
        # A pin that silently failed to apply is worse than no pin: the
        # chart lines the bars up either way, so the caveat has to
        # travel with the results.
        with mock.patch.object(field, "ghostty_config_keys", return_value=frozenset()):
            self.assertEqual(
                field.unpinned_scrollback({"ghostty": "/bin/ghostty"}),
                ["ghostty: byte-capped only, ran at its default"],
            )
        with mock.patch.object(
            field, "ghostty_config_keys", return_value=self.TIP_CAPS
        ):
            self.assertEqual(field.unpinned_scrollback({"ghostty": "/bin/ghostty"}), [])

    def test_no_terminal_blinks_its_cursor(self):
        # felis, kitty and ghostty blink out of the box and the other
        # three do not. A blink is a repaint twice a second forever,
        # which is invisible in a throughput number and is most of an
        # idle CPU one.
        steady = {
            "kitty": "cursor_blink_interval=0",
            "alacritty": 'cursor.style.blinking="Never"',
            "wezterm": "cursor_blink_rate = 0",
            "ghostty": "--cursor-style-blink=false",
            "foot": "cursor.blink=no",
        }
        for name, setting in steady.items():
            self.assertIn(setting, self.rendered(name), name)
        self.assertIn('blink = "never"', self.PRES.felis_config())

    def test_frame_pacing_is_never_pinned(self):
        # The line between the two kinds of setting: how much work there
        # is gets pinned, how a terminal chooses to serve it does not.
        # Equalising vsync or a repaint delay would measure a
        # configuration none of these terminals ship.
        strategy = ("repaint_delay", "input_delay", "sync_to_monitor", "vsync", "fps")
        for name in ("kitty", "alacritty", "wezterm", "ghostty", "foot"):
            rendered = self.rendered(name, self.TIP_CAPS)
            for knob in strategy:
                self.assertNotIn(knob, rendered, f"{name} pinned {knob}")

    def test_an_unprobed_grid_leaves_the_size_flags_off(self):
        # Better an unpinned window than one pinned to a guess: the
        # report reads the grid back from the run either way.
        pres = field.Presentation(rows=None, cols=None)
        argv = field.build_launch("kitty", "/bin/kitty", pres).argv
        self.assertNotIn("initial_window_width=None c", " ".join(argv))
        self.assertNotIn("initial_window_height", " ".join(argv))

    def test_the_child_command_lands_after_the_exec_flag(self):
        # alacritty and ghostty take `-e CMD`; a wrapper appended before
        # the flag would be read as a positional and ignored.
        for name in ("alacritty", "ghostty"):
            self.assertEqual(
                self.build(name).command("/tmp/w.sh")[-2:], ["-e", "/tmp/w.sh"]
            )

    def test_a_font_family_with_spaces_survives_the_hyperfine_string(self):
        # hyperfine takes one shell string, so the argv has to be quoted
        # back into one; unquoted, "DejaVu Sans Mono" becomes three args.
        command = self.build("ghostty").shell_command("/tmp/w.sh")
        self.assertIn("'--font-family=DejaVu Sans Mono'", command)

    def test_an_unknown_terminal_is_an_error_not_an_empty_launch(self):
        with self.assertRaises(KeyError):
            field.build_launch("xterm", "/bin/xterm", self.PRES)

    def test_a_variant_launches_exactly_like_the_build_it_stands_beside(self):
        # The point of the ghostty-tip bar is that the only difference
        # from the ghostty bar is the binary; a flag that drifted between
        # the two would turn the comparison into a different question.
        stable = field.build_launch("ghostty", "/bin/g", self.PRES).argv
        tip = field.build_launch("ghostty-tip", "/bin/g", self.PRES).argv
        self.assertEqual(stable, tip)

    def test_the_variant_sits_next_to_its_release_in_the_field(self):
        order = list(field.FIELD_ORDER)
        self.assertEqual(order[order.index("ghostty") + 1], "ghostty-tip")
        # And in the palette, so the pair reads as a pair.
        hues = report.TERM_ORDER
        self.assertEqual(hues[hues.index("ghostty") + 1], "ghostty-tip")


class VariantTest(unittest.TestCase):
    def test_a_variant_is_never_picked_up_from_path_or_the_flake(self):
        # A nightly a lockfile or a PATH entry supplies is not a nightly;
        # it enters the field only when someone names a build.
        with mock.patch.dict(
            os.environ, {"FELIS_BENCH_GHOSTTY_TIP": "/nix/store/x/bin/ghostty"}
        ):
            with mock.patch.object(ct.shutil, "which", return_value=None):
                tool = ct.resolve("ghostty-tip")
        # The env pin exists but points nowhere, so nothing is found —
        # and the flake never sets it in the first place.
        self.assertFalse(tool.found)

    def test_naming_a_build_puts_it_in_the_field_beside_the_release(self):
        with tempfile.TemporaryDirectory() as tmp:
            tip = Path(tmp) / "ghostty"
            tip.write_text("")
            tools = ct.resolve_field(False, {"ghostty-tip": str(tip)})
        self.assertEqual(tools["ghostty-tip"].path, str(tip))
        self.assertFalse(tools["ghostty-tip"].pinned)

    def test_an_unpinned_tool_is_recorded_by_digest_not_only_by_name(self):
        # ghostty-tip prints the same "Ghostty 1.3.1" banner as the
        # release and its file is replaced daily, so a chart is only
        # attributable to one build if the digest is written down.
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "ghostty"
            binary.write_bytes(b"tip")
            row = envinfo.describe_tool("ghostty-tip", str(binary), pinned=False)
        self.assertEqual(
            row["sha256"],
            "97380187a878903ffe722b7bfd8d8ba92457ef77de0216cfe9261c72c2b87397",
        )
        self.assertIn("built", row)

    def test_a_flake_pinned_tool_needs_no_digest(self):
        # The store path already names its contents; hashing a 50 MB
        # binary on every run to restate that is waste.
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "kitty"
            binary.write_bytes(b"pinned")
            row = envinfo.describe_tool("kitty", str(binary), pinned=True)
        self.assertNotIn("sha256", row)

    def test_a_vanished_binary_is_reported_without_a_digest_not_a_crash(self):
        row = envinfo.describe_tool("ghostty-tip", "/does/not/exist", pinned=False)
        self.assertTrue(row["found"])
        self.assertNotIn("sha256", row)

    def test_the_child_process_of_a_variant_is_matched_by_its_real_name(self):
        # `ps -o comm=` says "ghostty" for a ghostty-tip leg, so memory
        # sampling has to look for the alias or it counts no children.
        self.assertEqual(field.LAUNCH_ALIAS["ghostty-tip"], "ghostty")


class FelisConfigTest(unittest.TestCase):
    def test_the_config_writes_the_key_felis_actually_reads(self):
        pres = field.Presentation(family="Menlo", pt=9, scale=2.0)
        body = pres.felis_config()
        self.assertIn('family = "Menlo"', body)
        # `font.size` is not a key: felis logs "unknown key ignored" for
        # it and runs its 14px default, which is the whole field's grid
        # since the others are pinned to whatever felis probed.
        self.assertIn("size_px = ", body)
        self.assertNotIn("\nsize = ", body)

    def test_the_backing_scale_stays_out_of_the_pin(self):
        # Not 18: the client multiplies `font.size_px` by the window's
        # scale factor itself, so a pre-multiplied pin scales twice.
        expect = "size_px = 9" if field.DARWIN else "size_px = 12"
        self.assertIn(expect, field.Presentation(pt=9, scale=2.0).felis_config())

    def test_features_are_pinned_off(self):
        # A ligature font with `calt` on shapes differently from one
        # without, so the feature list is pinned rather than defaulted.
        self.assertIn("features = []", field.Presentation().felis_config())


class BackingScaleTest(unittest.TestCase):
    def test_scale_comes_from_native_over_logical(self):
        retina = {
            "_spdisplays_pixels": "3456 x 2234",
            "_spdisplays_resolution": "1728 x 1117 @ 120.00Hz",
        }
        self.assertEqual(envinfo.backing_scale(retina), 2.0)

    def test_a_one_to_one_panel_is_1x(self):
        plain = {
            "_spdisplays_pixels": "3440 x 1440",
            "_spdisplays_resolution": "3440 x 1440 @ 60.00Hz",
        }
        self.assertEqual(envinfo.backing_scale(plain), 1.0)

    def test_unparseable_resolution_is_unknown_not_1(self):
        # A run at 2x paints four times the pixels of one at 1x, so a
        # guessed 1x would file a Retina run's numbers under the wrong
        # conditions; "unknown" has to survive as far as the caller.
        self.assertIsNone(envinfo.backing_scale({"_spdisplays_pixels": "3456 x 2234"}))
        self.assertIsNone(envinfo.parse_wh("built-in"))

    def test_main_display_wins_over_the_first_one(self):
        machine = {
            "displays": [
                {"name": "external", "backing_scale": 1.0},
                {"name": "built-in", "backing_scale": 2.0, "main": True},
            ]
        }
        self.assertEqual(envinfo.main_backing_scale(machine), 2.0)

    def test_no_display_data_reports_unknown(self):
        self.assertIsNone(envinfo.main_backing_scale({"displays": []}))


class FontPinTest(unittest.TestCase):
    def test_the_pin_is_the_point_size_in_the_platform_s_logical_pixels(self):
        # A point is a logical pixel on macOS and 3/4 of one under the
        # Wayland convention every other terminal converts against, so
        # an unconverted pin would run felis 25% smaller than the field.
        pt = field.Presentation(pt=9, scale=1.0).px
        self.assertEqual(pt, 9 if field.DARWIN else 12)

    def test_the_backing_scale_is_not_a_conversion_factor(self):
        self.assertEqual(
            field.Presentation(pt=9, scale=2.0).px,
            field.Presentation(pt=9, scale=1.0).px,
        )

    def test_the_point_size_reaches_the_terminals_without_a_stray_decimal(self):
        # kitty and ghostty take the value verbatim, so `9.0` from a
        # float would be a different string than the recorded `9`.
        self.assertEqual(field.Presentation(pt=9.0).pt_str, "9")
        self.assertEqual(field.Presentation(pt=9.5).pt_str, "9.5")

    def test_the_orchestrator_and_the_field_share_one_default(self):
        # Two defaults that disagree would mean a suite run on its own
        # measured a different font than one run through the orchestrator.
        self.assertEqual(ct.DEFAULT_PARAMS["FONT_FAMILY"], field.DEFAULT_FONT_FAMILY)
        self.assertEqual(ct.DEFAULT_PARAMS["FONT_PT"], f"{field.DEFAULT_FONT_PT:g}")

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
        self.assertEqual(line, "212x56 cells · Menlo 9pt")

    def test_a_run_whose_felis_pin_differed_still_reports_both_sizes(self):
        # Runs recorded before the pin dropped its backing-scale
        # conversion carry FONT_PX != FONT_PT, and their reports have to
        # keep saying which size felis actually ran.
        line = report.pinned_field(
            {"FONT_FAMILY": "Menlo", "FONT_PT": "9", "FONT_PX": "18"}
        )
        self.assertEqual(line, "Menlo 9pt (felis 18px)")

    def test_an_unpinned_run_says_nothing_rather_than_half_a_condition(self):
        self.assertEqual(report.pinned_field({}), "")


class GridVerificationTest(unittest.TestCase):
    def test_one_shared_grid_reads_as_comparable(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            for name in ("felis", "kitty", "ghostty"):
                (d / f"{name}.size").write_text("56 212\n")
            (note,) = ld.grid_note(d)
            self.assertIn("same grid: 212x56", note)

    def test_a_terminal_that_missed_the_pin_is_called_out(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.size").write_text("56 212\n")
            (d / "kitty.size").write_text("50 212\n")
            (note,) = ld.grid_note(d)
            self.assertIn("GRIDS DIFFERED", note)
            self.assertIn("kitty 212x50", note)

    def test_a_recorded_mismatch_is_carried_into_the_note(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.size").write_text("56 212\n")
            (d / "grid-mismatch").write_text(
                "felis fell back from 'Menlo' to monospace\n"
            )
            (note,) = ld.grid_note(d)
            self.assertIn("fell back", note)

    def test_mismatches_are_collected_across_suites(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "memory").mkdir()
            (root / "memory" / "grid-mismatch").write_text("kitty got '50 212'\n")
            self.assertEqual(ct.mismatches(root), ["memory: kitty got '50 212'"])

    def test_a_clean_run_reports_no_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(ct.mismatches(Path(tmp)), [])


class SizeParsingTest(unittest.TestCase):
    def test_the_four_fields_are_cells_then_pixels(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "felis.size"
            path.write_text("56 212 1696 1344\n")
            self.assertEqual(field.read_size(path), field.Size(56, 212, 1696, 1344))

    def test_a_two_field_file_still_parses(self):
        # What `stty size` wrote before the pixel columns existed; a
        # results root taken then still has to render.
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "felis.size"
            path.write_text("56 212\n")
            size = field.read_size(path)
            self.assertEqual(size.cells(), (56, 212))
            self.assertFalse(size.has_pixels)

    def test_a_torn_or_missing_file_is_not_a_grid(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "felis.size"
            self.assertIsNone(field.read_size(path))
            path.write_text("56 212 169")
            self.assertIsNone(field.read_size(path))
            path.write_text("56 x 1696 1344")
            self.assertIsNone(field.read_size(path))


class PixelVerificationTest(unittest.TestCase):
    """The second column: the same cells drawn in a different area."""

    PRES = field.Presentation(rows=40, cols=100, xpixel=1000, ypixel=800)

    def test_padding_inside_one_cell_is_not_a_mismatch(self):
        got = field.Size(40, 100, 1006, 793)
        self.assertIsNone(field.verify_grid("kitty", got, self.PRES))

    def test_a_font_that_did_not_apply_is_caught_by_width(self):
        # Same cell count, a wider cell: exactly what a missed font pin
        # looks like, and what a cell count cannot see.
        got = field.Size(40, 100, 1200, 800)
        problem = field.verify_grid("kitty", got, self.PRES)
        self.assertIn("1200x800", problem)

    def test_height_is_compared_on_its_own(self):
        # One area against another would accept a wide-short window as a
        # narrow-tall one; they are not the same picture.
        got = field.Size(40, 100, 1000, 700)
        self.assertIsNotNone(field.verify_grid("kitty", got, self.PRES))

    def test_a_terminal_that_reports_no_pixels_is_checked_by_cells(self):
        got = field.Size(40, 100)
        self.assertIsNone(field.verify_grid("foot", got, self.PRES))

    def test_the_tolerance_is_one_reference_cell_per_axis(self):
        self.assertEqual(field.pixel_tolerance(self.PRES), (10.0, 20.0))

    def test_a_note_names_the_terminals_that_reported_no_pixels(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.size").write_text("40 100 1000 800\n")
            (d / "foot.size").write_text("40 100 0 0\n")
            (note,) = ld.grid_note(d)
            self.assertIn("Pixels unreported by foot", note)

    def test_both_pixel_notes_stand_on_their_own(self):
        # One terminal reporting nothing says nothing about whether the
        # ones that did report drew the same area.
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.size").write_text("40 100 1000 800\n")
            (d / "foot.size").write_text("40 100 0 0\n")
            (d / "kitty.size").write_text("40 100 1200 800\n")
            (note,) = ld.grid_note(d)
            self.assertIn("Pixels unreported by foot", note)
            self.assertIn("kitty 1200x800 px", note)

    def test_a_note_names_cells_of_a_different_size(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "felis.size").write_text("40 100 1000 800\n")
            (d / "kitty.size").write_text("40 100 1200 800\n")
            (note,) = ld.grid_note(d)
            self.assertIn("not the same size", note)
            self.assertIn("kitty 1200x800 px", note)


class NiriPinTest(unittest.TestCase):
    """The mechanics, verified against niri 26.04 before they were built on."""

    def test_a_window_is_matched_over_the_launch_s_process_tree(self):
        # `wezterm start` spawns wezterm-gui, which owns the window, so
        # the launch pid alone matches nothing.
        windows = [{"id": 3, "pid": 99}, {"id": 4, "pid": 4242}]
        self.assertEqual(wm.niri.match_window(windows, {40, 41, 4242}), (4, ""))

    def test_no_mapped_window_is_reported_rather_than_guessed(self):
        found, detail = wm.niri.match_window([{"id": 3, "pid": 99}], {40})
        self.assertIsNone(found)
        self.assertIn("no window", detail)

    def test_several_windows_of_one_launch_are_refused(self):
        windows = [{"id": 3, "pid": 40}, {"id": 4, "pid": 41}]
        found, detail = wm.niri.match_window(windows, {40, 41})
        self.assertIsNone(found)
        self.assertIn("2 windows", detail)

    def test_a_physical_target_is_asked_for_in_logical_pixels(self):
        # niri's set-window-* take logical pixels and TIOCGWINSZ reports
        # physical ones; at 2x a 1400 px window is asked for as 700.
        self.assertEqual(wm.niri.to_logical(1400, 2.0), 700)
        self.assertEqual(wm.niri.to_logical(1400, 1.0), 1400)

    def test_the_step_moves_by_the_cells_that_are_missing(self):
        # 100 cells in 800 physical px is an 8 px cell; ten more cells
        # at 1x is 80 logical px more.
        self.assertEqual(wm.niri.cell_step(800, 100, 800, 110, 1.0), 880)

    def test_the_step_crosses_the_scale_once(self):
        # The same window at 2x: 400 logical px wide, an 8 px physical
        # cell, so ten more cells is 40 logical px more.
        self.assertEqual(wm.niri.cell_step(400, 100, 800, 110, 2.0), 440)

    def test_a_terminal_reporting_no_pixels_steps_by_the_reference_cell(self):
        # Standing still would spend every attempt asking for the size
        # the window already has, and end tty-pinned for want of a cell
        # size the reference window knows.
        self.assertEqual(wm.niri.cell_step(800, 100, 0, 110, 1.0, 8.0), 880)

    def test_a_step_with_no_cell_size_at_all_leaves_the_window_alone(self):
        self.assertEqual(wm.niri.cell_step(800, 100, 0, 110, 1.0), 800)

    def test_one_output_is_the_one_the_scale_comes_from(self):
        outputs = {"DP-1": {"logical": {"scale": 2.0}}}
        self.assertEqual(wm.niri.output_scale(outputs, None), ("DP-1", 2.0))

    def test_with_several_outputs_the_focused_one_decides(self):
        outputs = {
            "DP-1": {"logical": {"scale": 1.0}},
            "DP-2": {"logical": {"scale": 2.0}},
        }
        self.assertEqual(wm.niri.output_scale(outputs, "DP-2"), ("DP-2", 2.0))

    def test_an_unreadable_scale_refuses_rather_than_assuming_one(self):
        # Every pixel target crosses this number, so assuming 1.0 would
        # halve or double the whole field silently.
        self.assertIsNone(wm.niri.output_scale({}, None))
        outputs = {
            "DP-1": {"logical": {"scale": 1.0}},
            "DP-2": {"logical": {"scale": 2.0}},
        }
        self.assertIsNone(wm.niri.output_scale(outputs, None))


class NiriResizeLoopTest(unittest.TestCase):
    """The loop that walks a tiled window onto the reference grid."""

    def resize(self, results: Path, lands_on_attempt: int, pixels: bool = True):
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
        with (
            mock.patch.object(wm.niri.field, "process_tree", lambda _pid: {4242}),
            mock.patch.object(wm.niri, "msg", lambda *_a: json.dumps([window])),
            mock.patch.object(wm.niri, "action", action),
            mock.patch.object(
                wm.niri, "wait_settled", lambda path, _t: field.read_size(path)
            ),
            mock.patch.object(
                wm.NiriPin,
                "wait_for_change",
                lambda _s, path, _p, timeout=0: field.read_size(path),
            ),
        ):
            return pin.pin(4242, target), asked

    def test_the_window_is_walked_onto_the_reference_grid(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, asked = self.resize(Path(tmp), lands_on_attempt=1)
            self.assertEqual(result.status, wm.PINNED)
            # 100 cells in 800 px is an 8 px cell, so twenty more
            # columns is 160 logical pixels more.
            self.assertEqual(asked[0], ("set-window-height", "--id", "7", "800"))

    def test_the_last_attempt_is_checked_before_the_tty_fallback(self):
        # The match is tested at the top of the loop, so a window that
        # only arrives on the final resize would otherwise be handed a
        # tty pin it does not need.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            result, asked = self.resize(results, lands_on_attempt=wm.niri.MAX_ATTEMPTS)
            self.assertEqual(result.status, wm.PINNED)
            self.assertFalse((results / "kitty.stty").exists())

    def test_a_terminal_that_reports_no_pixels_is_still_resized(self):
        # `cell_step` has no measured cell to work from, so the pin has
        # to carry the reference one or this leg can never be placed.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            result, asked = self.resize(results, lands_on_attempt=1, pixels=False)
            self.assertEqual(result.status, wm.PINNED)
            self.assertEqual(asked[0], ("set-window-height", "--id", "7", "760"))

    def test_a_window_that_never_arrives_leaves_a_tty_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            result, asked = self.resize(results, lands_on_attempt=99)
            self.assertEqual(result.status, wm.TTY_ONLY)
            self.assertEqual(len(asked), wm.niri.MAX_ATTEMPTS)
            self.assertEqual((results / "kitty.stty").read_text(), "40 120\n")


class FloatingPinTest(unittest.TestCase):
    def test_a_leg_that_missed_the_grid_leaves_a_tty_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "kitty.size").write_text("30 100 800 600\n")
            target = wm.Target(
                size=d / "kitty.size",
                stty=d / "kitty.stty",
                rows=40,
                cols=120,
                xpixel=960,
                ypixel=640,
            )
            with mock.patch.object(
                wm.floating, "wait_settled", lambda path, _t: field.read_size(path)
            ):
                result = wm.FloatingPin().pin(1, target)
            self.assertEqual(result.status, wm.TTY_ONLY)
            self.assertEqual((d / "kitty.stty").read_text(), "40 120\n")

    def test_a_leg_that_took_the_grid_is_pinned_without_a_tty_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "kitty.size").write_text("40 120 960 640\n")
            target = wm.Target(
                size=d / "kitty.size", stty=d / "kitty.stty", rows=40, cols=120
            )
            with mock.patch.object(
                wm.floating, "wait_settled", lambda path, _t: field.read_size(path)
            ):
                result = wm.FloatingPin().pin(1, target)
            self.assertEqual(result.status, wm.PINNED)
            self.assertFalse((d / "kitty.stty").exists())


class RoundLayoutTest(unittest.TestCase):
    def test_a_root_without_round_directories_is_one_implicit_round(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "kitty.kitten").write_text("ascii : 1s @ 100.0 MB/s\n")
            self.assertEqual(ld.rounds(d), [d])

    def test_round_directories_come_back_in_run_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            for index in (2, 10, 1):
                (d / f"round-{index}").mkdir()
            self.assertEqual(
                [p.name for p in ld.rounds(d)], ["round-1", "round-2", "round-10"]
            )

    def test_a_root_holding_both_layouts_is_refused_by_name(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "round-1").mkdir()
            (d / "kitty.kitten").write_text("")
            with self.assertRaises(ValueError) as caught:
                ld.rounds(d)
            self.assertIn("kitty.kitten", str(caught.exception))

    def test_a_run_log_does_not_make_a_rounds_root_mixed(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "round-1").mkdir()
            (d / "run.log").write_text("narration\n")
            self.assertEqual([p.name for p in ld.rounds(d)], ["round-1"])

    def kitten_rounds(self, d: Path, rates: dict[str, list[float]]) -> None:
        for index in range(1, 1 + max(len(v) for v in rates.values())):
            (d / f"round-{index}").mkdir()
            for name, values in rates.items():
                if index <= len(values):
                    (d / f"round-{index}" / f"{name}.kitten").write_text(
                        f"ascii : 1s @ {values[index - 1]} MB/s\n"
                    )

    def test_a_bar_is_the_median_over_the_rounds_that_produced_one(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.kitten_rounds(d, {"felis": [100.0, 140.0, 120.0], "kitty": [90.0]})
            (suite,) = ld.load_kittenbench(d)
            self.assertEqual(suite.rounds, 3)
            felis = suite.data["felis"]["ascii"]
            self.assertEqual(
                (felis.value, felis.lo, felis.hi, felis.rounds), (120, 100, 140, 3)
            )
            self.assertEqual(suite.data["kitty"]["ascii"].rounds, 1)

    def test_a_mismatch_in_one_round_only_is_reported_with_its_round(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.kitten_rounds(d, {"felis": [100.0, 140.0], "kitty": [90.0, 95.0]})
            for index in (1, 2):
                for name in ("felis", "kitty"):
                    (d / f"round-{index}" / f"{name}.size").write_text(
                        "40 120 960 640\n"
                    )
            (d / "round-2" / "grid-mismatch").write_text(
                "kitty: tty pinned, window unpinned (niri)\n"
            )
            (note,) = ld.grid_note(d)
            self.assertIn("Every terminal ran at the same grid: 120x40.", note)
            self.assertIn("round 2: kitty: tty pinned, window unpinned (niri)", note)

    def test_a_leg_that_changed_grid_between_rounds_is_listed_per_round(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            for index, cols in ((1, 120), (2, 100)):
                (d / f"round-{index}").mkdir()
                (d / f"round-{index}" / "felis.size").write_text("40 120 960 640\n")
                (d / f"round-{index}" / "kitty.size").write_text(
                    f"40 {cols} {cols * 8} 640\n"
                )
            (note,) = ld.grid_note(d)
            self.assertIn("GRIDS DIFFERED", note)
            self.assertIn("kitty round 2 100x40", note)

    def test_meta_json_is_found_from_inside_a_round_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "meta.json").write_text(json.dumps({"params": {"CAT_MB": "8"}}))
            (root / "cat" / "round-1").mkdir(parents=True)
            self.assertEqual(ld.run_params(root / "cat" / "round-1").get("CAT_MB"), "8")


class RotationTest(unittest.TestCase):
    def test_every_leg_moves_in_every_later_round(self):
        legs = ["felis", "kitty", "alacritty", "wezterm", "foot"]
        for index in range(1, len(legs)):
            rotated = field.rotate(legs, index)
            self.assertEqual(sorted(rotated), sorted(legs))
            self.assertFalse(
                [a for a, b in zip(legs, rotated, strict=True) if a == b],
                f"round {index + 1} left a terminal in place",
            )

    def test_the_first_round_is_chart_order(self):
        legs = ["felis", "kitty"]
        self.assertEqual(field.rotate(legs, 0), legs)

    def test_a_suite_iterates_the_order_it_was_handed(self):
        fld = field.Field(
            {"kitty": "/bin/true", "foot": "/bin/true"},
            "/bin/true",
            field.Presentation(family="Menlo", pt=9.0),
            leg_order=["foot", "felis", "kitty"],
        )
        self.assertEqual(fld.legs(), ["foot", "felis", "kitty"])


class ResumePreflightTest(unittest.TestCase):
    def test_a_rounds_root_is_refused_for_a_single_round_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "kittenbench" / "round-1").mkdir(parents=True)
            (root / "kittenbench" / "round-2").mkdir()
            with self.assertRaises(ValueError) as caught:
                ct.check_resume(root, 1)
            self.assertIn("2 round directories", str(caught.exception))
            self.assertIn("ROUNDS=1", str(caught.exception))

    def test_a_single_round_root_is_refused_for_a_repeated_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "kittenbench").mkdir(parents=True)
            (root / "kittenbench" / "kitty.kitten").write_text("")
            with self.assertRaises(ValueError) as caught:
                ct.check_resume(root, 3)
            self.assertIn("ROUNDS=1", str(caught.exception))
            self.assertIn("ROUNDS=3", str(caught.exception))

    def test_round_directories_without_a_recorded_count_are_their_own_count(self):
        # No meta.json to state it, so the directories are the record:
        # ROUNDS=3 against two of them would leave a round nobody ran.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "kittenbench" / "round-1").mkdir(parents=True)
            (root / "kittenbench" / "round-2").mkdir()
            with self.assertRaises(ValueError) as caught:
                ct.check_resume(root, 3)
            self.assertIn("ROUNDS=2", str(caught.exception))
            self.assertIn("ROUNDS=3", str(caught.exception))
            ct.check_resume(root, 2)

    def test_a_partly_finished_run_resumes_at_its_recorded_count(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "meta.json").write_text(json.dumps({"params": {"ROUNDS": "3"}}))
            (root / "kittenbench" / "round-1").mkdir(parents=True)
            ct.check_resume(root, 3)

    def test_a_recorded_round_count_must_match_the_requested_one(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "meta.json").write_text(json.dumps({"params": {"ROUNDS": "3"}}))
            with self.assertRaises(ValueError) as caught:
                ct.check_resume(root, 2)
            self.assertIn("ROUNDS=3", str(caught.exception))
            self.assertIn("ROUNDS=2", str(caught.exception))
            ct.check_resume(root, 3)

    def test_a_root_written_before_rounds_existed_resumes_as_one_round(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "meta.json").write_text(json.dumps({"params": {"CAT_MB": "8"}}))
            (root / "cat").mkdir()
            (root / "cat" / "kitty.json").write_text("{}")
            ct.check_resume(root, 1)

    def test_the_refusal_lands_before_a_window_opens(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "root"
            (root / "kittenbench" / "round-1").mkdir(parents=True)
            felis_bin = Path(tmp) / "felis"
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
            err = io.StringIO()
            with (
                mock.patch.object(ct, "resolve_field", return_value=tools),
                mock.patch.object(ct.wm, "select", return_value=wm.FloatingPin()),
                mock.patch.object(ct, "cached_grid", opened),
                mock.patch.object(suites, "probe_grid", opened),
                contextlib.redirect_stderr(err),
            ):
                code = ct.cmd_run(args, Path(tmp))
            self.assertEqual(code, 2)
            self.assertIn("ROUNDS=1", err.getvalue())

    def test_the_order_file_records_the_run_that_measured_the_legs(self):
        # A resume skips the done legs, so rewriting the file would
        # replace the order they ran in with one nothing was measured
        # under — and the environment note reads that order.
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp) / "cat"
            pres = field.Presentation(family="Menlo", pt=9.0)
            first = field.Field({"kitty": "/bin/true"}, "/bin/true", pres)
            resumed = field.Field(
                {"kitty": "/bin/true"},
                "/bin/true",
                pres,
                leg_order=["kitty", "felis"],
            )
            with (
                mock.patch.dict(suites.SUITES, {"cat": lambda *_args: True}),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                ct.run_suite("cat", first, results, {})
                ct.run_suite("cat", resumed, results, {})
            self.assertEqual((results / "order").read_text(), "felis\nkitty\n")

    def test_one_round_writes_the_suite_directory_itself(self):
        root = Path("/results")
        self.assertEqual(ct.suite_dir(root, "cat", 1, 1), root / "cat")
        self.assertEqual(ct.suite_dir(root, "cat", 2, 3), root / "cat" / "round-2")

    def test_rounds_must_be_a_count(self):
        self.assertEqual(ct.requested_rounds({}), 1)
        self.assertEqual(ct.requested_rounds({"ROUNDS": "3"}), 3)
        for bad in ("0", "-1", "two", "2.5"):
            with self.assertRaises(ValueError):
                ct.requested_rounds({"ROUNDS": bad})


class LegEnvironmentTest(unittest.TestCase):
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

    def test_a_leg_taken_under_more_load_than_the_first_one_is_named(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "order").write_text("felis\nkitty\n")
            self.write(d, "felis", self.linux(0.4, 0), self.linux(0.5, 0))
            self.write(d, "kitty", self.linux(0.6, 0), self.linux(2.1, 0))
            (note,) = ld.env_notes(d)
            self.assertIn("kitty 2.1", note)
            self.assertNotIn("felis", note)

    def test_load_within_one_of_the_first_leg_says_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "felis", self.linux(0.4, 0), self.linux(0.5, 0))
            self.write(d, "kitty", self.linux(1.3, 0), self.linux(1.4, 0))
            self.assertEqual(ld.env_notes(d), [])

    def test_a_linux_throttle_counter_that_advanced_is_reported_as_events(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "felis", self.linux(0.4, 7), self.linux(0.4, 7))
            self.write(d, "kitty", self.linux(0.4, 7), self.linux(0.4, 9))
            (note,) = ld.env_notes(d)
            self.assertEqual(
                note, "The CPU was throttled during: kitty (+2 throttling events)."
            )

    def test_a_macos_speed_limit_below_100_at_either_end_is_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "felis", self.darwin(0.4, 100), self.darwin(0.4, 100))
            self.write(d, "kitty", self.darwin(0.4, 100), self.darwin(0.4, 70))
            (note,) = ld.env_notes(d)
            self.assertIn("kitty (CPU speed limit 70%)", note)

    def test_a_machine_with_no_throttle_reading_claims_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            unavailable = {"loadavg": 0.4, "throttle": "unavailable"}
            self.write(d, "felis", unavailable, unavailable)
            self.assertEqual(ld.env_notes(d), [])

    def test_the_baseline_is_the_first_leg_of_the_first_round(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            for index in (1, 2):
                (d / f"round-{index}").mkdir()
            (d / "round-1" / "order").write_text("kitty\nfelis\n")
            self.write(d / "round-1", "kitty", self.linux(0.2, 0), self.linux(0.3, 0))
            self.write(d / "round-1", "felis", self.linux(0.3, 0), self.linux(0.3, 0))
            self.write(d / "round-2", "felis", self.linux(1.9, 0), self.linux(2.0, 0))
            (note,) = ld.env_notes(d)
            self.assertIn("was 0.2 at the first leg on record", note)
            self.assertIn("felis round 2 2", note)

    def test_the_baseline_is_the_load_the_run_started_under(self):
        # A background job that arrives during an earlier suite is
        # already in this suite's first leg, so a per-suite baseline
        # would take the raised load for the new normal.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "meta.json").write_text(
                json.dumps({"machine": {"load_average": 0.3}})
            )
            suite = root / "cat"
            suite.mkdir()
            (suite / "order").write_text("felis\nkitty\n")
            self.write(suite, "felis", self.linux(2.0, 0), self.linux(2.1, 0))
            self.write(suite, "kitty", self.linux(2.2, 0), self.linux(2.3, 0))
            (note,) = ld.env_notes(suite)
            self.assertIn("was 0.3 when the run started", note)
            self.assertIn("felis 2.1", note)
            self.assertIn("kitty 2.3", note)

    def test_a_refused_leg_is_named_with_its_reason(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "order").write_text("felis\nkitty\n")
            self.write(d, "felis", self.linux(0.4, 0), self.linux(0.4, 0))
            (d / "kitty.env.json").write_text(
                json.dumps(
                    {
                        "before": self.linux(0.4, 0),
                        "refused": "the screen was locked before the leg",
                    }
                )
            )
            (note,) = ld.env_notes(d)
            self.assertIn("kitty (the screen was locked before the leg)", note)
            self.assertNotIn("felis", note)

    def test_a_missing_throttle_file_is_unavailable_rather_than_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(envinfo.linux_throttle(Path(tmp)), "unavailable")
            cpu = Path(tmp) / "cpu0" / "thermal_throttle"
            cpu.mkdir(parents=True)
            (cpu / "core_throttle_count").write_text("3\n")
            (cpu / "package_throttle_count").write_text("1\n")
            self.assertEqual(
                envinfo.linux_throttle(Path(tmp)), {"kind": "counters", "events": 4}
            )


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


@presenting
class StartupSuiteTest(unittest.TestCase):
    """`startup` never opens a leg through `open_leg`, so it owns its own marker."""

    def drive(
        self,
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
        with (
            mock.patch.object(suites.firstwindow, "select", lambda: watcher),
            mock.patch.object(
                suites.fieldmod,
                "launch_terminal",
                lambda *_a: mock.Mock(pid=KITTY_PID),
            ),
            mock.patch.object(suites.subprocess, "Popen", daemon.launch),
            mock.patch.object(suites, "stop_launch", daemon.stop),
            mock.patch.object(suites.fieldmod, "hold_felis_daemon", daemon.hold),
            mock.patch.object(suites.fieldmod, "stop", daemon.terminate),
            mock.patch.object(suites, "felis_kill_sessions", lambda *_a: None),
            mock.patch.object(
                suites.fieldmod,
                "felis_home",
                lambda _pres: (Path(tempfile.mkdtemp()), Path(tempfile.mkdtemp())),
            ),
            mock.patch.object(suites.fieldmod, "felis_daemon_pid", daemon.bound),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(err or io.StringIO()),
        ):
            suites.suite_startup(
                fld, results, {"RUNS": str(runs), "WARMUP": str(warmup)}
            )
        return watcher

    def test_warmup_launches_are_discarded(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            self.drive(results, {KITTY_PID, FELIS_PID}, runs=3, warmup=2)
            record = json.loads((results / "kitty.startup.json").read_text())
            self.assertEqual(len(record["samples_ms"]), 3)
            self.assertNotIn("failed", record)
            self.assertGreater(record["samples_ms"][0], 0)

    def test_a_measured_leg_leaves_a_marker_and_is_not_run_twice(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            self.drive(results, {KITTY_PID, FELIS_PID})
            self.assertTrue((results / "kitty.done").exists())
            self.assertTrue((results / "felis.done").exists())
            self.assertEqual(self.drive(results, {KITTY_PID}).waits, [])

    def test_felis_writes_a_cold_and_a_warm_row(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            self.drive(results, {KITTY_PID, FELIS_PID})
            for row in ("felis-cold", "felis-warm"):
                record = json.loads((results / f"{row}.startup.json").read_text())
                self.assertEqual(len(record["samples_ms"]), 2)

    def test_every_warm_launch_finds_the_daemon_the_first_one_spawned(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            daemon = FakeDaemon()
            self.drive(results, {KITTY_PID, FELIS_PID}, runs=2, warmup=1, daemon=daemon)
            warm = json.loads((results / "felis-warm.startup.json").read_text())
            cold = json.loads((results / "felis-cold.startup.json").read_text())
            self.assertNotIn("failed", warm)
            self.assertEqual(warm["daemon_checked"], 3)
            self.assertEqual(cold["daemon_checked"], 3)
            # Three cold launches, then the one that primed the warm row.
            self.assertEqual(daemon.spawned, 4)
            # The priming launch and all three warm ones spare that daemon.
            self.assertEqual(daemon.spared[-4:], [[1004]] * 4)

    def test_a_warm_row_whose_daemon_died_with_its_launch_fails(self):
        # What happened before the daemon was spared: on macOS the stop
        # killed it, and every "warm" launch spawned a fresh one.
        class Unspared(FakeDaemon):
            def stop(self, proc, spare=()):
                super().stop(proc, ())

        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            self.drive(results, {KITTY_PID, FELIS_PID}, daemon=Unspared())
            warm = json.loads((results / "felis-warm.startup.json").read_text())
            self.assertIn("warm daemon", warm["failed"])
            self.assertIn("before launch 1 of 3", warm["failed"])
            self.assertEqual(warm["samples_ms"], [])

    def test_a_cold_launch_that_would_find_a_daemon_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            daemon = FakeDaemon(stubborn=True)
            daemon.pid = 999
            self.drive(results, {KITTY_PID, FELIS_PID}, daemon=daemon)
            cold = json.loads((results / "felis-cold.startup.json").read_text())
            self.assertIn("survived SIGTERM", cold["failed"])
            # A daemon that ignores SIGTERM is a finding, not a process
            # to SIGKILL out of the way.
            self.assertNotIn(True, daemon.escalations)
            # Nor is the survivor mistaken for a daemon the warm row primed.
            warm = json.loads((results / "felis-warm.startup.json").read_text())
            self.assertIn("before the priming launch", warm["failed"])

    def test_a_leg_whose_display_cannot_present_is_never_launched(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            asleep = {"locked": False, "asleep": True}
            err = io.StringIO()
            with mock.patch.object(envinfo, "display_state", lambda: asleep):
                watcher = self.drive(results, {KITTY_PID, FELIS_PID}, err=err)
            self.assertEqual(watcher.waits, [])
            self.assertIn("REFUSED: felis", err.getvalue())
            for name in ("kitty", "felis"):
                self.assertFalse((results / f"{name}.done").exists())
                record = json.loads((results / f"{name}.env.json").read_text())
                self.assertEqual(
                    record["refused"], "the display was asleep before the leg"
                )

    def test_a_result_a_crashed_attempt_left_is_not_charted_beside_a_refusal(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            (results / "kitty.startup.json").write_text("{}")
            (results / "felis-warm.startup.json").write_text("{}")
            locked = {"locked": True, "asleep": False}
            with mock.patch.object(envinfo, "display_state", lambda: locked):
                self.drive(results, {KITTY_PID, FELIS_PID})
            self.assertEqual(ld.collect(results, "startup"), [])
            self.assertEqual(list(results.glob("*.startup.json")), [])

    def test_a_priming_launch_that_raises_leaves_no_earlier_warm_row(self):
        class Unspawnable(FakeDaemon):
            def launch(self, *a, **k):
                if self.spawned == 3:
                    raise OSError("fork failed")
                return super().launch(*a, **k)

        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            (results / "felis-warm.startup.json").write_text(
                json.dumps({"samples_ms": [5.0, 6.0]})
            )
            with self.assertRaises(OSError):
                self.drive(results, {KITTY_PID, FELIS_PID}, daemon=Unspawnable())
            (startup,) = ld.collect(results, "startup")
            self.assertEqual(sorted(startup.data), ["kitty"])
            self.assertFalse((results / "felis.done").exists())

    def test_a_leg_that_ended_unable_to_present_lands_in_refused(self):
        # kitty runs first: before, after; then felis: before, after.
        states = iter(
            [PRESENTING, {"locked": True, "asleep": False}, PRESENTING, PRESENTING]
        )
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            with mock.patch.object(envinfo, "display_state", lambda: next(states)):
                self.drive(results, {KITTY_PID, FELIS_PID})
            self.assertFalse((results / "kitty.startup.json").exists())
            self.assertTrue((results / "refused" / "kitty.startup.json").exists())
            self.assertFalse((results / "kitty.done").exists())
            self.assertTrue((results / "felis-warm.startup.json").exists())
            self.assertTrue((results / "felis.done").exists())

    def test_stopping_a_launch_leaves_the_spared_process_alone(self):
        tree = {10: [20], 20: [30]}
        held = {pid: FakeHeld(pid) for pid in (10, 20, 30)}
        with (
            mock.patch.object(
                suites.fieldmod,
                "held_children",
                lambda parent, known: [
                    held[pid]
                    for pid in tree.get(parent.pid, [])
                    if held[pid].alive() and pid not in known
                ],
            ),
            mock.patch.object(suites.time, "sleep", lambda _s: None),
        ):
            with mock.patch.object(suites.fieldmod.Held, "child", lambda _p: held[10]):
                suites.stop_launch(mock.Mock(pid=10), spare=[20])
        self.assertEqual(held[20].signals, [])
        self.assertEqual(held[10].signals, [signal.SIGTERM])
        self.assertEqual(held[30].signals, [signal.SIGTERM])

    def test_a_terminal_that_never_maps_a_window_is_a_failure_not_a_number(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            watcher = self.drive(results, {FELIS_PID})
            record = json.loads((results / "kitty.startup.json").read_text())
            self.assertIn("no window", record["failed"])
            self.assertEqual(record["samples_ms"], [])
            # The first silent launch ends the leg rather than timing out
            # once per run.
            self.assertEqual(watcher.waits.count(KITTY_PID), 1)
            # Recorded, so a resume does not spend the timeout again.
            self.assertTrue((results / "kitty.done").exists())

    def test_each_leg_records_the_machine_it_ran_on(self):
        with tempfile.TemporaryDirectory() as tmp:
            results = Path(tmp)
            self.drive(results, {KITTY_PID})
            record = json.loads((results / "kitty.env.json").read_text())
            self.assertIn("loadavg", record["before"])
            self.assertIn("loadavg", record["after"])

    def test_the_felis_roster_is_read_for_its_ids(self):
        roster = json.dumps({"sessions": [{"id": "ab12"}, {"id": "cd34"}]})
        self.assertEqual(suites.session_ids(roster), ["ab12", "cd34"])
        self.assertEqual(suites.session_ids("not json"), [])


class FirstWindowTest(unittest.TestCase):
    def test_the_roster_niri_sends_first_replaces_what_was_known(self):
        windows, full = firstwindow.niri_windows(
            {"WindowsChanged": {"windows": [{"id": 1, "pid": 5}]}}
        )
        self.assertTrue(full)
        self.assertEqual([w["id"] for w in windows], [1])

    def test_an_opened_window_is_one_window(self):
        windows, full = firstwindow.niri_windows(
            {"WindowOpenedOrChanged": {"window": {"id": 7, "pid": 9}}}
        )
        self.assertFalse(full)
        self.assertEqual(windows, [{"id": 7, "pid": 9}])

    def test_other_events_name_no_window(self):
        self.assertEqual(
            firstwindow.niri_windows({"WorkspaceActivated": {"id": 1}}), ([], False)
        )

    def test_a_window_is_the_launch_s_when_its_pid_is_in_the_tree(self):
        with mock.patch.object(
            firstwindow.field, "process_tree", lambda _pid: {10, 11}
        ) as _tree:
            mine = firstwindow.TreeMatcher(10)
            self.assertTrue(mine(10))
            self.assertTrue(mine(11))
            self.assertFalse(mine(12))
            self.assertFalse(mine(None))


class ExplicitGridTest(unittest.TestCase):
    def test_an_explicit_pair_needs_a_pin_that_can_resize(self):
        # felis has no size flag, so under a floating desktop the
        # request would reach every terminal except the one the rest are
        # being compared against.
        with self.assertRaises(ValueError):
            ct.explicit_grid({"GRID_ROWS": "40", "GRID_COLS": "120"}, wm.FloatingPin())

    def test_half_a_pair_is_a_usage_error(self):
        with self.assertRaises(ValueError):
            ct.explicit_grid({"GRID_ROWS": "40"}, wm.NiriPin())

    def test_neither_leaves_the_reference_in_charge(self):
        self.assertIsNone(ct.explicit_grid({}, wm.FloatingPin()))


if __name__ == "__main__":
    unittest.main()

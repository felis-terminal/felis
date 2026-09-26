#!/usr/bin/env python3
"""What a terminal spends while a throughput workload runs through it.

The throughput suites say how fast a terminal drains a workload; this
says what that cost it. A sampler thread watches one leg's processes
from the wrapper's `<name>.start` marker to its `<name>.end` marker —
the measured pass only, not the discarded warm-up — and records:

- **CPU time**: the growth of the processes' `utime + stime` over that
  window, beside the window's wall time so a share can be derived.
  Only what is charged to the sampled processes counts: work a GPU
  driver does in its own kernel threads, or the compositor does on the
  terminal's behalf, is not.
- **Memory**: the platform's own ledger (`field.memory_sample`: PSS on
  Linux, `phys_footprint` on macOS), sampled every `INTERVAL` seconds,
  as its maximum and mean over the window. The maximum is of the
  samples, not a kernel high-water mark: the one macOS keeps is the
  startup peak, which no workload is responsible for.

The window is a handshake, not two timestamps: the wrapper touches
`.start` and waits for `.started`, which the sampler writes only once it
has resolved the process set and taken the baseline reading. A workload
that finishes in less time than resolving takes (a fast terminal drains
a small payload in tens of milliseconds) would otherwise be over before
the first reading and chart as zero. After `.end` the window stays open
until the terminal goes idle (`TAIL_MAX` at most), because a producer
can finish before the terminal has parsed what it wrote: `cat` returns
once its last byte is in the pty, and a terminal that reads eagerly
into its own buffer is still working.

The process set is the memory suite's: felis's client and its daemon,
recorded separately and summed; every other terminal's launch pid and
the children `field.child_pids` counts as the terminal's own.
"""

from __future__ import annotations

import json
import shlex
import threading
import time
from collections.abc import Callable
from pathlib import Path

import field

# Four samples a second: fine enough that a one-second workload is still
# a handful of readings, coarse enough that walking a large terminal's
# page tables for PSS stays out of what is being measured.
INTERVAL = 0.25
# How often the markers are looked for between samples, so the window
# closes within a tick of the workload ending rather than a sample.
POLL = 0.02
# After the end marker the window stays open while the terminal keeps
# working, up to this long. Idle is a sample interval in which the
# processes used less than `IDLE_SHARE` of one core; a terminal that
# never drops that low (one that repaints on a timer) is cut off here.
TAIL_MAX = 3.0
IDLE_SHARE = 0.05


def wrap(results: Path, name: str, workload: list[str]) -> list[str]:
    """The wrapper lines that put `workload` inside a sampled window.

    The wait for `.started` is bounded so a sampler that died does not
    hold the window open forever; the workload then runs unsampled and
    the leg reports why.
    """
    start, started, end = (
        shlex.quote(str(results / f"{name}.{which}"))
        for which in ("start", "started", "end")
    )
    return [
        f"touch {start}",
        f"for i in $(seq 500); do [ -e {started} ] && break; sleep 0.01; done",
        *workload,
        f"touch {end}",
    ]


MARKERS = ("start", "started", "end")


def reading(pids: list[int]) -> tuple[float, int] | None:
    """CPU seconds and memory in KB for a set of pids, or None if any went missing."""
    cpu = 0.0
    for pid in pids:
        seconds = field.cpu_seconds(pid)
        if seconds is None:
            return None
        cpu += seconds
    memory = field.memory_sample(pids)
    return None if memory is None else (cpu, memory.kb)


def summarize(readings: list[tuple[float, int]], wall: float) -> dict:
    """One part's window: CPU growth, and the sampled memory's peak and mean."""
    memory = [kb for _cpu, kb in readings]
    return {
        "cpu_s": round(readings[-1][0] - readings[0][0], 4),
        "wall_s": round(wall, 4),
        "peak_kb": max(memory),
        "mean_kb": round(sum(memory) / len(memory)),
        "samples": len(readings),
    }


def record(
    parts: dict[str, list[int]], series: dict[str, list[tuple[float, int]]], wall: float
) -> dict:
    """The leg's `.res.json`: each part, and their sum where there is more than one."""
    out = {
        "metric": "phys_footprint" if field.DARWIN else "pss",
        "interval_s": INTERVAL,
        "pids": parts,
    }
    summaries = {name: summarize(series[name], wall) for name in parts}
    if len(parts) == 1:
        out |= next(iter(summaries.values()))
        return out
    # Summed per sample, not per part, so the peak is one the leg actually
    # held rather than two maxima from different moments added together.
    total = [
        (sum(series[n][i][0] for n in parts), sum(series[n][i][1] for n in parts))
        for i in range(min(len(s) for s in series.values()))
    ]
    out |= summarize(total, wall)
    out["parts"] = summaries
    return out


class Sampler(threading.Thread):
    """Samples one leg between its start and end markers, then writes the record."""

    def __init__(
        self,
        results: Path,
        name: str,
        resolve: Callable[[], dict[str, list[int]]],
        alive: Callable[[], bool],
        timeout: float,
    ) -> None:
        super().__init__(daemon=True)
        self.results = results
        self.name = name
        self.resolve = resolve
        self.alive = alive
        self.timeout = timeout
        self.stop = threading.Event()
        self.outcome: str | None = None

    def marker(self, which: str) -> Path:
        return self.results / f"{self.name}.{which}"

    def run(self) -> None:
        try:
            self.outcome = self.sample()
        except Exception as err:  # noqa: BLE001
            # A sampler that dies silently would read as a leg with no
            # resource record and no reason; the leg itself is unharmed.
            self.outcome = f"sampler failed: {err}"

    def sample(self) -> str | None:
        deadline = time.monotonic() + self.timeout
        start, end = self.marker("start"), self.marker("end")
        while not start.exists():
            if self.stop.is_set() or not self.alive() or time.monotonic() > deadline:
                return "the workload never started"
            time.sleep(POLL)
        parts = self.resolve()
        series: dict[str, list[tuple[float, int]]] = {name: [] for name in parts}

        def take() -> str | None:
            for name, pids in parts.items():
                got = reading(pids)
                if got is None:
                    return f"{name} ({pids}) became unreadable during the workload"
                series[name].append(got)
            return None

        if why := take():
            return why
        began = time.monotonic()
        self.marker("started").touch()
        ended = None
        while True:
            tick = time.monotonic() + INTERVAL
            while time.monotonic() < tick and (ended or not end.exists()):
                if self.stop.is_set() or time.monotonic() > deadline:
                    return "the workload never ended"
                time.sleep(POLL)
            before = sum(s[-1][0] for s in series.values())
            if why := take():
                return why
            now = time.monotonic()
            if ended is None:
                # The interval that saw the marker was cut short by it, so
                # it says nothing about whether the terminal is idle yet.
                if end.exists():
                    ended = now
                continue
            spent = sum(s[-1][0] for s in series.values()) - before
            if spent < IDLE_SHARE * INTERVAL or now - ended >= TAIL_MAX:
                break
        payload = record(parts, series, time.monotonic() - began)
        payload["tail_s"] = round(time.monotonic() - ended, 4)
        (self.results / f"{self.name}.res.json").write_text(
            json.dumps(payload, indent=2) + "\n"
        )
        return None

    def finish(self, grace: float = TAIL_MAX + 5.0) -> str | None:
        """Wait for the record, and say why there is none if there is none."""
        self.join(grace)
        if self.is_alive():
            self.stop.set()
            self.join(1.0)
            return "the sampler did not finish"
        return self.outcome

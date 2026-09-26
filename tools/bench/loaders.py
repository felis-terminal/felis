#!/usr/bin/env python3
"""Load cross-terminal benchmark artifacts into report suites.

The window-driving code writes raw files per suite; this module is the
translation layer between those artifacts and `report.py`'s presentation
model. Keeping parsers here lets `crossterm.py` stay an orchestrator and
keeps the report renderer free of harness-specific file formats.
"""

from __future__ import annotations

import json
import math
import re
import statistics
from dataclasses import dataclass, field as dc_field
from pathlib import Path

import field
from report import Point, Suite, median

# ── the results layout, resolved once ────────────────────────────────

ROUND_DIR = re.compile(r"^round-(\d+)$")
# What a suite directory holds that belongs to the run rather than to a
# leg, and so says nothing about which layout it is in.
LAYOUT_NEUTRAL = {"run.log"}


def rounds(results: Path) -> list[Path]:
    """The round directories under a suite directory, in order.

    One resolver for every artifact reader, because a single-round run
    writes its legs directly under the suite directory and a repeated
    one writes them under `round-N/`: a per-reader glob would have to
    know both, and the ones that read `grid-mismatch` or `*.size`
    rather than a harness artifact were the ones that would quietly
    stop finding anything. A directory with no `round-*` is an implicit
    round 1, which is what keeps every results root written before
    rounds existed readable.
    """
    if not results.is_dir():
        return [results]
    numbered = [(ROUND_DIR.match(p.name), p) for p in results.iterdir()]
    dirs = [(int(m.group(1)), p) for m, p in numbered if m and p.is_dir()]
    if not dirs:
        return [results]
    stray = sorted(
        p.name
        for m, p in numbered
        if not m and p.name not in LAYOUT_NEUTRAL and not p.name.startswith(".")
    )
    if stray:
        raise ValueError(
            f"{results} holds both layouts: round directories beside "
            f"{', '.join(stray)}. A root is one or the other; move the loose "
            "artifacts into a round directory or read them from a copy."
        )
    return [p for _, p in sorted(dirs)]


def per_round(suite: Suite, results: Path, parse) -> None:
    """Fill one suite from every round, tagging each leg with its round."""
    directories = rounds(results)
    suite.rounds = len(directories)
    for index, directory in enumerate(directories, 1):
        parse(directory, index)


def whisker_note(suite: Suite) -> None:
    """Name the whisker in the subtitle, so one chart carries one meaning."""
    if suite.rounds > 1:
        suite.subtitle += (
            f" Bars are the median over {suite.rounds} rounds and whiskers "
            "their min-max range across them."
        )
    elif any(
        point.err for per_cat in suite.data.values() for point in per_cat.values()
    ):
        suite.subtitle += " One round, so a whisker is the within-run spread."


# ── loaders — one per harness artifact ───────────────────────────────

# kitten colors the rate it prints, and the artifact is its raw stdout.
ANSI = re.compile(r"\x1b\[[0-9;:]*[A-Za-z]")


def pixels_apart(sizes: dict[str, field.Size]) -> str:
    """Terminals whose render area misses felis's by more than a cell.

    A cell of tolerance per axis, because padding is sub-cell by
    construction while a font that did not apply moves the window by at
    least one — the same rule `field.verify_grid` holds a live leg to.
    """
    if not sizes:
        return ""
    reference = sizes.get("felis") or next(iter(sizes.values()))
    wide = reference.xpixel / reference.cols if reference.cols else 0
    high = reference.ypixel / reference.rows if reference.rows else 0
    if not (wide and high):
        return ""
    off = [
        f"{name} {size.xpixel}x{size.ypixel} px"
        for name, size in sorted(sizes.items())
        if abs(size.xpixel - reference.xpixel) > wide
        or abs(size.ypixel - reference.ypixel) > high
    ]
    if not off:
        return ""
    return f"{', '.join(off)} against felis's {reference.xpixel}x{reference.ypixel} px"


def grid_note(results: Path) -> list[str]:
    """What the field was actually pinned to, read back from the run.

    Read from each terminal's recorded `.size` rather than from the
    parameters, so the note reports the condition the numbers were taken
    under and not the one they were asked for; a terminal that landed
    somewhere else says so here instead of hiding behind the flag.

    Pixels are a second column, not a replacement: the same cell count
    drawn in a different area is a font pin that did not apply, and cell
    counts cannot see it. A terminal that leaves TIOCGWINSZ's pixel
    fields at zero is verified by cells alone and named — that is a gap
    in what could be checked, not a mismatch.
    """
    sizes = leg_sizes(results)
    if not sizes:
        return []
    cells = {s.cells() for s in sizes.values()}
    if len(cells) == 1:
        rows, cols = cells.pop()
        note = f"Every terminal ran at the same grid: {cols}x{rows}."
    else:
        listed = ", ".join(f"{n} {s.cols}x{s.rows}" for n, s in sorted(sizes.items()))
        note = (
            f"GRIDS DIFFERED, so these bars are not comparable: {listed}. "
            f"Re-run with the window manager stopped."
        )
    if silent := [n for n, s in sorted(sizes.items()) if not s.has_pixels]:
        note += (
            f" Pixels unreported by {', '.join(silent)}, so those bars are "
            "verified by cell count alone."
        )
    if apart := pixels_apart({n: s for n, s in sizes.items() if s.has_pixels}):
        note += f" The cells were not the same size: {apart}."
    for recorded in mismatch_notes(results):
        note += " " + recorded.rstrip(".") + "."
    return [note]


def leg_sizes(results: Path) -> dict[str, field.Size]:
    """Each leg's recorded geometry, over every round the run has.

    A terminal that took the same grid in every round is one entry; one
    that did not is one entry per round, because that difference is
    exactly what the note exists to show.
    """
    per_name: dict[str, list[tuple[int, field.Size]]] = {}
    for index, directory in enumerate(rounds(results), 1):
        for path in sorted(directory.glob("*.size")):
            if size := field.read_size(path):
                per_name.setdefault(path.stem, []).append((index, size))
    sizes = {}
    for name, seen in per_name.items():
        if len({size.cells() for _, size in seen}) == 1:
            sizes[name] = seen[0][1]
            continue
        for index, size in seen:
            sizes[f"{name} round {index}"] = size
    return sizes


def mismatch_notes(results: Path) -> list[str]:
    """What each round recorded about legs that missed the pin, one per line."""
    directories = rounds(results)
    out = []
    for index, directory in enumerate(directories, 1):
        path = directory / "grid-mismatch"
        if not path.exists():
            continue
        for line in path.read_text().splitlines():
            if text := " ".join(line.split()):
                out.append(f"round {index}: {text}" if len(directories) > 1 else text)
    return out


def has_legs(directory: Path) -> bool:
    """Does this directory hold a leg's artifacts rather than only a layout?"""
    return any(
        path.is_file() and path.name not in LAYOUT_NEUTRAL
        for path in directory.iterdir()
    )


# ── the machine each leg ran on, sampled per leg ─────────────────────


def leg_envs(results: Path) -> list[tuple[str, dict]]:
    """Every leg's environment sample, in the order the legs ran.

    The order is what makes the first sample the run's baseline, so it
    comes from the round's own `order` artifact rather than from a
    directory listing.
    """
    out = []
    directories = rounds(results)
    for index, directory in enumerate(directories, 1):
        found = {
            path.name.removesuffix(".env.json"): path
            for path in directory.glob("*.env.json")
        }
        order = read_order(directory)
        for name in order + sorted(n for n in found if n not in order):
            if (path := found.get(name)) is None:
                continue
            try:
                record = json.loads(path.read_text())
            except json.JSONDecodeError:
                continue
            label = f"{name} round {index}" if len(directories) > 1 else name
            out.append((label, record))
    return out


def read_order(directory: Path) -> list[str]:
    path = directory / "order"
    if not path.exists():
        return []
    return [line.strip() for line in path.read_text().splitlines() if line.strip()]


def throttle_note(record: dict) -> str | None:
    """Whether the platform said this leg ran throttled, in its own terms.

    The two platforms expose different shapes — macOS reports a speed
    limit it is holding the CPU to, Linux a counter of throttling events
    — so a leg carries whichever it saw and neither is inferred from the
    other. A machine that exposes neither says nothing rather than
    "not throttled".
    """
    before = (record.get("before") or {}).get("throttle")
    after = (record.get("after") or {}).get("throttle")
    if not isinstance(before, dict) or not isinstance(after, dict):
        return None
    if before.get("kind") == "counters":
        advanced = (after.get("events") or 0) - (before.get("events") or 0)
        return f"+{advanced} throttling events" if advanced > 0 else None
    if before.get("kind") == "speed_limit":
        limits = [
            value
            for value in (before.get("cpu_speed_limit"), after.get("cpu_speed_limit"))
            if value is not None
        ]
        if limits and min(limits) < 100:
            return f"CPU speed limit {min(limits)}%"
    return None


def env_notes(results: Path) -> list[str]:
    """Legs that were taken under a different machine than the first one.

    A run is over an hour long, so the machine at leg twelve is not the
    machine `meta.json` recorded at leg one; a leg that ran beside a
    background job or on a throttled CPU is otherwise indistinguishable
    from a slow terminal.
    """
    samples = leg_envs(results)
    if not samples:
        return []
    # The load the run started under, not the load this suite started
    # under: a background job that arrives during the third suite is
    # already in the first leg of the fourth, and comparing a suite
    # against itself would call that the new normal.
    baseline = (run_meta(results).get("machine") or {}).get("load_average")
    started = "when the run started"
    if baseline is None:
        baseline = (samples[0][1].get("before") or {}).get("loadavg")
        started = "at the first leg on record"
    busy, throttled, refused = [], [], []
    for label, record in samples:
        if reason := record.get("refused"):
            refused.append(f"{label} ({reason})")
        loads = [
            value
            for mark in ("before", "after")
            if (value := (record.get(mark) or {}).get("loadavg")) is not None
        ]
        if baseline is not None and loads and max(loads) > baseline + 1.0:
            busy.append(f"{label} {max(loads):g}")
        if reason := throttle_note(record):
            throttled.append(f"{label} ({reason})")
    notes = []
    if refused:
        notes.append(
            "Refused, because the display could not present a frame, so these "
            f"legs have no bar: {', '.join(refused)}. A window that cannot "
            "present does not draw, and what it spends instead is not what the "
            "chart measures; resuming the run retakes them."
        )
    if busy:
        notes.append(
            f"Load average was {baseline:g} {started}; these legs ran more "
            f"than 1.0 above it: {', '.join(busy)}. A busy machine reads as a "
            "slow terminal."
        )
    if throttled:
        notes.append(f"The CPU was throttled during: {', '.join(throttled)}.")
    return notes


def condition_notes(results: Path) -> list[str]:
    """Everything about how a leg was taken, rather than what it measured."""
    return grid_note(results) + env_notes(results)


def load_vtebench(results: Path) -> list[Suite]:
    """`vtebench --dat`: a header of benchmark names, then one row per run."""
    suite = Suite(
        key="vtebench",
        title="vtebench — time to drain a payload",
        subtitle="Renderer live (no DECSET 2026), so this includes PTY-read backpressure.",
        unit="ms",
        better="lower",
    )

    def parse(directory: Path, index: int) -> None:
        for dat in sorted(directory.glob("*.dat")):
            rows = [ln.split() for ln in dat.read_text().splitlines() if ln.strip()]
            if len(rows) < 2:
                continue
            names = rows[0]
            for i, name in enumerate(names):
                samples = [float(r[i]) for r in rows[1:] if i < len(r) and r[i] != "_"]
                if not samples:
                    continue
                mean = sum(samples) / len(samples)
                var = sum((s - mean) ** 2 for s in samples) / len(samples)
                suite.put(dat.stem, name, Point(mean, math.sqrt(var)), index)

    per_round(suite, results, parse)
    if not suite.data:
        return []
    whisker_note(suite)
    suite.notes = condition_notes(results) + [
        "Within a leg the value is the mean over vtebench's samples, and its "
        "± is one stddev of them.",
        "Benchmarks whose payload comes out empty in this environment are "
        "dropped in every terminal alike.",
    ]
    return [suite]


def load_termbench(results: Path) -> list[Suite]:
    """termbench-pro `tb --output`: [{name, "MB/s"}]."""
    suite = Suite(
        key="termbench",
        title="termbench-pro — throughput on fixed payloads",
        subtitle="Same bytes everywhere; the grid changes rendering load only.",
        unit="MB/s",
        better="higher",
    )

    def parse(directory: Path, index: int) -> None:
        for js in sorted(directory.glob("*.json")):
            try:
                entries = json.loads(js.read_text())
            except json.JSONDecodeError:
                continue
            if not isinstance(entries, list):
                continue
            for entry in entries:
                name = entry.get("name")
                if name is None:
                    continue
                try:
                    suite.put(js.stem, name, Point(float(entry.get("MB/s"))), index)
                except (TypeError, ValueError):
                    # tb writes a non-numeric marker ("stalled") for a category
                    # that never completes; a zero-length bar says that, a
                    # fabricated number would not.
                    suite.put(
                        js.stem, name, Point(0.0, note=str(entry.get("MB/s"))), index
                    )

    per_round(suite, results, parse)
    if not suite.data:
        return []
    whisker_note(suite)
    suite.notes = condition_notes(results)
    return [suite]


def load_cat(results: Path) -> list[Suite]:
    """hyperfine `--export-json`, one file per terminal, named per payload."""
    suite = Suite(
        key="cat",
        title="cat a large file",
        subtitle="Wall clock for `cat` to return — the pty drained, not the parse finished.",
        unit="s",
        better="lower",
    )

    def parse(directory: Path, index: int) -> None:
        for js in sorted(directory.glob("*.json")):
            try:
                payload = json.loads(js.read_text())
            except json.JSONDecodeError:
                continue
            for run in payload.get("results") or []:
                name = run.get("command")
                if name is None or "mean" not in run:
                    continue
                suite.put(
                    js.stem, name, Point(run["mean"], run.get("stddev", 0)), index
                )

    per_round(suite, results, parse)
    if not suite.data:
        return []
    whisker_note(suite)
    params = run_params(results)
    size = params.get("CAT_MB", "150")
    suite.notes = condition_notes(results) + [
        f"{size} MB per payload, generated from a fixed seed "
        f"(tools/bench/payloads.py), so every terminal drained the same bytes.",
        "`cat` returns once the last byte is written to the pty, so a terminal "
        "that buffers input it has not parsed yet finishes early here; the "
        "kittenbench chart is the one that waits for the parse.",
    ]
    return [suite]


def load_kittenbench(results: Path) -> list[Suite]:
    """`kitten __benchmark__` stdout: `<name> : <duration> @ <rate> MB/s`."""
    suite = Suite(
        key="kittenbench",
        title="kitten __benchmark__ — parse throughput",
        subtitle="Rendering suppressed (DECSET 2026); each pass ends in a DSR the "
        "terminal can only answer once it has parsed everything.",
        unit="MB/s",
        better="higher",
    )
    line = re.compile(r"^\s*(?P<name>.+?)\s*:.*?@\s*(?P<rate>[\d.]+)\s*MB/s")

    def parse(directory: Path, index: int) -> None:
        for out in sorted(directory.glob("*.kitten")):
            for raw in out.read_text(errors="replace").splitlines():
                if match := line.match(ANSI.sub("", raw)):
                    try:
                        rate = float(match["rate"])
                    except ValueError:
                        continue
                    suite.put(out.stem, match["name"], Point(rate), index)

    per_round(suite, results, parse)
    if not suite.data:
        return []
    whisker_note(suite)
    params = run_params(results)
    suite.notes = condition_notes(results) + [
        f"{params.get('KITTEN_REPS', '100')} repetitions per benchmark, after a "
        "discarded warm-up pass.",
    ]
    return [suite]


def load_doom_fire(results: Path) -> list[Suite]:
    """`<term>.doom-fire`: the line the patched DOOM-fire prints to stderr."""
    suite = Suite(
        key="doom-fire",
        title="DOOM-fire — full-screen animation",
        subtitle="Frames per second over a fixed run, counted by the app itself.",
        unit="fps",
        better="higher",
    )
    throughput = {}

    def parse(directory: Path, index: int) -> None:
        for out in sorted(directory.glob("*.doom-fire")):
            fields = out.read_text().split()
            values = {}
            for key, value in zip(fields[1::2], fields[2::2], strict=False):
                try:
                    values[key] = float(value)
                except ValueError:
                    continue
            if "fps" not in values:
                continue
            suite.put(out.stem, "DOOM-fire", Point(values["fps"]), index)
            if "bytes_avg" in values:
                rate = values["fps"] * values["bytes_avg"] / (1 << 20)
                # One figure per terminal: the note is about the payload
                # the app emitted, which the rounds do not change.
                throughput.setdefault(out.stem, f"{out.stem} {rate:.0f} MB/s")

    per_round(suite, results, parse)
    if not suite.data:
        return []
    whisker_note(suite)
    params = run_params(results)
    suite.notes = condition_notes(results) + [
        f"{params.get('DOOM_FIRE_SECS', '20')}s per terminal, after a discarded "
        f"{params.get('DOOM_FIRE_WARMUP_SECS', '5')}s pass; the app's counter is a "
        "cumulative average, which is why the warm-up is a separate run rather "
        "than discarded frames.",
        "The frame is sized from the tty, so a leg that missed the grid pin was "
        "drawing a different picture, not the same one slower.",
    ]
    if throughput:
        suite.notes.append(
            "Bytes emitted: "
            + ", ".join(v for _, v in sorted(throughput.items()))
            + "."
        )
    return [suite]


WARM = "felis (warm)"


STARTUP_TERMS = {"felis-cold": "felis", "felis-warm": WARM}


def load_startup(results: Path) -> list[Suite]:
    """`<term>.startup.json` launch-to-first-window samples.

    A results root taken before the suite timed windows holds hyperfine
    exports of a launch running `true` instead; those still chart, under
    a subtitle that says they are process lifetimes.
    """
    suite = Suite(
        key="startup",
        title="Time to first window",
        subtitle="",
        unit="ms",
        better="lower",
        labels={"felis": "felis (cold)"},
        variant_of={WARM: "felis"},
    )
    watchers: set[str] = set()
    failed: list[str] = []
    spread: list[str] = []
    legacy = False

    def parse(directory: Path, index: int) -> None:
        nonlocal legacy
        for js in sorted(directory.glob("*.startup.json")):
            try:
                payload = json.loads(js.read_text())
            except json.JSONDecodeError:
                continue
            stem = js.name.removesuffix(".startup.json")
            term = STARTUP_TERMS.get(stem, stem)
            label = suite.label(term) if term != WARM else WARM
            if suite.rounds > 1:
                label += f" round {index}"
            watchers.add(payload.get("watcher", "?"))
            if reason := payload.get("failed"):
                failed.append(f"{label}: {reason}")
                continue
            samples = sorted(payload.get("samples_ms") or [])
            if not samples:
                continue
            quartiles = (
                statistics.quantiles(samples, n=4) if len(samples) > 1 else [0, 0, 0]
            )
            suite.put(
                term,
                "first window",
                Point(statistics.median(samples), (quartiles[2] - quartiles[0]) / 2),
                index,
            )
            spread.append(
                f"{label} {samples[0]:.0f}-{samples[-1]:.0f} ({len(samples)})"
            )
        for js in sorted(directory.glob("*.json")):
            if js.name.endswith((".startup.json", ".env.json")):
                continue
            try:
                payload = json.loads(js.read_text())
            except json.JSONDecodeError:
                continue
            runs = payload.get("results") or []
            if not runs:
                continue
            legacy = True
            term = STARTUP_TERMS.get(js.stem, js.stem)
            suite.put(
                term,
                "launch",
                Point(runs[0]["mean"] * 1000, runs[0].get("stddev", 0) * 1000),
                index,
            )

    per_round(suite, results, parse)
    if legacy:
        suite.title = "Window launch latency"
        suite.subtitle = (
            "hyperfine over a launch that runs `true` and exits: a process "
            "lifetime, superseded by the time to the first window because a "
            "terminal can exit without ever showing one. On a tiler, work "
            "delaying first configure is included, but animation completion "
            "is not timed."
        )
    else:
        suite.subtitle = (
            "From launching the terminal to the platform reporting its first "
            "window: under niri, mapped with its first buffer committed; on "
            "macOS, ordered on screen, which does not prove the window has "
            "been drawn into. On a tiler, work delaying first configure is "
            "included, but animation completion is not timed."
        )
    if not suite.data and not failed:
        return []
    whisker_note(suite)
    params = run_params(results)
    notes = env_notes(results) + [
        "felis (cold) kills the bench daemon before every launch, so it "
        "carries the autospawn premium; felis (warm) reuses a primed one. "
        "Only felis has a daemon to be warm about.",
    ]
    if not legacy:
        notes.append(
            f"Median over {params.get('RUNS', '10')} launches after "
            f"{params.get('WARMUP', '3')} discarded ones; within a leg the ± is "
            "half the interquartile range. Each launch runs `sleep` so the "
            "window has a reason to stay, and is terminated once seen. "
            "Watched by: " + ", ".join(sorted(watchers)) + "."
        )
    if spread:
        notes.append("Range and launch count: " + ", ".join(spread) + ".")
    if failed:
        notes.append(
            "No number, because a launch showed no window: " + "; ".join(failed) + "."
        )
    suite.notes = notes
    return [suite] if suite.data else []


def load_latency(results: Path) -> list[Suite]:
    """`<term>.latency.json`: the per-character samples, in the order typed."""
    suite = Suite(
        key="latency",
        title="Keystroke to glyph on screen",
        subtitle="A key pressed through the desktop's own injection API, timed "
        "until the pixel where the glyph lands changes color.",
        unit="ms",
        better="lower",
    )
    spread = []
    covered = []
    failed = []
    # Named from the artifacts rather than from `sys.platform`: a results
    # root is rendered wherever it is opened, including on the other
    # platform, and the bars carry whichever instrument took them.
    instruments: set[str] = set()

    def parse(directory: Path, index: int) -> None:
        for path in sorted(directory.glob("*.latency-failed")):
            term = path.name.removesuffix(".latency-failed")
            label = f"{term} round {index}" if suite.rounds > 1 else term
            reasons = "; ".join(line for line in path.read_text().splitlines() if line)
            outcome = (
                "measured on a later attempt"
                if (directory / f"{term}.done").exists()
                else "not measured"
            )
            failed.append(f"{label} ({outcome}): {reasons}")
        for js in sorted(directory.glob("*.latency.json")):
            try:
                payload = json.loads(js.read_text())
            except json.JSONDecodeError:
                continue
            samples = payload.get("samples_ms") or []
            if not samples:
                continue
            instruments.add(payload.get("instrument", "typometer"))
            term = js.name.removesuffix(".latency.json")
            mean = sum(samples) / len(samples)
            var = sum((s - mean) ** 2 for s in samples) / len(samples)
            ordered = sorted(samples)
            # The tail is the half a reader feels; nearest-rank keeps it an
            # observed sample rather than an interpolation between two.
            tail = ordered[min(len(ordered) - 1, math.ceil(0.95 * len(ordered)) - 1)]
            suite.put(term, "mean", Point(mean, math.sqrt(var)), index)
            suite.put(term, "95th percentile", Point(tail), index)
            label = f"{term} round {index}" if suite.rounds > 1 else term
            spread.append(
                f"{label} {ordered[0]:.0f}-{ordered[-1]:.0f} ({len(samples)})"
            )
            if waited := payload.get("covered_before_key"):
                covered.append(f"{label} {waited}")

    per_round(suite, results, parse)
    if not suite.data:
        return []
    whisker_note(suite)
    params = run_params(results)
    suite.notes = condition_notes(results) + [
        f"{params.get('LATENCY_COUNT', '200')} characters per terminal, "
        f"{params.get('LATENCY_DELAY', '150')} ms apart, typed into `cat` so "
        "the echo comes from the tty line discipline rather than a shell's "
        "own redraw. Each keystroke waits for its glyph before the next one, "
        "so a sample is one keypress answered and never a backlog draining.",
        f"This suite runs at its own font size ({params.get('LATENCY_FONT_PT', '14')} "
        "pt, against the rest of the field's "
        f"{params.get('FONT_PT', '9')}) and re-probes the grid at that size: "
        "the pattern detector cannot find what it typed in a cell that small. "
        "Cell width does not enter a keystroke's journey to the screen, and "
        "every terminal here shares the one window area.",
        "Instrument: " + ", ".join(sorted(instruments)) + ". Typometer injects "
        "and reads the screen through the OS event API; `wl-latency` runs the "
        "same loop over `zwp_virtual_keyboard_v1` and `wlr-screencopy`, which "
        "is what a Wayland desktop offers an unprivileged client. Bars taken "
        "by different instruments, on different machines, are not one axis.",
        "Compositor, display and the measurement itself are inside every bar "
        "— on macOS that floor is over 10 ms, and on a Wayland desktop every "
        "sample is quantized by the output's frame period — and no terminal "
        "can be shown to beat either. The comparison between bars is the "
        "signal; the distance to zero is not.",
        "Range and sample count: " + ", ".join(spread) + ".",
    ]
    if covered:
        suite.notes.append(
            "Cells that were already painted over before their key went down, "
            "and cleared without a key: " + ", ".join(covered) + ". The "
            "instrument waits such a cell out before typing into it, so each one "
            "is a frame the terminal presented and then replaced, not a sample."
        )
    if failed:
        suite.notes.append(
            "The instrument failed these legs, and each got one more window: "
            + " | ".join(failed)
            + "."
        )
    return [suite]


def run_meta(results: Path) -> dict:
    """The run's own record, read back from meta.json beside the suite.

    A loader gets a suite directory, but everything recorded once for
    the whole run lives above it — so look one level up, one further
    for a loader handed a single `round-N` directory, and in place for
    `report --suite <dir>`.
    """
    for candidate in (
        results.parent / "meta.json",
        results.parent.parent / "meta.json",
        results / "meta.json",
    ):
        if candidate.exists():
            try:
                return json.loads(candidate.read_text())
            except json.JSONDecodeError:
                return {}
    return {}


def run_params(results: Path) -> dict[str, str]:
    """The pinned condition the whole run was held to."""
    return run_meta(results).get("params") or {}


# How the platform's own accounting reads on a chart axis. A metric
# with no entry is drawn under its raw name rather than silently under
# someone else's.
METRIC_LABEL = {
    "pss": "PSS (each shared page split among the processes mapping it)",
    "phys_footprint": "Physical footprint (what Activity Monitor shows)",
    "rss": "RSS via `ps` — the superseded metric, not comparable with newer runs",
}


def legacy_memory(results: Path) -> dict[str, dict]:
    """Runs taken before the metric changed, read rather than dropped.

    A results directory keeps working after the harness moves on; the
    alternative is a report that quietly loses its memory chart, which
    reads as a suite that was never run. The CPU marks are not
    recoverable — only the flooded total was recorded — so those runs
    keep their bars and lose that chart.
    """
    out = {}
    for mem in sorted(results.glob("*.mem")):
        parts = mem.read_text().split()
        if len(parts) != 3:
            continue
        idle_kb, flood_kb, _cpu = (float(p) for p in parts)
        out[mem.stem] = {"metric": "rss", "idle_kb": idle_kb, "flooded_kb": flood_kb}
    return out


def records_in(directory: Path) -> dict[str, dict]:
    """One round's memory records, or the pre-JSON ones it left instead."""
    found = {
        path.name.removesuffix(".mem.json"): json.loads(path.read_text())
        for path in sorted(directory.glob("*.mem.json"))
    }
    return found or legacy_memory(directory)


def median_round(per_round_records: list[dict[str, dict]]) -> dict[str, dict]:
    """The round whose felis leg sits in the middle, for the notes.

    The bars are aggregated across rounds, but the split, the detached
    daemon and the peaks are several numbers that only add up together
    — a median taken field by field would describe a run that never
    happened, so one real round is quoted and it is the one the bars
    are closest to.
    """
    populated = [records for records in per_round_records if records]
    if not populated:
        return {}
    ranked = sorted(
        populated, key=lambda records: (records.get("felis") or {}).get("flooded_kb", 0)
    )
    return ranked[(len(ranked) - 1) // 2]


def load_memory(results: Path) -> list[Suite]:
    """`<term>.mem.json`: the metric, both marks, and the CPU at each."""
    directories = rounds(results)
    per_round_records = [records_in(directory) for directory in directories]
    if not (records := median_round(per_round_records)):
        return []
    metric = next(iter(records.values())).get("metric", "rss")
    rss = Suite(
        key="memory",
        title="Memory held",
        subtitle=(
            f"{METRIC_LABEL.get(metric, metric)} when idle, and after 200k "
            "lines have gone through scrollback. Each number is what the OS "
            "charges to the processes the suite samples under its ledger: "
            "PSS (the process's proportional share of its resident pages) "
            "on Linux, `phys_footprint` (the process's physical-footprint "
            "ledger, the number Activity Monitor's Memory column shows) on "
            "macOS. Whatever the GPU driver allocates and the OS charges to "
            "the process is inside; whatever it holds elsewhere (device "
            "memory the process does not map, kernel-side driver state) is "
            "outside — and how the split falls depends on the API, the "
            "driver and the GPU, so a GPU terminal's bar carries a driver "
            "share the reader cannot separate from the rest, while a CPU "
            "renderer (foot) carries none."
        ),
        unit="MB",
        better="lower",
    )
    cpu = Suite(
        key="flood-cpu",
        title="CPU time to chew the flood",
        subtitle=(
            "`utime + stime` charged to the processes the suite samples — "
            "the launched process and the children named after it or its "
            "helpers, such as kitty's `kitten` (`field.child_pids`); "
            "felis's client and daemon — between the "
            "two marks. Work charged elsewhere (the compositor's "
            "processes, the GPU driver's separately scheduled kernel "
            "workers) is outside; a CPU renderer does all of its rendering "
            "in-process and is charged in full, a GPU terminal is charged "
            "only for what runs on its own threads, including the driver's "
            "synchronous kernel time."
        ),
        unit="s",
        better="lower",
    )
    rss.rounds = cpu.rounds = len(directories)
    for index, per_round_record in enumerate(per_round_records, 1):
        for name, record in per_round_record.items():
            rss.put(name, "idle", Point(record["idle_kb"] / 1024), index)
            rss.put(
                name,
                "after 200k-line flood",
                Point(record["flooded_kb"] / 1024),
                index,
            )
            if "flooded_cpu_cs" in record:
                spent = record["flooded_cpu_cs"] - record["idle_cpu_cs"]
                cpu.put(name, "200k-line flood", Point(spent / 100), index)
    whisker_note(rss)
    whisker_note(cpu)

    notes = condition_notes(results)
    if parts := (records.get("felis") or {}).get("parts"):
        client, daemon = parts["client"], parts["daemon"]
        notes.append(
            "felis is client+daemon summed — client "
            f"{client['idle_kb'] / 1024:.1f} → {client['flooded_kb'] / 1024:.1f} MB, "
            f"daemon {daemon['idle_kb'] / 1024:.1f} → "
            f"{daemon['flooded_kb'] / 1024:.1f} MB."
        )
        cpu.notes.append(
            "felis split: client "
            f"{(client['flooded_cpu_cs'] - client['idle_cpu_cs']) / 100:.2f}s, "
            f"daemon {(daemon['flooded_cpu_cs'] - daemon['idle_cpu_cs']) / 100:.2f}s."
        )
    if detached := (records.get("felis") or {}).get("detached"):
        notes.append(
            f"felis's detached daemon holds {detached['kb'] / 1024:.1f} MB and "
            f"burns {detached['cpu_cs'] / 100:.2f}s CPU over "
            f"{detached['seconds']}s idle after the window closes."
        )
    if peaks := {n: r["peak_kb"] for n, r in records.items() if r.get("peak_kb")}:
        listed = ", ".join(f"{n} {kb / 1024:.0f}" for n, kb in sorted(peaks.items()))
        notes.append(
            f"Peak resident size while the leg ran, in MB: {listed}. `VmHWM` is "
            "a high-water mark of RSS, not of the metric drawn above, and is "
            "summed over a leg's processes — it bounds the bars rather than "
            "extending them."
        )
    params = run_params(results)
    if escaped := params.get("SCROLLBACK_UNPINNED"):
        depth = params.get("SCROLLBACK", "the pinned depth")
        notes.append(
            f"Scrollback was pinned to {depth} rows everywhere except "
            f"{escaped}. A flooded bar is decided by the cap it hit, so "
            "that one is not equated with the rest."
        )
    rss.notes = notes
    # A legacy run recorded no idle mark, so there is no honest flood
    # column to draw for it.
    return [rss, cpu] if cpu.data else [rss]


# The suites whose legs carry a `<name>.res.json`, in the order the
# resource charts lay out their panels.
SAMPLED_SUITES = ("vtebench", "termbench", "cat", "kittenbench", "doom-fire")


def load_usage(root: Path, only: str | None = None) -> list[Suite]:
    """`<name>.res.json` from every sampled suite: CPU time and peak memory.

    Two charts, one panel per suite, because the two quantities are two
    units and every unit gets its own axis. A suite's panel compares
    terminals on the same workload; the panels do not compare with each
    other.
    """
    cpu = Suite(
        key="usage-cpu",
        title="CPU time spent on each throughput workload",
        subtitle=(
            "`utime + stime` the terminal's processes accrued from the start "
            "of the measured pass (not the discarded warm-up) until they went "
            "idle after it ended. felis is its client and its daemon summed; "
            "every other terminal is its launch process and the children "
            "`field.child_pids` counts as its own. Work done outside those "
            "processes is not counted: a GPU driver's own threads and the "
            "compositor's rendering on the terminal's behalf are invisible "
            "here, so a GPU terminal is charged less of its drawing than a "
            "CPU renderer (foot), which does all of it in-process."
        ),
        unit="s",
        better="lower",
    )
    memory = Suite(
        key="usage-memory",
        title="Peak memory during each throughput workload",
        subtitle="",
        unit="MB",
        better="lower",
    )
    metrics: set[str] = set()
    shares: dict[str, dict[str, list[float]]] = {}
    splits: dict[str, list[str]] = {}
    suites = [only] if only else [n for n in SAMPLED_SUITES if (root / n).is_dir()]
    rounds_seen = 1
    for suite_name in suites:
        base = root if only else root / suite_name
        directories = rounds(base)
        rounds_seen = max(rounds_seen, len(directories))
        for index, directory in enumerate(directories, 1):
            for path in sorted(directory.glob("*.res.json")):
                try:
                    record = json.loads(path.read_text())
                except json.JSONDecodeError:
                    continue
                if "cpu_s" not in record:
                    continue
                term = path.name.removesuffix(".res.json")
                metrics.add(record.get("metric", "?"))
                cpu.put(term, suite_name, Point(record["cpu_s"]), index)
                memory.put(term, suite_name, Point(record["peak_kb"] / 1024), index)
                if record.get("wall_s"):
                    shares.setdefault(suite_name, {}).setdefault(term, []).append(
                        100 * record["cpu_s"] / record["wall_s"]
                    )
                if parts := record.get("parts"):
                    splits.setdefault(suite_name, []).append(
                        f"{term}"
                        + (f" round {index}" if len(directories) > 1 else "")
                        + ": "
                        + ", ".join(
                            f"{part} {p['cpu_s']:.2f} s / {p['peak_kb'] / 1024:.0f} MB"
                            for part, p in parts.items()
                        )
                    )
    if not cpu.data:
        return []
    cpu.rounds = memory.rounds = rounds_seen
    whisker_note(cpu)
    whisker_note(memory)
    labels = ", ".join(METRIC_LABEL.get(m, m) for m in sorted(metrics))
    memory.subtitle = (
        f"The largest of the samples taken every 0.25 s over the measured "
        f"pass, under the platform's own ledger: {labels}. "
        "Not a kernel high-water mark: macOS keeps only the lifetime one, "
        "which is the terminal's startup, not the workload. felis is client "
        "and daemon summed sample by sample."
    )
    cpu.notes.append(
        "CPU share of the workload's wall time (median over rounds; above "
        "100% means more than one core): "
        + "; ".join(
            f"{suite_name}: "
            + ", ".join(
                f"{term} {median(values):.0f}%"
                for term, values in sorted(per_term.items())
            )
            for suite_name, per_term in shares.items()
        )
        + ". A terminal that finishes a fixed payload sooner spends less "
        "wall time on it, so compare the seconds above within a panel, and "
        "the share only as how hard the terminal worked while it ran."
    )
    for suite_name, lines in splits.items():
        cpu.notes.append(f"Split, {suite_name}: " + "; ".join(lines) + ".")
    return [cpu, memory]


@dataclass
class Harness:
    """One suite: how to measure it, where, and what it needs to do so."""

    load: object  # results dir → [Suite]
    # Platforms the suite can measure on. One missing here is reported
    # as skipped rather than silently absent from the charts.
    platforms: list[str] = dc_field(default_factory=lambda: ["darwin", "linux"])
    tools: list[str] = dc_field(default_factory=list)
    # Where one measurement takes a different instrument on each platform.
    tools_linux: list[str] | None = None

    def runs_on(self, platform: str) -> bool:
        return platform in self.platforms

    def needs(self, platform: str) -> list[str]:
        if platform == "linux" and self.tools_linux is not None:
            return self.tools_linux
        return self.tools


HARNESSES: dict[str, Harness] = {
    "vtebench": Harness(load=load_vtebench, tools=["vtebench"]),
    "termbench": Harness(load=load_termbench, tools=["tb"]),
    # cat and kittenbench carry their own bytes, so an unequal grid costs
    # them rendering load only and they need no Linux-specific pin.
    "cat": Harness(load=load_cat, tools=["hyperfine"]),
    "kittenbench": Harness(load=load_kittenbench, tools=["kitten"]),
    "doom-fire": Harness(load=load_doom_fire, tools=["doom-fire"]),
    "startup": Harness(load=load_startup),
    "memory": Harness(load=load_memory),
    # One measurement, one instrument per platform: Typometer injects and
    # reads the screen through X11, which on a Wayland desktop is a path
    # nobody's terminal takes, so the Linux half is measured by wl-latency
    # over the Wayland protocols niri hands an unprivileged client
    # (tools/bench/wl-latency, docs/explanation/benchmarks.md).
    "latency": Harness(
        load=load_latency, tools=["typometer"], tools_linux=["wl-latency"]
    ),
}


def collect(root: Path, only: str | None) -> list[Suite]:
    for suite in [root] if only else [p for p in root.iterdir() if p.is_dir()]:
        for directory in rounds(suite):
            field.finish_promotions(directory)
    if only:
        found = list(HARNESSES[only].load(root))
        return found + (load_usage(root, only) if only in SAMPLED_SUITES else [])
    found = []
    for name, harness in HARNESSES.items():
        if (root / name).is_dir():
            found += list(harness.load(root / name))
    return found + load_usage(root)

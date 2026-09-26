#!/usr/bin/env python3
"""The measuring suites, and the grid probe they are all pinned by.

Each suite drives real terminal windows through one workload and leaves
its raw artifacts in a results directory; `loaders.py` parses those
for the report. What "the same conditions" means — pristine configs, one
font family, one point size (felis takes it as its logical-pixel
`font.size`), one grid — is `field.py`, which every suite goes through so
no two of them can drift.

Shared shape: every leg of `field.legs()` runs the same wrapper script
through `field.open_leg`, felis among them (its own entry brings a
private daemon socket and a throwaway config home); every leg is pinned
to the reference grid and records the geometry it actually got, and a
leg that already left a `.done` marker is skipped so a crashed run
resumes.

Run one on its own:

    python3 tools/bench/suites.py vtebench --results /tmp/vtb
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import json
import os
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Collection
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import envinfo  # noqa: E402
import field as fieldmod  # noqa: E402
import firstwindow  # noqa: E402
import payloads  # noqa: E402
import usage  # noqa: E402
import wm  # noqa: E402
from field import Field  # noqa: E402

DARWIN = sys.platform == "darwin"

# A suite's per-terminal wrapper body: (terminal name, the attempt
# directory its artifacts go to) → shell lines.
Body = Callable[[str, Path], list[str]]

# A suite that drives the window from outside: (terminal name, launch
# pid, attempt directory) → did the measurement succeed. Only `latency`
# has one; every other suite is driven entirely by its wrapper script.
Driver = Callable[[str, int, Path], bool]


# ── shared driving ───────────────────────────────────────────────────


def run_leg(
    field: Field,
    name: str,
    results: Path,
    make_body: Body,
    timeout: float,
    drive: Driver | None = None,
    marker: str = "done",
    sample: bool = False,
) -> bool:
    """One leg of the field — felis included: launch, wait, verify the grid.

    A suite that measures from outside the window passes `drive`, and a
    `marker` naming what the wrapper touches once the window is ready to
    be driven; the driver then owns writing `<name>.done`, so a leg that
    was launched but not measured is retried rather than skipped on the
    next run.

    `sample` records what the terminal spent on the part of the body
    that `usage.wrap` put inside a sampled window, into
    `<name>.res.json` (`usage.py`). A leg whose sampling failed keeps its throughput
    number: the two are measured independently, and the missing record
    is reported rather than inferred.
    """
    if (results / f"{name}.done").exists():
        print(f"skipping {name} (already done)")
        return True
    if not field.has_leg(name):
        print(f"{name} is not in the field; skipping")
        return True
    with fieldmod.attempt(results, name) as tried:
        out = tried.dir
        with contextlib.ExitStack() as stack:
            try:
                leg = stack.enter_context(
                    fieldmod.open_leg(field, tried, make_body(name, out))
                )
            except fieldmod.DisplayUnavailable:
                return False
            sampler = None
            if sample:
                sampler = usage.Sampler(
                    out,
                    name,
                    lambda: leg_processes(leg),
                    lambda: leg.proc.poll() is None,
                    timeout,
                )
                sampler.start()
            ok = ready = fieldmod.wait_for(tried.path(marker), timeout, leg.proc)
            if ready and drive is not None:
                # The driver reports its own failure; it knows which one it was.
                ok = drive(name, leg.pid, out)
            if sampler is not None and (why := sampler.finish()):
                print(f"  UNSAMPLED: {name} resource usage — {why}", file=sys.stderr)
        if leg.refused:
            return False
        if not ready:
            print(f"  TIMEOUT: {name} never finished", file=sys.stderr)
        if not ok:
            return False
        if got := field.check(name, tried.path("size"), results):
            print(f"  grid: {got.cols}x{got.rows}")
        return True


def leg_processes(leg: fieldmod.Leg) -> dict[str, list[int]]:
    """The processes a leg's resources are charged to, by part.

    Resolved when the workload starts, not at launch: wezterm's CLI has
    spawned `wezterm-gui` by then and not a moment earlier. The alias,
    not the field name: a `ghostty-tip` leg still spawns processes called
    `ghostty`.
    """
    if leg.felis is not None:
        daemon = leg.felis.daemon_pid()
        if daemon is None:
            raise RuntimeError("the bench daemon was not found")
        return {"client": [leg.pid], "daemon": [daemon]}
    alias = fieldmod.LAUNCH_ALIAS.get(leg.name, leg.name)
    return {"terminal": fieldmod.child_pids(leg.pid, alias)}


def require_quiet_desktop() -> bool:
    """Is there a pin for this desktop that can hold the field?

    Selected rather than remembered, because a window manager that comes
    up mid-run resizes the windows under a suite that is already typing
    into them.
    """
    try:
        wm.select().prepare()
    except wm.PinRefused as err:
        print(err, file=sys.stderr)
        return False
    return True


def report_mismatches(field: Field) -> None:
    if not field.mismatches:
        return
    print(
        "\n!! the field was NOT uniform; these numbers are not comparable:",
        file=sys.stderr,
    )
    for line in field.mismatches:
        print(f"   {line}", file=sys.stderr)


# ── probe: felis's grid, the reference the field is pinned to ────────


def probe_grid(field: Field) -> fieldmod.Size | None:
    """felis's own window, measured once and cached as the reference.

    The reference is always felis's window: it is the one leg with no
    size flag, so every other terminal can be brought to it and it
    cannot be brought anywhere. Measured before any suite rather than
    inside whichever one runs first — that is what makes the suites
    comparable to each other. Startup takes each terminal's own size
    flag but skips the WM pin and grid read-back: its process exits
    before the harness can place or measure the window. The up-front
    probe is the only way startup, whose window is gone before anything
    can read a tty size, can be pinned at all. Discovery mode:
    the pin floats the window where the desktop tiles and resizes
    nothing, so what comes back is felis's natural geometry under this
    desktop's own decoration.
    """
    work = Path(tempfile.mkdtemp(prefix="felis-probe."))
    done = work / "felis.done"
    body = [f"touch {shlex.quote(str(done))}", "sleep 1"]
    try:
        with fieldmod.open_leg(
            field, fieldmod.Attempt(work, "felis", work), body, settle=0, discover=True
        ) as leg:
            if not fieldmod.wait_for(done, 60, leg.proc):
                print("felis never reported a grid; its log:", file=sys.stderr)
                if leg.felis is not None:
                    print(leg.felis.log.read_text(errors="replace"), file=sys.stderr)
                return None
            if leg.felis is not None and leg.felis.font_fallback():
                print("the probed grid is a fallback font's", file=sys.stderr)
                return None
            size = fieldmod.read_size(leg.size_path)
        return None if leg.refused else size
    except fieldmod.DisplayUnavailable:
        return None
    finally:
        shutil.rmtree(work, ignore_errors=True)


# ── vtebench ─────────────────────────────────────────────────────────


def suite_vtebench(field: Field, results: Path, params: dict) -> bool:
    """alacritty/vtebench: time to drain a payload, renderer live.

    vtebench differs from `kitten __benchmark__` twice over: its
    benchmark scripts read the grid from the controlling tty, so an
    unequal grid means unequal payloads rather than merely noisy
    numbers; and it does not suppress rendering (no DECSET 2026), so it
    measures PTY-read backpressure with the renderer live.
    """
    vtebench = params["VTEBENCH_BIN"]
    benchmarks = params["VTEBENCH_BENCHMARKS"]
    max_secs = params.get("MAX_SECS", "10")

    def body(name: str, out: Path) -> list[str]:
        dat = out / f"{name}.dat"
        return [
            # A short pass is discarded as focus warmup: the first pass
            # after the window maps is several times slow. stdout must
            # stay on the tty — the payload IS stdout — so results go
            # through --dat, never a redirect.
            f"{shlex.quote(vtebench)} -b {shlex.quote(benchmarks + '/light_cells')}"
            " --max-secs 2 --silent",
            *usage.wrap(
                out,
                name,
                [
                    f"{shlex.quote(vtebench)} -b {shlex.quote(benchmarks)}"
                    f" --max-secs {max_secs} --silent --dat {shlex.quote(str(dat))}"
                ],
            ),
            f"touch {shlex.quote(str(out / (name + '.done')))}",
            "sleep 1",
        ]

    ok = True
    for name in field.legs():
        ok = run_leg(field, name, results, body, 900, sample=True) and ok
    report_mismatches(field)
    return ok


# ── termbench-pro ────────────────────────────────────────────────────


def suite_termbench(field: Field, results: Path, params: dict) -> bool:
    """termbench-pro: throughput on fixed payloads.

    Complements vtebench — the payload does not scale with the grid, so
    the grid changes rendering load only, across five categories
    vtebench does not isolate.
    """
    tb = params["TB_BIN"]
    size_mb = params.get("SIZE_MB", "32")

    def body(name: str, out: Path) -> list[str]:
        result = out / f"{name}.json"
        return [
            f"{shlex.quote(tb)} --size 4",
            *usage.wrap(
                out,
                name,
                [
                    f"{shlex.quote(tb)} --size {size_mb} "
                    f"--output {shlex.quote(str(result))}"
                ],
            ),
            f"touch {shlex.quote(str(out / (name + '.done')))}",
            "sleep 1",
        ]

    ok = True
    for name in field.legs():
        ok = run_leg(field, name, results, body, 600, sample=True) and ok
    report_mismatches(field)
    return ok


# ── cat ──────────────────────────────────────────────────────────────


def suite_cat(field: Field, results: Path, params: dict) -> bool:
    """`time cat` over a 150 MB file — the comparison ghostty published.

    The crudest workload in the suite and the one users actually run:
    hand the terminal a large file and see how long it takes to come
    back. It differs from every other suite here in what it stops at —
    `cat` returns when the last byte has been *written to the pty*, not
    when the terminal has finished parsing it, so a terminal that reads
    eagerly into its own buffer can return before the work is done.
    That is the number ghostty published and the number a user feels;
    `kittenbench` is the one that waits for parsing to finish.

    hyperfine drives it so a leg is a mean over repetitions rather than
    one sample, and `--show-output` is what keeps the payload on the tty
    where the terminal has to draw it — without it hyperfine sends the
    output to /dev/null and measures `cat` alone.
    """
    hyperfine = params["HYPERFINE_BIN"]
    megabytes = int(params.get("CAT_MB", "150"))
    runs = params.get("CAT_RUNS", "3")
    kinds = params.get("CAT_KINDS", "ascii unicode csi").split()
    directory = Path(params["PAYLOAD_DIR"])

    files = {}
    for kind in kinds:
        print(f"payload: {kind} {megabytes} MB...", flush=True)
        files[kind] = payloads.build(kind, megabytes, directory)

    def body(name: str, out: Path) -> list[str]:
        argv = [
            hyperfine,
            "--warmup",
            "1",
            "--runs",
            runs,
            "--show-output",
            "--export-json",
            str(out / f"{name}.json"),
        ]
        for kind in kinds:
            argv += ["-n", kind, f"cat {shlex.quote(str(files[kind]))}"]
        return [
            *usage.wrap(out, name, [" ".join(shlex.quote(a) for a in argv)]),
            f"touch {shlex.quote(str(out / (name + '.done')))}",
            "sleep 1",
        ]

    timeout = 300 + 120 * len(kinds) * (1 + int(runs))
    ok = True
    for name in field.legs():
        ok = run_leg(field, name, results, body, timeout, sample=True) and ok
    report_mismatches(field)
    return ok


# ── kittenbench ──────────────────────────────────────────────────────


def suite_kittenbench(field: Field, results: Path, params: dict) -> bool:
    """kitty's `kitten __benchmark__`: throughput with parsing confirmed.

    The counterpart to `cat`. The benchmark suppresses rendering
    (DECSET 2026) and ends each pass with a DSR the terminal can only
    answer once it has parsed everything before it, so the number is
    parse throughput with no room to buffer the work away.

    Each benchmark is a separate invocation: `images` waits on a
    graphics reply and can hang, and one stall must not cost the others.
    The first pass after a window maps is several times slow, so one is
    run and discarded. Never wrap kitten in `timeout` — it reads
    /dev/tty for the timing query and hangs outside the foreground
    process group.
    """
    kitten = params["KITTEN_BIN"]
    reps = params.get("KITTEN_REPS", "100")
    warm = params.get("KITTEN_WARMUP_REPS", "20")
    benchmarks = params.get(
        "KITTEN_BENCHMARKS", "ascii unicode csi long_escape_codes"
    ).split()

    def body(name: str, out: Path) -> list[str]:
        result = shlex.quote(str(out / f"{name}.kitten"))
        run = f"{shlex.quote(kitten)} __benchmark__ --repetitions"
        return [
            f"{run} {warm} ascii > /dev/null 2>&1",
            *usage.wrap(
                out,
                name,
                [f"{run} {reps} {b} >> {result} 2>/dev/null" for b in benchmarks],
            ),
            f"touch {shlex.quote(str(out / (name + '.done')))}",
            "sleep 1",
        ]

    timeout = 120 + 120 * len(benchmarks)
    ok = True
    for name in field.legs():
        ok = run_leg(field, name, results, body, timeout, sample=True) and ok
    report_mismatches(field)
    return ok


# ── doom-fire ────────────────────────────────────────────────────────


def suite_doom_fire(field: Field, results: Path, params: dict) -> bool:
    """DOOM-fire-zig: frames per second on a full-screen animation.

    The one workload here produced by a real application rather than a
    benchmark harness: a full repaint of every cell, every frame, with
    the app computing the next frame between writes. That last part is
    why the numbers saturate — a terminal fast enough to keep up is
    waiting on the fire, not the other way round — and it is also why
    the frame rate is the number ghostty published beside its `cat`
    times.

    The frame rate is the app's own count over the run, which the
    `.#bench` build reports to stderr (see doom-fire-bench.patch). Its
    frames are sized from the tty, so this suite is only as comparable
    as the grid pin: a leg that missed the pin was drawing a different
    picture, not the same one slower.

    A short pass is run and discarded first — the first seconds after a
    window maps are slow, and the app's counter is a cumulative average
    that never shakes them off.
    """
    doom = params["DOOM_BIN"]
    secs = params.get("DOOM_FIRE_SECS", "20")
    warm = params.get("DOOM_FIRE_WARMUP_SECS", "5")

    def body(name: str, out: Path) -> list[str]:
        result = shlex.quote(str(out / f"{name}.doom-fire"))
        return [
            f"DOOM_BENCH_SECS={shlex.quote(warm)} {shlex.quote(doom)} 2>/dev/null",
            *usage.wrap(
                out,
                name,
                [
                    f"DOOM_BENCH_SECS={shlex.quote(secs)} {shlex.quote(doom)} 2> {result}"
                ],
            ),
            f"touch {shlex.quote(str(out / (name + '.done')))}",
            "sleep 1",
        ]

    timeout = 180 + 3 * (int(secs) + int(warm))
    ok = True
    for name in field.legs():
        ok = run_leg(field, name, results, body, timeout, sample=True) and ok
    report_mismatches(field)
    return ok


# ── startup ──────────────────────────────────────────────────────────

# Long enough for the slowest cold start in the field (a first-ever app
# bundle launch on macOS is several seconds) and short enough that a
# terminal that never maps a window costs the run seconds, not minutes.
STARTUP_TIMEOUT = 20.0


def suite_startup(field: Field, results: Path, params: dict) -> bool:
    """Time from launching a terminal to its first window on screen.

    Each launch runs a child that outlives the measurement (`sleep`), so
    the window has a reason to exist; the time is taken when the
    platform first reports the launch's window (`firstwindow.py`: mapped
    with a committed buffer under niri, ordered on screen on macOS), and
    the terminal is then terminated. Timing a launch that runs `true`
    until it exits, which this replaces, measured something else for
    part of the field: foot on Linux exits without ever mapping a
    window, wezterm on macOS creates one it never orders on screen, and
    felis maps one it never draws into, so their numbers were process
    lifetimes rather than windows.

    felis gets two rows because of the daemon/client split: cold kills
    the bench daemon before every launch (autospawn included in the
    number), warm reuses a primed one — the steady-state cost a user
    pays after the first window. The warm daemon is the one an untimed
    first launch autospawned, spared when each launch is stopped: on
    macOS it is a forked child of the client and so inside the tree the
    stop kills. Each row checks its daemon before every launch (none for
    cold, the primed pid for warm), and a launch that finds anything
    else fails the row, since its number would belong to the other one.

    No WM pin and no grid read-back: the pin's handshake would sit
    inside the launch being timed. Each terminal still receives its own
    size flag for the reference grid. On a tiling desktop the first
    configure is the tile's, so work that delays it is inside the number.

    A launch that maps no window within `STARTUP_TIMEOUT` is recorded as
    that failure, and the leg has no number.
    """
    runs = int(params.get("RUNS", "10"))
    warmup = int(params.get("WARMUP", "3"))
    watcher = firstwindow.select()
    if watcher is None:
        print(
            "!! startup: no way to see a window map on this desktop (niri or "
            "macOS); skipped",
            file=sys.stderr,
        )
        return False
    wrapper = fieldmod.wrapper_script(["exec sleep 60"], settle=0)

    def record(
        out: Path, label: str, samples: list[float], failure: str | None, **extra
    ) -> bool:
        payload = {
            "metric": "first-window",
            "watcher": watcher.name,
            "runs": runs,
            "warmup": warmup,
            "samples_ms": [round(s, 3) for s in samples],
            **extra,
        }
        if failure:
            payload["failed"] = failure
            print(f"  FAILED: {label} — {failure}", file=sys.stderr)
        else:
            print(f"  {label}: median {statistics.median(samples):.1f} ms")
        (out / f"{label}.startup.json").write_text(json.dumps(payload, indent=2) + "\n")
        return failure is None

    def felis_entry(out: Path) -> bool:
        home, sockdir = fieldmod.felis_home(field.pres)
        sock = sockdir / "daemon.sock"
        env = {
            **os.environ,
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
        }
        argv = [field.felis_bin, "--socket", str(sock), "--", str(wrapper)]

        def launch() -> subprocess.Popen:
            return subprocess.Popen(
                argv, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )

        def kill_daemon() -> None:
            if daemon := fieldmod.hold_felis_daemon(sock):
                fieldmod.stop([daemon], escalate=False)

        checked = {"felis-cold": 0, "felis-warm": 0}

        def no_daemon() -> str | None:
            kill_daemon()
            if pid := fieldmod.felis_daemon_pid(sock):
                return f"the bench daemon (pid {pid}) survived SIGTERM"
            return None

        def cold_check() -> str | None:
            if problem := no_daemon():
                return problem
            checked["felis-cold"] += 1
            return None

        def prime() -> tuple[int | None, str | None]:
            if problem := no_daemon():
                return None, f"before the priming launch, {problem}"
            watcher.settle()
            proc = launch()
            daemon = None
            try:
                if watcher.wait(proc.pid, STARTUP_TIMEOUT) is not None:
                    daemon = fieldmod.felis_daemon_pid(sock)
            finally:
                stop_launch(proc, spare=[daemon] if daemon else [])
            if daemon is None:
                return None, "the priming launch left no daemon on the bench socket"
            return daemon, None

        try:
            print("--- felis (cold) ---", flush=True)
            cold = measure_launches(
                watcher, launch, stop_launch, runs, warmup, cold_check
            )
            print("--- felis (warm) ---", flush=True)
            daemon, problem = prime()

            def warm_check() -> str | None:
                # A warm launch opens a fresh session and the one before it
                # outlives its window, so without this each warm run would
                # find one more session in the daemon than the last.
                felis_kill_sessions(field.felis_bin, sock, env)
                if fieldmod.felis_daemon_pid(sock) != daemon:
                    return f"the warm daemon (pid {daemon}) was gone"
                checked["felis-warm"] += 1
                return None

            if problem:
                warm = ([], problem)
            else:
                warm = measure_launches(
                    watcher,
                    launch,
                    lambda proc: stop_launch(proc, spare=[daemon]),
                    runs,
                    warmup,
                    warm_check,
                )
            # Not short-circuited: a failed cold row still records the warm
            # one, and the entry succeeds only if both do.
            cold_ok = record(
                out, "felis-cold", *cold, daemon_checked=checked["felis-cold"]
            )
            warm_ok = record(
                out, "felis-warm", *warm, daemon_checked=checked["felis-warm"]
            )
            return cold_ok and warm_ok
        finally:
            kill_daemon()
            shutil.rmtree(sockdir, ignore_errors=True)
            shutil.rmtree(home, ignore_errors=True)

    def terminal_entry(name: str, out: Path) -> bool:
        spec = field.launch(name)
        if spec is None:
            return False
        print(f"--- {name} ---", flush=True)
        try:
            return record(
                out,
                name,
                *measure_launches(
                    watcher,
                    lambda: fieldmod.launch_terminal(spec, wrapper),
                    stop_launch,
                    runs,
                    warmup,
                ),
            )
        finally:
            for path in spec.tmp:
                path.unlink(missing_ok=True)

    try:
        for name in field.legs():
            if (results / f"{name}.done").exists():
                print(f"skipping {name} (already done)")
                continue
            with fieldmod.attempt(results, name) as tried:
                out = tried.dir
                before = envinfo.leg_env()
                if reason := envinfo.cannot_present(before):
                    fieldmod.refuse(
                        out, name, {"before": before}, f"{reason} before the leg"
                    )
                    continue
                measured = (
                    felis_entry(out) if name == "felis" else terminal_entry(name, out)
                )
                after = envinfo.leg_env()
                if reason := envinfo.cannot_present(after):
                    reason = f"{reason} when the leg ended"
                    fieldmod.refuse(
                        out, name, {"before": before, "after": after}, reason
                    )
                    continue
                tried.path("env.json").write_text(
                    json.dumps({"before": before, "after": after}, indent=2) + "\n"
                )
                # A launch that mapped no window is a result too, recorded
                # in the leg's JSON; only a leg that could not be launched at
                # all is left to be retried by the next run.
                if measured or any(out.glob("*.startup.json")):
                    tried.path("done").touch()
    finally:
        watcher.close()
        wrapper.unlink(missing_ok=True)
    return True


def measure_launches(
    watcher,
    launch: Callable[[], subprocess.Popen],
    stop: Callable[[subprocess.Popen], None],
    runs: int,
    warmup: int,
    before_each: Callable[[], str | None] | None = None,
) -> tuple[list[float], str | None]:
    """`warmup` discarded launches, then `runs` timed ones, in milliseconds.

    Stops at the first launch that maps no window: the ones before it
    would be a number for a terminal that does not reliably open one.
    `before_each` stops it the same way by naming what is wrong.
    """
    samples: list[float] = []
    total = warmup + runs
    for index in range(total):
        if before_each is not None and (problem := before_each()):
            return samples, f"before launch {index + 1} of {total}, {problem}"
        watcher.settle()
        started = time.monotonic()
        proc = launch()
        try:
            mapped = watcher.wait(proc.pid, STARTUP_TIMEOUT)
        finally:
            stop(proc)
        if mapped is None:
            return samples, (
                f"launch {index + 1} of {total} put no window on screen within "
                f"{STARTUP_TIMEOUT:g} s"
            )
        if index >= warmup:
            samples.append((mapped - started) * 1000)
    return samples, None


def stop_launch(proc: subprocess.Popen, spare: Collection[int] = ()) -> None:
    """Terminate a launch and everything under it but `spare`, and wait for it to go.

    The whole tree, because the window is not always the launched
    process's own (`wezterm start` spawns `wezterm-gui`); waited for, so
    the next launch does not start beside a window still closing.
    """
    if lingering := fieldmod.stop([fieldmod.Held.child(proc)], spare):
        print(f"  LINGERING: {lingering} survived SIGKILL", file=sys.stderr)
    time.sleep(0.3)


def felis_kill_sessions(felis_bin: str, sock: Path, env: dict) -> None:
    listed = subprocess.run(
        [felis_bin, "--socket", str(sock), "sessions", "list", "--format", "json"],
        env=env,
        capture_output=True,
        text=True,
    )
    for session in session_ids(listed.stdout):
        subprocess.run(
            [felis_bin, "--socket", str(sock), "sessions", "kill", session],
            env=env,
            capture_output=True,
        )


def session_ids(roster: str) -> list[str]:
    """The ids in `felis sessions list --format json`; none when it did not parse."""
    try:
        parsed = json.loads(roster)
    except json.JSONDecodeError:
        return []
    return [s["id"] for s in parsed.get("sessions") or [] if s.get("id")]


# ── memory ───────────────────────────────────────────────────────────


def memory_record(
    idle: fieldmod.MemorySample,
    flooded: fieldmod.MemorySample,
    idle_cpu: int,
    flooded_cpu: int,
) -> dict:
    """One leg's raw numbers, metric included.

    Both CPU marks, not their difference: a results directory holds what
    was measured and `loaders.py` derives what is drawn, so a reader who
    disagrees with the subtraction can still see the two readings.
    """
    return {
        "metric": idle.metric,
        "idle_kb": idle.kb,
        "flooded_kb": flooded.kb,
        "peak_kb": flooded.peak_kb,
        "idle_cpu_cs": idle_cpu,
        "flooded_cpu_cs": flooded_cpu,
    }


def suite_memory(field: Field, results: Path, params: dict) -> bool:
    """What a terminal holds idle and after a flood, plus the CPU the flood cost.

    The suite most sensitive to the pinned condition: the grid is rows x
    cols x cell and the glyph atlas is sized from the font, so an
    unequal field does not add noise here, it changes the quantity being
    reported.

    Each number `field.memory_sample` returns is what the OS charges to
    the processes the suite samples under its ledger: PSS (the
    process's proportional share of its resident pages) on Linux,
    `phys_footprint` (the process's physical-footprint ledger, the
    number Activity Monitor's Memory column shows) on macOS. Whatever
    the GPU driver allocates and the OS charges to the process is
    inside; whatever it holds elsewhere (device memory the process does
    not map, kernel-side driver state) is outside — and how the split
    falls depends on the API, the driver and the GPU, so a GPU
    terminal's bar carries a driver share the reader cannot separate
    from the rest, while a CPU renderer (foot) carries none.

    The CPU number is the difference between the two marks rather than
    the total at the second one: startup, font loading and the idle
    wait are work the flood did not do, and they differ across the
    field by more than the flood itself does on the fast terminals.
    """
    flood = params.get("FLOOD_LINES", "200000")

    def body(name: str, out: Path) -> list[str]:
        return [
            f"touch {shlex.quote(str(out / (name + '.idle')))}",
            "sleep 6",
            f"seq 1 {flood}",
            "sleep 2",
            f"touch {shlex.quote(str(out / (name + '.flooded')))}",
            "sleep 60",
        ]

    def flooded_mark(leg: fieldmod.Leg, out: Path) -> bool:
        if not fieldmod.wait_for(out / f"{leg.name}.flooded", 300, leg.proc):
            print(f"  TIMEOUT: {leg.name} never finished the flood", file=sys.stderr)
            return False
        time.sleep(2)
        return True

    def sample_terminal(leg: fieldmod.Leg, out: Path) -> dict | None:
        # Resolved after the idle mark, not before: wezterm's CLI has
        # spawned wezterm-gui by then and not a moment earlier. The
        # alias, not the field name: a `ghostty-tip` leg still spawns
        # processes called `ghostty`, and matching on the label would
        # miss every one of them.
        pids = fieldmod.child_pids(
            leg.pid, fieldmod.LAUNCH_ALIAS.get(leg.name, leg.name)
        )
        idle = fieldmod.memory_sample(pids)
        idle_cpu = fieldmod.cpu_centiseconds(pids)
        if not flooded_mark(leg, out):
            return None
        flooded = fieldmod.memory_sample(pids)
        cpu = fieldmod.cpu_centiseconds(pids)
        if idle is None or flooded is None:
            print(f"  UNSAMPLED: {leg.name} left no readable memory", file=sys.stderr)
            return None
        if idle_cpu is None or cpu is None:
            print(
                f"  UNSAMPLED: {leg.name} left no readable CPU accounting",
                file=sys.stderr,
            )
            return None
        return memory_record(idle, flooded, idle_cpu, cpu)

    def sample_felis(leg: fieldmod.Leg, out: Path) -> dict | None:
        """felis, sampled as client and daemon separately.

        The split is the cost no other terminal has, and the detached
        daemon is the cost no other terminal can even express.
        """
        run = leg.felis
        daemon = run.daemon_pid()
        if daemon is None:
            print("  bench daemon not found", file=sys.stderr)
            return None
        idle_c = fieldmod.memory_sample([leg.pid])
        idle_d = fieldmod.memory_sample([daemon])
        idle_cpu_c = fieldmod.cpu_centiseconds([leg.pid])
        idle_cpu_d = fieldmod.cpu_centiseconds([daemon])
        if not flooded_mark(leg, out):
            return None
        flood_c = fieldmod.memory_sample([leg.pid])
        flood_d = fieldmod.memory_sample([daemon])
        cpu_c = fieldmod.cpu_centiseconds([leg.pid])
        cpu_d = fieldmod.cpu_centiseconds([daemon])
        idle = fieldmod.total([idle_c, idle_d])
        flooded = fieldmod.total([flood_c, flood_d])
        if idle is None or flooded is None:
            print("  UNSAMPLED: felis left no readable memory", file=sys.stderr)
            return None
        if None in (idle_cpu_c, idle_cpu_d, cpu_c, cpu_d):
            print("  UNSAMPLED: felis left no readable CPU accounting", file=sys.stderr)
            return None
        record = memory_record(idle, flooded, idle_cpu_c + idle_cpu_d, cpu_c + cpu_d)
        record["parts"] = {
            "client": memory_record(idle_c, flood_c, idle_cpu_c, cpu_c),
            "daemon": memory_record(idle_d, flood_d, idle_cpu_d, cpu_d),
        }
        # Detached residency: close the window, keep the session.
        run.stop_client()
        time.sleep(2)
        detached = fieldmod.memory_sample([daemon])
        before = fieldmod.cpu_centiseconds([daemon])
        time.sleep(10)
        after = fieldmod.cpu_centiseconds([daemon])
        if detached is not None and before is not None and after is not None:
            record["detached"] = {
                "kb": detached.kb,
                "cpu_cs": after - before,
                "seconds": 10,
            }
        else:
            print(
                "  UNSAMPLED: felis daemon left no readable detached accounting",
                file=sys.stderr,
            )
        return record

    ok = True
    for name in field.legs():
        if (results / f"{name}.mem.json").exists():
            print(f"skipping {name} (already done)")
            continue
        record = None
        with fieldmod.attempt(results, name, "mem.json") as tried:
            out = tried.dir
            with contextlib.ExitStack() as stack:
                try:
                    leg = stack.enter_context(
                        fieldmod.open_leg(field, tried, body(name, out), settle=2)
                    )
                except fieldmod.DisplayUnavailable:
                    ok = False
                    continue
                if fieldmod.wait_for(tried.path("idle"), 120, leg.proc):
                    time.sleep(1)
                    sample = sample_felis if leg.felis else sample_terminal
                    record = sample(leg, out)
                else:
                    print(f"  TIMEOUT: {name} never settled", file=sys.stderr)
            if leg.refused:
                ok = False
                continue
            if record is None:
                # Every path that returns None has already said which
                # reading went missing; silence here would read as a
                # measured leg that simply had no bar.
                ok = False
                continue
            tried.path("mem.json").write_text(json.dumps(record, indent=2) + "\n")
            if got := field.check(name, tried.path("size"), results):
                print(f"  grid: {got.cols}x{got.rows}")
    report_mismatches(field)
    return ok


# ── latency ──────────────────────────────────────────────────────────


def suite_latency(field: Field, results: Path, params: dict) -> bool:
    """The delay between pressing a key and seeing the glyph.

    The one suite measured from outside the terminal rather than from
    inside it: the instrument presses a key through the platform's own
    injection API and then polls the screen until the pixel the glyph
    lands on changes color, so the number carries the compositor and the
    display the way a reader's eye does — and, being taken off the
    screen, it needs no cooperation from the terminal under test, which
    is what lets one tool cover the whole field.

    Two instruments, one method. macOS borrows Typometer, whose figures
    every published terminal-latency number of the last decade was taken
    with; it injects and reads the screen through X11, so the Wayland
    field is measured instead by `wl-latency --measure`, which follows
    upstream's loop step for step over `zwp_virtual_keyboard_v1` and
    `wlr-screencopy` (docs/explanation/benchmarks.md).

    Four conditions no other suite needs — the last one below, and:

    - **The window has to have the keyboard**, since keystrokes go
      wherever focus is. On macOS every leg is raised
      (`field.raise_window`, which two of the terminals cannot do for
      themselves) and then *verified*; under niri the window is focused
      by id and the instrument re-checks that id before every keystroke.
      A leg that will not take focus is skipped: the alternative is
      typing two hundred characters into whatever the developer left
      open.
    - **`cat`, never a shell.** The echo then comes from the kernel's
      line discipline — identical for every terminal, and with no
      prompt, autosuggestion or syntax highlighting repainting the
      pixels the instrument is watching.
    - **A steady bar cursor**, set with DECSCUSR from inside the window.
      Both instruments abort on a block cursor ("previously undetected
      block cursor found"): the block already covers the pixel they wait
      on. The escape is the one lever the whole field shares — felis has
      no cursor-shape config key at all, because shape is the running
      program's to choose (client-core config.rs) — so a per-terminal
      launch flag could not have pinned this.

    The fourth is the font size, and the comment on `pinned` below says
    why this is the one suite that does not inherit the field's.
    """
    count = params.get("LATENCY_COUNT", "200")
    delay = params.get("LATENCY_DELAY", "150")
    font_pt = float(params.get("LATENCY_FONT_PT", "14"))
    # Each sample costs its own delay plus the round trip, and detection
    # types and deletes a reference pattern before the first one. The
    # round trip is the quantity under test, so its allowance covers a
    # terminal that averages close to a second rather than a typical one.
    budget = 60 + int(count) * (int(delay) + 1000) / 1000

    def body(name: str, out: Path) -> list[str]:
        ready = shlex.quote(str(out / f"{name}.ready"))
        return [
            # Clear, home, DECSCUSR 6 (steady bar): a blank screen is
            # what the pattern detection needs, and the bar is what lets
            # it run at all.
            r"printf '\033[2J\033[H\033[6 q'",
            f"touch {ready}",
            "exec cat > /dev/null",
        ]

    def instrument(name: str, pid: int, out: Path) -> list[str] | None:
        """The argv that measures this leg, or None if it cannot be typed into.

        Both branches end at the same command shape and the same JSON
        file; what differs is how the keyboard is pointed at the window,
        which is the one part of the method the platform owns.
        """
        result = out / f"{name}.latency.json"
        if DARWIN:
            # Raised again rather than trusted: the window came forward
            # when it launched, but the seconds it spent settling are
            # seconds something else could have taken the front.
            front = fieldmod.frontmost_pid()
            if front is None or front not in fieldmod.process_tree(pid):
                if not fieldmod.raise_window(pid):
                    if fieldmod.screen_locked():
                        print(
                            "  SKIPPED: the screen is locked, so no window can "
                            "be in front of it. Unlock the Mac and re-run.",
                            file=sys.stderr,
                        )
                    else:
                        print(
                            f"  SKIPPED: {name}'s window would not come to the "
                            "front, so a keystroke would land somewhere else. "
                            "Check that System Events may be controlled by the "
                            "terminal this run was started from, leave the "
                            "desktop alone, and re-run.",
                            file=sys.stderr,
                        )
                    return None
            return [
                params["TYPOMETER_BIN"],
                *("--count", count, "--delay", delay, "--json", str(result)),
            ]
        # The window id, not the pid: a virtual keyboard types into
        # whatever the compositor has focused, and the instrument refuses
        # to inject unless that is the id it was given — which it
        # re-checks before every keystroke, so a window that loses focus
        # mid-leg fails the leg instead of typing into the desktop.
        window_id, detail = wm.select().focus_for_typing(pid)
        if window_id is None:
            print(
                f"  SKIPPED: {name} — {detail}, so a keystroke would land "
                "somewhere else.",
                file=sys.stderr,
            )
            return None
        return [
            params["WL_LATENCY_BIN"],
            *("--measure", "--window-id", str(window_id)),
            *("--count", count, "--delay", delay, "--json", str(result)),
        ]

    def drive(name: str, pid: int, out: Path) -> bool:
        # Re-checked per leg, not only at the start of the run: a window
        # manager that comes up mid-run — a login, a launchd restart —
        # resizes the window under the keystrokes, which moves the pixel
        # the instrument is watching and fails the leg as a "block
        # cursor" rather than as what it is.
        if not require_quiet_desktop():
            return False
        argv = instrument(name, pid, out)
        if argv is None:
            return False
        try:
            done = subprocess.run(
                argv, timeout=budget, stderr=subprocess.PIPE, text=True
            )
        except subprocess.TimeoutExpired:
            print(f"  TIMEOUT: {name} did not finish typing", file=sys.stderr)
            record_latency_failure(results, name, "did not finish typing")
            return False
        # Re-printed rather than inherited: the last line is the reason a
        # leg failed, and it has to reach the results as well as the console.
        print(done.stderr, end="", file=sys.stderr)
        if done.returncode != 0:
            reason = last_line(done.stderr) or f"exited {done.returncode}"
            record_latency_failure(results, name, reason)
            print(
                f"  FAILED: {name} — the instrument exited {done.returncode}. "
                "If it could not detect the reference pattern, either the "
                "desktop is denying it the screen — on macOS grant Screen "
                "Recording and Accessibility to the terminal application this "
                "run was started from (System Settings > Privacy & Security) — "
                "or the cell is too small to recognize, which --param "
                "LATENCY_FONT_PT=<bigger> answers.",
                file=sys.stderr,
            )
            return False
        (out / f"{name}.done").touch()
        return True

    # Typometer finds the characters it typed by diffing two screen
    # captures, and its detector smooths with a 2 px radius before
    # looking for five separate glyph areas. Below roughly 12 points per
    # cell they merge into one blob and every leg aborts with "Cannot
    # detect the reference pattern" — measured on this field: 9 and 10
    # fail, 12 and 16 work. So the run-wide font pin cannot be used
    # here, and replacing it costs the measurement nothing: how far a
    # keystroke has to travel to the screen does not depend on how wide
    # the cell it lands in is. felis's grid is re-probed at the new size
    # so the field still shares one window area, which does enter the
    # per-frame paint cost. An explicit GRID_ROWS/GRID_COLS is kept
    # instead: re-probing would hand back felis's tile under a tiling
    # desktop, a different grid from every other suite in the run.
    if params.get("GRID_SOURCE") == "explicit":
        pinned = dataclasses.replace(
            field, pres=dataclasses.replace(field.pres, pt=font_pt)
        )
    else:
        pinned = dataclasses.replace(
            field,
            pres=dataclasses.replace(field.pres, pt=font_pt, rows=None, cols=None),
        )
        # Cached beside the artifacts like the orchestrator caches its own:
        # a resumed run must not re-probe into a different answer after the
        # desktop has changed underneath it.
        cache = results / "grid"
        if grid := fieldmod.read_size(cache) or probe_grid(pinned):
            pinned.pres = dataclasses.replace(
                pinned.pres,
                rows=grid.rows,
                cols=grid.cols,
                xpixel=grid.xpixel,
                ypixel=grid.ypixel,
            )
            cache.write_text(grid.text())
        else:
            print(
                "  the grid could not be probed at this suite's font size; every "
                "terminal will open at its own default and the bars carry "
                "different window areas",
                file=sys.stderr,
            )
    print(f"  {pinned.pres.summary()}")

    # Held here as well as around the whole run (crossterm.py), so that
    # driving this one suite by hand is not the case that measures a
    # parked display — and this is the suite that cannot survive one at
    # all: no window comes to the front of a lock screen.
    awake = fieldmod.keep_display_awake()
    # The window has to open and settle before the marker lands; the
    # measurement itself runs under `budget`, not under this.
    ready_timeout = 120
    try:
        ok = True
        for name in pinned.legs():
            ok = (
                with_retry(
                    results,
                    name,
                    lambda n=name: run_leg(
                        pinned, n, results, body, ready_timeout, drive, "ready"
                    ),
                )
                and ok
            )
    finally:
        awake.terminate()
    report_mismatches(field)
    return ok


# A leg the instrument failed gets one more window. A transient — a frame
# the compositor sampled mid-animation, a key the seat dropped — does not
# survive a relaunch, and a terminal that genuinely cannot be measured fails
# twice. Both attempts' reasons stay in `<name>.latency-failed`, which the
# chart quotes, so a retried bar is never mistaken for a clean one.
LATENCY_ATTEMPTS = 2


def with_retry(results: Path, name: str, attempt: Callable[[], bool]) -> bool:
    """Run one latency leg, and once more if the instrument failed it.

    Only a failure the instrument itself recorded earns another window:
    a leg that was skipped (no focus, no quiet desktop) would be skipped
    again, for the same reason.
    """
    for tries in range(1, LATENCY_ATTEMPTS + 1):
        before = latency_failures(results, name)
        if attempt():
            return True
        if latency_failures(results, name) == before:
            return False
        if tries < LATENCY_ATTEMPTS:
            print(f"  retrying {name} in a fresh window", file=sys.stderr)
    return False


def last_line(text: str) -> str:
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    return lines[-1] if lines else ""


def latency_failures(results: Path, name: str) -> list[str]:
    path = results / f"{name}.latency-failed"
    return path.read_text().splitlines() if path.exists() else []


def record_latency_failure(results: Path, name: str, reason: str) -> None:
    with (results / f"{name}.latency-failed").open("a") as sink:
        sink.write(" ".join(reason.split()) + "\n")


SUITES = {
    "vtebench": suite_vtebench,
    "termbench": suite_termbench,
    "cat": suite_cat,
    "kittenbench": suite_kittenbench,
    "doom-fire": suite_doom_fire,
    "startup": suite_startup,
    "memory": suite_memory,
    "latency": suite_latency,
}


# ── standalone entry point ───────────────────────────────────────────


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("suite", choices=sorted(SUITES))
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--felis-bin", default="./target/release/felis")
    parser.add_argument("--font-family", default=fieldmod.DEFAULT_FONT_FAMILY)
    parser.add_argument("--font-pt", type=float, default=fieldmod.DEFAULT_FONT_PT)
    parser.add_argument(
        "--scale",
        type=float,
        default=1.0,
        help="the display's backing scale, recorded with the results; it "
        "does not change the pinned font size (crossterm.py reads it from "
        "envinfo, and a standalone run has nobody to ask)",
    )
    parser.add_argument(
        "--terminals",
        nargs="+",
        metavar="NAME",
        help="limit the comparison field to these terminals; felis always "
        "runs its own leg, so `--terminals felis` iterates on the dev "
        "build alone",
    )
    parser.add_argument(
        "--param",
        action="append",
        metavar="KEY=VALUE",
        help="override a workload knob (the keys of crossterm.DEFAULT_PARAMS, "
        "e.g. KITTEN_REPS=80)",
    )
    args = parser.parse_args(argv)

    pin = wm.select()
    try:
        pin_scale = pin.prepare()
    except wm.PinRefused as err:
        print(err, file=sys.stderr)
        return 1
    import crossterm  # local import: only the standalone path needs it

    tools = crossterm.resolve_field(False, {})
    pres = fieldmod.Presentation(
        family=args.font_family, pt=args.font_pt, scale=pin_scale or args.scale
    )
    binaries = {
        n: t.path for n, t in tools.items() if t.found and n in crossterm.TERMINALS
    }
    if args.terminals:
        binaries = {n: p for n, p in binaries.items() if n in args.terminals}
    fld = Field(binaries, args.felis_bin, pres, pin=pin)
    if grid := probe_grid(fld):
        fld.pres = dataclasses.replace(
            pres,
            rows=grid.rows,
            cols=grid.cols,
            xpixel=grid.xpixel,
            ypixel=grid.ypixel,
        )
    print(fld.pres.summary())
    args.results.mkdir(parents=True, exist_ok=True)
    fieldmod.clear_attempts(args.results)
    repo = Path(__file__).resolve().parent.parent.parent
    defaults = dict(crossterm.DEFAULT_PARAMS)
    for override in args.param or []:
        key, _, value = override.partition("=")
        if not value:
            print(f"--param wants KEY=VALUE, got {override!r}", file=sys.stderr)
            return 2
        defaults[key] = value
    params = crossterm.harness_params(tools, defaults, repo)
    return 0 if SUITES[args.suite](fld, args.results, params) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

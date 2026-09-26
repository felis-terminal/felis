#!/usr/bin/env python3
"""felis cross-terminal benchmark orchestrator.

Runs the same workloads through felis and the other terminals under one
set of conditions, then renders the results as charts. The measuring is
`suites.py`, and the condition every terminal is held to — pristine
config, one font family, the point size converted to felis's physical
pixels, one grid — is `field.py`. What this adds:

- **One field, resolved once.** Every tool and terminal comes from
  `nix develop .#bench` (the `FELIS_BENCH_*` paths that shell exports;
  see devshell.nix beside this file), falling back to PATH only when a
  run happens outside it. Nothing is looked up inside a suite.
- **One reference grid.** felis has no size flag, so its grid is probed
  once here and the rest of the field is pinned to it — before any
  suite, so they are comparable to each other and not only within
  themselves.
- **Provenance.** `envinfo.py` records the machine, GPU, display, power
  state and every tool's version and origin into meta.json, so a result
  can be compared with one from another day or another machine.
- **One results root** per run, and the chart pass (`report.py`).

Commands
  run     drive the suites, then write the report
  report  render charts from an existing results root

Results root layout, written by `run` and read by `report`:

    <root>/meta.json          machine, tools, the pinned condition
    <root>/grid               felis's probed grid, "ROWS COLS"
    <root>/<suite>/           that suite's raw artifacts, plus run.log
    <root>/<suite>/order      the order the legs ran in
    <root>/<suite>/grid-mismatch  legs that missed the pin, if any
    <root>/<suite>/refused/   a refused leg's artifacts, out of the report's reach
    <root>/<suite>/unfinished/  those of a leg that ended without its resume marker
    <root>/<suite>/.attempts/ legs in progress (`field.attempt`), never read
    <root>/report.md          the written report
    <root>/png/<chart>.png    one faceted chart per suite

With `ROUNDS` above 1 each suite's artifacts move one level down, into
`<root>/<suite>/round-<n>/`, and a leg's resume marker is per round.
`loaders.rounds` is the one place that resolves either layout.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import shutil
import sys
import time
from dataclasses import dataclass, replace
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import envinfo  # noqa: E402
import field  # noqa: E402
import loaders  # noqa: E402
import report  # noqa: E402
import suites  # noqa: E402
import wm  # noqa: E402
from loaders import HARNESSES, collect  # noqa: E402

# The terminals felis is compared against. ghostty is macOS/Linux, foot
# is Wayland-only, and `ghostty-tip` is pinned on Linux but arrives
# through `--tool` on macOS.
# A missing one is left out of the field, except that a full run refuses
# to start without both ghostty builds (`missing_ghostty`).
TERMINALS = list(field.FIELD_ORDER)

# Workload knobs the harnesses read from the environment. They live here
# rather than as script defaults so that every run records the values it
# used — a payload size or scrollback depth that drifted between two runs
# is otherwise invisible in the results.
#
# The font entries come from `field.py`, which owns the reasoning for
# them; they are surfaced as parameters so `--param FONT_PT=14` works
# like any other knob.
DEFAULT_PARAMS = {
    "FONT_FAMILY": field.DEFAULT_FONT_FAMILY,
    "FONT_PT": f"{field.DEFAULT_FONT_PT:g}",
    "MAX_SECS": "10",  # vtebench: seconds per benchmark
    "SIZE_MB": "32",  # termbench: payload size
    "CAT_MB": "150",  # cat: payload size, the size ghostty published
    "CAT_KINDS": "ascii unicode csi",  # cat: which payloads (payloads.py)
    "CAT_RUNS": "3",  # cat: hyperfine runs per payload
    "KITTEN_REPS": "100",  # kittenbench: repetitions per benchmark
    "KITTEN_WARMUP_REPS": "20",  # kittenbench: discarded first pass
    "KITTEN_BENCHMARKS": "ascii unicode csi long_escape_codes",
    "DOOM_FIRE_SECS": "20",  # doom-fire: measured run length
    "DOOM_FIRE_WARMUP_SECS": "5",  # doom-fire: discarded first run
    "RUNS": "10",  # startup: timed launches per terminal
    "WARMUP": "3",  # startup: discarded launches before them
    "LATENCY_COUNT": "200",  # latency: characters typed per terminal
    "LATENCY_DELAY": "150",  # latency: ms between keystrokes
    # latency: its own font size, because Typometer cannot find its own
    # pattern in a 9 pt cell (suites.py:suite_latency)
    "LATENCY_FONT_PT": "14",
    # How many times the whole field is measured. One is a quick look;
    # the between-round range a chart can draw a whisker from needs at
    # least two, and a publishable run uses three.
    "ROUNDS": "1",
    "FLOOD_LINES": "200000",  # memory
    "SCROLLBACK": "10000",  # memory: felis's fixed DEFAULT_SCROLLBACK_ROWS
}


# ── tool resolution ──────────────────────────────────────────────────


@dataclass
class Tool:
    name: str
    path: str | None
    pinned: bool  # came from the .#bench shell rather than PATH

    @property
    def found(self) -> bool:
        return self.path is not None


# A tool whose binary is not named after its entry here. DOOM-fire keeps
# upstream's spelling, and the entry stays lowercase like every other so
# `--tool doom-fire=…` and `FELIS_BENCH_DOOM_FIRE` follow the same rule.
PATH_ALIAS = {"doom-fire": "DOOM-fire"}


def resolve(name: str, use_path: bool = False, override: str | None = None) -> Tool:
    """A tool's explicit override, else its pinned path, else PATH."""
    if override:
        # Never silently fall through: an override that points nowhere is a
        # typo, and measuring the pinned build instead would look like it
        # worked.
        if not Path(override).exists():
            raise FileNotFoundError(f"--tool {name}: no such file: {override}")
        return Tool(name, override, pinned=False)
    env = os.environ.get(f"FELIS_BENCH_{name.upper().replace('-', '_')}")
    if env and not use_path and Path(env).exists():
        return Tool(name, env, pinned=True)
    return Tool(name, shutil.which(PATH_ALIAS.get(name, name)), pinned=False)


def resolve_field(use_path: bool, overrides: dict[str, str]) -> dict[str, Tool]:
    names = [
        "vtebench",
        "tb",
        "hyperfine",
        "kitten",
        "doom-fire",
        "typometer",
        "wl-latency",
        *TERMINALS,
    ]
    if unknown := set(overrides) - set(names):
        raise KeyError(f"--tool: unknown tool(s): {', '.join(sorted(unknown))}")
    return {name: resolve(name, use_path, overrides.get(name)) for name in names}


def parse_overrides(values: list[str] | None) -> dict[str, str]:
    out = {}
    for value in values or []:
        name, _, path = value.partition("=")
        if not path:
            raise ValueError(f"--tool wants NAME=PATH, got {value!r}")
        out[name] = path
    return out


def harness_params(
    tools: dict[str, Tool], params: dict[str, str], repo: Path
) -> dict[str, str]:
    """Every path a suite needs, so none of them searches for one."""
    out = dict(params)
    for key, name in (
        ("VTEBENCH_BIN", "vtebench"),
        ("TB_BIN", "tb"),
        ("HYPERFINE_BIN", "hyperfine"),
        ("KITTEN_BIN", "kitten"),
        ("DOOM_BIN", "doom-fire"),
        ("TYPOMETER_BIN", "typometer"),
        ("WL_LATENCY_BIN", "wl-latency"),
    ):
        tool = tools.get(name)
        if tool and tool.found:
            out[key] = tool.path
    # Outside the results root on purpose: the cat payloads are a pure
    # function of (kind, size, seed), so every run can share the 450 MB
    # rather than write its own copy.
    out["PAYLOAD_DIR"] = str(repo / "target/bench-payloads")
    # nixpkgs installs vtebench's workload definitions outside the binary;
    # without -b pointing at them every payload comes out empty.
    if benchmarks := os.environ.get("FELIS_BENCH_VTEBENCH_BENCHMARKS"):
        out["VTEBENCH_BENCHMARKS"] = benchmarks
    return out


def cached_grid(root: Path, fld: field.Field) -> field.Size | None:
    """felis's window, probed once per results root and cached in `<root>/grid`.

    All four fields: the cell grid is what each terminal is asked for,
    and the pixel pair is what says the cells were the same size. Cached
    so that resuming a crashed run does not re-probe — and, more
    importantly, does not re-probe into a *different* answer after the
    desktop has changed underneath it.
    """
    cache = root / "grid"
    if grid := field.read_size(cache):
        return grid
    print("probing felis's grid (the field is pinned to it)...", flush=True)
    grid = suites.probe_grid(fld)
    if grid is None:
        print(
            "!! could not probe felis's grid; every terminal would run at its "
            "own default size and the suites would not be comparable",
            file=sys.stderr,
        )
        return None
    cache.write_text(grid.text())
    return grid


def explicit_grid(params: dict[str, str], pin) -> tuple[int, int] | None:
    """`GRID_ROWS`/`GRID_COLS`, validated as the pair they only mean together.

    An explicit grid overrides the reference, and only a pin that can
    resize can bring felis to it — felis has no size flag, so under a
    floating desktop the request would hold for every terminal except
    the one the others are being compared against.
    """
    rows, cols = params.get("GRID_ROWS"), params.get("GRID_COLS")
    if not rows and not cols:
        return None
    if not (rows and cols):
        raise ValueError("GRID_ROWS and GRID_COLS are one pin; pass both or neither")
    if not pin.can_resize:
        raise ValueError(
            f"the {pin.name} pin cannot resize a window, and felis has no size "
            "flag, so an explicit GRID_ROWS/GRID_COLS could not reach the one "
            "terminal the rest are pinned to"
        )
    return int(rows), int(cols)


def requested_rounds(params: dict[str, str]) -> int:
    raw = params.get("ROUNDS", "1")
    if not raw.isdigit() or int(raw) < 1:
        raise ValueError(f"ROUNDS wants a whole number of at least 1, got {raw!r}")
    return int(raw)


def suite_dir(root: Path, name: str, index: int, count: int) -> Path:
    """One round writes today's layout; several write `round-N/` under it.

    So a quick run's results root and every root written before rounds
    existed are the same directory shape, and only a repeated run pays
    the extra level.
    """
    return root / name if count == 1 else root / name / f"round-{index}"


def check_resume(root: Path, count: int) -> None:
    """Refuse a resume the layout cannot answer, before a window opens.

    A results root is written for one round count: resuming a
    three-round root with two would load a stale `round-3` under a
    subtitle claiming two, and either layout mixed with the other has
    no reading at all. Discovering that in the loaders would cost the
    hour of legs that ran first.
    """
    if not root.is_dir():
        return
    recorded = None
    if (meta := root / "meta.json").exists():
        try:
            params = json.loads(meta.read_text()).get("params") or {}
            recorded = int(params.get("ROUNDS", 1))
        except (json.JSONDecodeError, TypeError, ValueError):
            recorded = 1
        if recorded != count:
            raise ValueError(
                f"{root} was measured with ROUNDS={recorded} and this run asks "
                f"for ROUNDS={count}. Point --out at a new root, or ask for "
                f"ROUNDS={recorded} to resume this one."
            )
    for name in HARNESSES:
        suite = root / name
        if not suite.is_dir():
            continue
        # Raises by itself where the two layouts are mixed.
        layout = loaders.rounds(suite)
        if layout != [suite] and count == 1:
            raise ValueError(
                f"{suite} holds {len(layout)} round directories and this run "
                "asks for ROUNDS=1, which writes its legs directly there. "
                f"Resume it with ROUNDS={len(layout)} or start a new root."
            )
        if layout != [suite] and recorded is None and count != len(layout):
            # No meta.json to state the count, so the directories are
            # the only record of it, and a crashed run is
            # indistinguishable from a finished one: resuming under a
            # different count would load a round nobody asked for.
            raise ValueError(
                f"{suite} holds {len(layout)} round directories and the root "
                f"records no ROUNDS, so it reads as a ROUNDS={len(layout)} run "
                f"and this one asks for ROUNDS={count}. Start a new root."
            )
        if layout == [suite] and count > 1 and loaders.has_legs(suite):
            raise ValueError(
                f"{suite} holds legs from a ROUNDS=1 run and this run asks for "
                f"ROUNDS={count}, which writes them under round-N. Resume it "
                "with ROUNDS=1 or start a new root."
            )


def mismatches(root: Path) -> list[str]:
    """Terminals that did not end up in the pinned condition after all.

    The harnesses verify the pin from inside the terminal instead of
    trusting the size flag, because a window manager, a screen edge or a
    missing font all defeat it silently.
    """
    out = []
    for suite in sorted(path for path in root.iterdir() if path.is_dir()):
        out += [f"{suite.name}: {note}" for note in loaders.mismatch_notes(suite)]
    return out


# ── run ──────────────────────────────────────────────────────────────


def missing_ghostty(field_names: list[str]) -> list[str]:
    """The ghostty builds a full field lacks.

    Only the pair shows whether a ghostty result belongs to its release or
    to a regression that exists only on tip: its Linux keystroke latency
    went from 10 ms to seconds between 1.3.1 and tip.
    """
    return [name for name in ("ghostty", "ghostty-tip") if name not in field_names]


def platform_key() -> str:
    return "darwin" if sys.platform == "darwin" else "linux"


class Tee:
    """Console and run.log at once, so a finished run keeps its narration."""

    def __init__(self, sink):
        self.sink = sink

    def write(self, text: str) -> int:
        self.sink.write(text)
        return sys.__stdout__.write(text)

    def flush(self) -> None:
        self.sink.flush()
        sys.__stdout__.flush()


def run_suite(
    name: str, fld: field.Field, results: Path, params: dict[str, str]
) -> bool:
    """Run one suite in-process, teeing its narration to run.log.

    In-process rather than as a subprocess: the suites need the resolved
    field, the pinned presentation and the grid, and passing all of that
    through the environment was the only reason they were ever separate
    programs.
    """
    results.mkdir(parents=True, exist_ok=True)
    field.clear_attempts(results)
    print(f"\n=== {name} " + "=" * max(0, 55 - len(name)), flush=True)
    # Beside the artifacts it explains: which terminal ran when is what
    # a reader needs to tell drift over the run from a slow terminal,
    # and the rotation makes it different every round. Never rewritten:
    # on a resume this file is the order the legs already on disk ran
    # in, and the skipped ones will not run again to restore it.
    order = results / "order"
    if not order.exists():
        order.write_text("\n".join(fld.legs()) + "\n")
    before = len(fld.mismatches)
    log = results / "run.log"
    with log.open("w") as sink, contextlib.redirect_stdout(Tee(sink)):
        try:
            ok = suites.SUITES[name](fld, results, params)
        except Exception as err:  # noqa: BLE001
            # One suite failing still leaves the rest of the field worth
            # measuring, so the driver carries on and the report shows
            # what is present.
            print(f"!! {name} failed: {err}", file=sys.stderr)
            ok = False
    # Each leg already wrote its own note as it happened, so this only
    # picks up whatever a suite added outside a leg.
    for note in fld.mismatches[before:]:
        field.write_mismatch(results, note)
    return ok


def cmd_run(args: argparse.Namespace, repo: Path) -> int:
    felis_bin = Path(args.felis_bin or repo / "target/release/felis").resolve()
    if not os.access(felis_bin, os.X_OK):
        print(f"felis not built: {felis_bin} (cargo build --release)", file=sys.stderr)
        return 1

    # Selected and prepared before anything reads a scale or writes a
    # parameter: the pin is where the scale of the display the windows
    # will map on comes from, and `envinfo` has no Linux source for it.
    # A desktop with no usable pin refuses the run rather than
    # publishing an unequal comparison.
    pin = wm.select()
    try:
        pin_scale = pin.prepare()
    except wm.PinRefused as err:
        print(err, file=sys.stderr)
        return 1

    try:
        tools = resolve_field(args.use_path, parse_overrides(args.tool))
    except (ValueError, KeyError, FileNotFoundError) as err:
        print(err, file=sys.stderr)
        return 2
    if not os.environ.get("FELIS_BENCH_SHELL") and not args.use_path:
        print(
            "warning: not inside `nix develop .#bench` — the field will come "
            "from this host's PATH and will not reproduce elsewhere "
            "(`just bench-vs` enters that shell for you)",
            file=sys.stderr,
        )
    field_names = [t for t in TERMINALS if tools[t].found]
    if args.terminals:
        if unknown := set(args.terminals) - set(TERMINALS):
            print(
                f"--terminals: unknown: {', '.join(sorted(unknown))}", file=sys.stderr
            )
            return 2
        field_names = [t for t in field_names if t in args.terminals]
    elif missing := missing_ghostty(field_names):
        print(
            f"missing from the field: {', '.join(missing)}. A full run measures "
            "ghostty's release and its tip side by side. On Linux both come "
            "from `nix develop .#bench`, so a missing one usually means its "
            "store path was garbage-collected: re-enter the shell. On macOS "
            "fetch the tip with `just bench-vs-fetch-tip` and pass the "
            "--tool ghostty-tip=<path> it prints. Name the field with "
            "--terminals to run without either.",
            file=sys.stderr,
        )
        return 1
    if not field_names:
        print(
            "no comparison terminal found; nothing to compare felis against",
            file=sys.stderr,
        )
        return 1

    params = dict(DEFAULT_PARAMS)
    for override in args.param or []:
        key, _, value = override.partition("=")
        if not value:
            print(f"--param wants KEY=VALUE, got {override!r}", file=sys.stderr)
            return 2
        params[key] = value
    try:
        count = requested_rounds(params)
    except ValueError as err:
        print(err, file=sys.stderr)
        return 2

    wanted = args.suites or list(HARNESSES)
    # Resolved, because these paths are handed to a wrapper script that
    # runs *inside* the terminal under test, and not every terminal
    # gives it this process's working directory: wezterm starts its
    # shell in the user's home, so a relative `--out` writes the run's
    # markers somewhere nobody is watching and the leg times out.
    root = Path(
        args.out or repo / "target/crossterm-bench" / time.strftime("%Y%m%d-%H%M%S")
    ).resolve()
    # Before the grid probe, which is the first thing to open a window:
    # a layout this run cannot write is worth an hour of the reader's
    # time only if it is found now.
    try:
        check_resume(root, count)
    except ValueError as err:
        print(err, file=sys.stderr)
        return 2
    root.mkdir(parents=True, exist_ok=True)
    print(f"results root: {root}")
    print(f"field: felis + {', '.join(field_names)}")

    machine = envinfo.machine()
    try:
        explicit = explicit_grid(params, pin)
    except ValueError as err:
        print(err, file=sys.stderr)
        return 2
    print(f"window pin: {pin.name}")
    fld = field.Field(
        binaries={n: tools[n].path for n in field_names},
        felis_bin=str(felis_bin),
        pres=field.Presentation(
            family=params["FONT_FAMILY"],
            pt=float(params["FONT_PT"]),
            # Recorded as run context, never folded into the pin: the
            # client scales `font.size` by the window's scale factor
            # itself (field.py, item 3). The pin's number wins where it
            # has one, because that is the scale it converts through.
            scale=pin_scale or envinfo.main_backing_scale(machine) or 1.0,
            scrollback=int(params["SCROLLBACK"]),
        ),
        pin=pin,
    )
    if explicit:
        fld.pres = replace(fld.pres, rows=explicit[0], cols=explicit[1])
        params["GRID_SOURCE"] = "explicit"
    elif grid := cached_grid(root, fld):
        fld.pres = replace(
            fld.pres,
            rows=grid.rows,
            cols=grid.cols,
            xpixel=grid.xpixel,
            ypixel=grid.ypixel,
        )
    print(fld.pres.summary())
    params |= fld.pres.as_params()
    # A pin that could not be applied has to travel with the results,
    # not stay in this terminal's scrollback: the chart lines the bars
    # up either way, so the caveat has to reach whoever reads it.
    if escaped := field.unpinned_scrollback(fld.binaries):
        params["SCROLLBACK_UNPINNED"] = "; ".join(escaped)
        for note in escaped:
            print(f"note: scrollback not pinned for {note}")

    meta = {
        "date": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "suites": " ".join(wanted),
        **envinfo.collect(
            repo,
            felis_bin,
            [
                envinfo.describe_tool(tool.name, tool.path, tool.pinned)
                for tool in tools.values()
            ],
            params,
            machine,
        ),
    }
    (root / "meta.json").write_text(
        json.dumps(meta, indent=2, ensure_ascii=False) + "\n"
    )
    # Before the suites, not with the report: a full pass is over an
    # hour, and a binary that is not this checkout's is worth knowing
    # about while re-running is still cheap.
    if drift := (meta.get("felis") or {}).get("drift"):
        print(f"!! felis provenance: {drift}", file=sys.stderr)

    suite_params = harness_params(tools, params, repo)
    # A full pass runs for over an hour on a desktop nobody is allowed
    # to touch, which is exactly how the display parks and every
    # windowed suite after that measures a terminal drawing to nothing.
    awake = field.keep_display_awake()
    try:
        for index in range(1, count + 1):
            # Rotated, not shuffled: over three rounds a rotation moves
            # every terminal, and the position is what carries whatever
            # drifts over a run.
            legs = field.rotate(fld.legs(), index - 1)
            if count > 1:
                print(f"\n##### round {index}/{count}: {' '.join(legs)}", flush=True)
            per_round = replace(fld, leg_order=legs)
            for name in wanted:
                harness = HARNESSES[name]
                if not harness.runs_on(platform_key()):
                    print(
                        f"!! {name} skipped: not measurable on {platform_key()} yet",
                        file=sys.stderr,
                    )
                    continue
                needed = harness.needs(platform_key())
                if missing := [t for t in needed if not tools[t].found]:
                    print(
                        f"!! {name} skipped: missing {', '.join(missing)}",
                        file=sys.stderr,
                    )
                    continue
                run_suite(
                    name,
                    per_round,
                    suite_dir(root, name, index, count),
                    suite_params,
                )
    finally:
        # Both hold something the desktop keeps until it is told
        # otherwise, so a suite that raised must not leave the display
        # inhibited or a window floating.
        pin.release()
        if awake is not None:
            awake.terminate()

    print()
    if broken := mismatches(root):
        print(
            "!! the field was not uniform — these legs are not comparable:",
            file=sys.stderr,
        )
        for line in broken:
            print(f"   {line}", file=sys.stderr)
    measured = collect(root, None)
    if not measured:
        print(f"no usable results under {root}", file=sys.stderr)
        return 1
    revision = (meta.get("felis") or {}).get("revision") or "unknown"
    report.write(
        root,
        measured,
        root / "report.md",
        root / "png",
        args.title or f"felis vs other terminals — {revision}",
    )
    print(f"\nopen {root / 'report.md'}")
    return 0


def cmd_report(args: argparse.Namespace, _repo: Path) -> int:
    root = args.root
    if not root.is_dir():
        print(f"no such results dir: {root}", file=sys.stderr)
        return 1
    suites = collect(root, args.suite)
    if not suites:
        print(f"no harness results found under {root}", file=sys.stderr)
        return 1
    report.write(
        root,
        suites,
        args.output or root / "report.md",
        args.png_dir or root / "png",
        args.title or "felis vs other terminals",
    )
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    run = sub.add_parser("run", help="drive the suites, then write the report")
    run.add_argument("suites", nargs="*", choices=list(HARNESSES), help="default: all")
    run.add_argument(
        "--out", help="results root (default target/crossterm-bench/<stamp>)"
    )
    run.add_argument("--felis-bin", help="default target/release/felis")
    run.add_argument(
        "--use-path",
        action="store_true",
        help="ignore the .#bench pins and measure the terminals on PATH",
    )
    run.add_argument(
        "--param",
        action="append",
        metavar="KEY=VALUE",
        help=f"override a workload knob ({', '.join(DEFAULT_PARAMS)})",
    )
    run.add_argument(
        "--tool",
        action="append",
        metavar="NAME=PATH",
        help="name a binary for one entry in the field. Naming an existing "
        "one replaces it: `ghostty-tip` supplies the tip on macOS and swaps "
        "the pinned one on Linux (fetch_ghostty_tip.py prints the path). "
        "Either way the version and path land in meta.json and the row is "
        "marked unpinned.",
    )
    run.add_argument(
        "--terminals",
        nargs="+",
        metavar="NAME",
        help="limit the comparison field to these terminals; felis always "
        "runs its own leg",
    )
    run.add_argument("--title")

    rep = sub.add_parser("report", help="render charts from an existing results root")
    rep.add_argument("root", type=Path)
    rep.add_argument(
        "--suite",
        choices=sorted(HARNESSES),
        help="read root as one harness's $RESULTS dir",
    )
    rep.add_argument("-o", "--output", type=Path)
    rep.add_argument("--png-dir", type=Path)
    rep.add_argument("--title")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo = Path(__file__).resolve().parent.parent.parent
    return cmd_run(args, repo) if args.command == "run" else cmd_report(args, repo)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

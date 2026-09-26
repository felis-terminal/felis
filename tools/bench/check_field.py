#!/usr/bin/env python3
"""Check the field before spending half an hour measuring with it.

Prints each terminal's pinned launch argv, then (unless --dry) launches
each one on a throwaway command and checks the two things a suite needs:

  runs    the wrapper script executes inside the window — what
          vtebench, termbench, cat, kittenbench, doom-fire and memory wait
          for. They wait on a marker file, not on the process, so a terminal
          that runs the wrapper is usable even if the app then lingers.
  exits   the launch process exits by itself once its command is done.
          No suite waits on it (every one terminates its launch), so a
          terminal that lingers is still usable; the line says whether
          its windows close themselves when a run is interrupted.

Worth its own entry point because a failure is not loud during a run:
a terminal that refuses a flag just leaves a bar missing from a chart
half an hour later, after burning a suite timeout per leg. The reported grid is also the cheapest way to see a window manager
eating the pin — with paneru up, a field pinned to 40 rows comes back as
137/137/123/136.

Run it after changing anything in field.py.
"""

from __future__ import annotations

import argparse
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import crossterm  # noqa: E402
import field  # noqa: E402
import suites  # noqa: E402
import wm  # noqa: E402


def check_one(name: str, launch: field.Launch, work: Path, timeout: float) -> str:
    """Launch `name` on a wrapper that exits at once; report what happened."""
    marker, size = work / f"{name}.ran", work / f"{name}.size"
    wrapper = field.wrapper_script(
        [
            f"stty size > {shlex.quote(str(size))}",
            f"touch {shlex.quote(str(marker))}",
        ],
        settle=0,
    )
    proc = field.launch_terminal(launch, wrapper)
    ran = exited = None
    started = time.monotonic()
    while (elapsed := time.monotonic() - started) < timeout:
        if ran is None and marker.exists():
            ran = elapsed
        if proc.poll() is not None:
            exited = elapsed
            break
        time.sleep(0.25)
    if ran is None and marker.exists():
        ran = time.monotonic() - started
    if proc.poll() is None:
        proc.terminate()
    wrapper.unlink(missing_ok=True)

    lines = []
    if ran is not None:
        grid = field.read_grid(size)
        where = f", grid {grid[1]}x{grid[0]}" if grid else ""
        lines.append(f"  runs:  yes ({ran:.0f}s){where}")
    else:
        lines.append(
            f"  runs:  NO in {timeout:.0f}s — the wrapper never executed. A "
            "first-ever launch of a freshly downloaded bundle can be this "
            "slow; re-run before believing it."
        )
    if exited is not None:
        lines.append(f"  exits: yes ({exited:.0f}s)")
    else:
        lines.append("  exits: NO — usable; each suite terminates the launch itself")
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--dry", action="store_true", help="print the argv only, launch nothing"
    )
    parser.add_argument("--rows", type=int, default=40)
    parser.add_argument("--cols", type=int, default=120)
    # 60s, not 30: a bundle's first-ever launch pays a cold start plus,
    # on a freshly downloaded one, Gatekeeper's scan. Observed here — a
    # just-unpacked ghostty tip took 20s while every already-warm
    # terminal in the same pass timed out at 30s on a loaded machine,
    # which reads as "the field is broken" when it is only slow.
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument(
        "--tool",
        action="append",
        metavar="NAME=PATH",
        help="same as crossterm.py's: check a build before a run commits "
        "half an hour to it, e.g. --tool ghostty-tip=<path>",
    )
    args = parser.parse_args(argv)

    try:
        tools = crossterm.resolve_field(False, crossterm.parse_overrides(args.tool))
    except (ValueError, KeyError, FileNotFoundError) as err:
        raise SystemExit(str(err)) from err
    pres = field.Presentation(rows=args.rows, cols=args.cols)
    fld = field.Field(
        binaries={n: tools[n].path for n in crossterm.TERMINALS if tools[n].found},
        felis_bin="",
        pres=pres,
    )
    print(pres.summary())
    # A warning, not a refusal like a suite's: seeing a tiling WM eat the
    # requested rows is one of the things this script is for. The legs
    # below are launched bare, without the window pin a suite applies.
    pin = wm.select()
    if pin.name != "floating":
        print(
            f"note: the {pin.name} pin is not applied here, so the grids "
            "below are the desktop's own"
        )
    print()

    work = Path(tempfile.mkdtemp(prefix="felis-field-check."))
    failed = False
    try:
        for name in crossterm.TERMINALS:
            launch = fld.launch(name)
            if launch is None:
                print(f"{name}: not in the field")
                continue
            print(f"{name}:")
            print(f"  argv: {shlex.join(launch.argv)}")
            if launch.env:
                print(f"  env:  {' '.join(f'{k}={v}' for k, v in launch.env.items())}")
            if not args.dry:
                report = check_one(name, launch, work, args.timeout)
                print(report)
                failed = failed or "runs:  NO" in report
            for path in launch.tmp:
                path.unlink(missing_ok=True)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

#!/usr/bin/env python3
"""niri: the one tiling compositor in the field that can place a window back.

Every window is tiled and every size flag is overridden, so the field
would be whatever the compositor handed out. niri's IPC has runtime
actions for exactly the two moves that undo this — float one window,
then give it a size — so the pin asks the compositor instead of asking
each terminal, and reads the answer back from the tty rather than from
the compositor's own geometry, which counts the frame the terminal draws
its padding in.

Runtime actions only. The user's `config.kdl` is never read or written:
a bench that edits it changes the desktop the user comes back to, and
`niri msg action` reaches the same placement without persisting
anything.

Units: `ws_xpixel`/`ws_ypixel` are physical pixels and niri's
`set-window-width`/`-height` take logical ones, so every target crosses
the output's scale on the way in — at 2x, a 1400 px window is asked for
as 700 — and nothing is ever compared across the two.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import field  # noqa: E402

from .base import (  # noqa: E402
    FAILED,
    PINNED,
    PinRefused,
    PinResult,
    Target,
    fall_back_to_stty,
    wait_settled,
)

# Five is enough for the padding round-trip to converge and few enough
# that a window fighting back (a compositor rule, a size constraint)
# fails the leg in seconds rather than holding the run.
MAX_ATTEMPTS = 5
# Longer than one resize round trip of the slowest client in the field.
SETTLE_SECS = 1.0


def msg(*args: str) -> str | None:
    """`niri msg --json <args>`, or None when niri cannot answer."""
    try:
        done = subprocess.run(
            ["niri", "msg", "--json", *args],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return done.stdout if done.returncode == 0 else None


def action(*args: str) -> bool:
    try:
        done = subprocess.run(
            ["niri", "msg", "action", *args], capture_output=True, timeout=10
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return done.returncode == 0


def output_scale(outputs: dict, focused: str | None) -> tuple[str, float] | None:
    """The scale of the output the windows will map on, and its name.

    One output is the answer; several make it the focused one, since
    that is where a fresh window lands. Neither available is a refusal
    upstream rather than an assumed 1.0: every pixel target crosses this
    number, and assuming it silently halves or doubles the field.
    """
    usable = {
        name: entry["logical"]["scale"]
        for name, entry in outputs.items()
        if (entry.get("logical") or {}).get("scale")
    }
    if not usable:
        return None
    if len(usable) == 1:
        return next(iter(usable.items()))
    if focused in usable:
        return focused, usable[focused]
    return None


def match_window(windows: list[dict], tree: set[int]) -> tuple[int | None, str]:
    """The one mapped window belonging to a launch's process tree.

    The window's pid is not always the launch pid — `wezterm start`
    spawns `wezterm-gui`, which owns the window — so the match is over
    the whole tree. Zero or several is reported rather than resized: a
    guess would place somebody else's window.
    """
    found = [w["id"] for w in windows if w.get("pid") in tree]
    if len(found) == 1:
        return found[0], ""
    if not found:
        return None, "no window of this launch is mapped"
    return None, f"{len(found)} windows share this launch's processes"


def to_logical(physical: float, scale: float) -> int:
    """Physical pixels — what the tty reports — as the logical ones niri takes."""
    return max(1, round(physical / scale))


def cell_step(
    logical: int,
    got_cells: int,
    got_pixels: int,
    want_cells: int,
    scale: float,
    reference_cell: float = 0.0,
) -> int:
    """The logical size to ask for next, from the cell size the tty just showed.

    The window is larger than its cells by whatever padding and
    decoration the terminal draws, and that offset is unknown per
    terminal — so the step moves the *current* size by the missing cells
    rather than computing an absolute target, and the padding cancels.

    A terminal that leaves TIOCGWINSZ's pixel fields at zero would
    otherwise ask for its own size five times over and end up tty-pinned
    for want of a cell size, so the reference window's cell stands in.
    It is the wrong cell by however much the two fonts differ, which the
    next iteration measures away in cells.
    """
    cell = (
        got_pixels / got_cells if got_cells > 0 and got_pixels > 0 else reference_cell
    )
    if got_cells <= 0 or cell <= 0:
        return logical
    return to_logical(logical * scale + (want_cells - got_cells) * cell, scale)


class NiriPin:
    name = "niri"
    can_resize = True

    def __init__(self) -> None:
        self.scale = 1.0
        self.output = ""

    @classmethod
    def detect(cls) -> NiriPin | None:
        if not os.environ.get("NIRI_SOCKET") or not shutil.which("niri"):
            return None
        return cls()

    def prepare(self) -> float:
        outputs = msg("outputs")
        if outputs is None:
            raise PinRefused("niri is running but `niri msg outputs` failed")
        try:
            parsed = json.loads(outputs)
        except json.JSONDecodeError as err:
            raise PinRefused(f"niri's output list did not parse: {err}") from err
        focused = json.loads(msg("focused-output") or "null")
        chosen = output_scale(parsed, (focused or {}).get("name"))
        if chosen is None:
            raise PinRefused(
                "niri reports no output to read a scale from (with several "
                "outputs, focus one first); every pixel target crosses that "
                "number, so a run cannot assume it"
            )
        self.output, self.scale = chosen
        return self.scale

    # ── placement ────────────────────────────────────────────────────

    def window_for(
        self, launch_pid: int, timeout: float = 20.0
    ) -> tuple[int | None, str]:
        deadline = time.monotonic() + timeout
        detail = "niri listed no windows"
        while time.monotonic() < deadline:
            listed = msg("windows")
            if listed is not None:
                found, detail = match_window(
                    json.loads(listed), field.process_tree(launch_pid)
                )
                if found is not None:
                    return found, ""
                if "share this launch" in detail:
                    return None, detail
            time.sleep(0.5)
        return None, detail

    def entry(self, window_id: int) -> dict | None:
        listed = msg("windows")
        if listed is None:
            return None
        for window in json.loads(listed):
            if window.get("id") == window_id:
                return window
        return None

    def float_window(self, window_id: int) -> bool:
        """Assert floating rather than toggle it.

        `toggle-window-floating` would tile a window the user's own
        rules had already floated, which is the opposite of the pin.
        """
        entry = self.entry(window_id)
        if entry is None:
            return False
        if entry.get("is_floating"):
            return True
        action("move-window-to-floating", "--id", str(window_id))
        time.sleep(0.5)
        entry = self.entry(window_id)
        return bool(entry and entry.get("is_floating"))

    def logical_size(self, window_id: int) -> tuple[int, int] | None:
        entry = self.entry(window_id)
        size = (entry or {}).get("layout", {}).get("window_size")
        return (int(size[0]), int(size[1])) if size else None

    def pin(self, launch_pid: int, target: Target) -> PinResult:
        window_id, detail = self.window_for(launch_pid)
        if window_id is None:
            return PinResult(FAILED, field.read_size(target.size), detail)
        if not self.float_window(window_id):
            return PinResult(
                FAILED,
                field.read_size(target.size),
                "the window would not float, so its size is the layout's",
            )
        size = wait_settled(target.size, 20)
        if target.discovering:
            return PinResult(PINNED, size)

        def holds(seen: field.Size | None) -> bool:
            return seen is not None and (seen.rows, seen.cols) == (
                target.rows,
                target.cols,
            )

        def stays(seen: field.Size | None) -> field.Size | None:
            # A slow client (ghostty tip) applies the configures of an
            # earlier attempt after a later one already read back as the
            # target, and then runs the workload at 106x7.
            if not holds(seen):
                return None
            time.sleep(SETTLE_SECS)
            again = field.read_size(target.size)
            return again if holds(again) else None

        for _ in range(MAX_ATTEMPTS):
            # The current size first: a window that already matches is
            # pinned without a resize, so waiting for a change cannot
            # time out on a correct window.
            if held := stays(size):
                return PinResult(PINNED, held)
            size = field.read_size(target.size) or size
            logical = self.logical_size(window_id)
            if logical is None or size is None:
                break
            width = cell_step(
                logical[0],
                size.cols,
                size.xpixel,
                target.cols,
                self.scale,
                (target.xpixel or 0) / target.cols,
            )
            height = cell_step(
                logical[1],
                size.rows,
                size.ypixel,
                target.rows,
                self.scale,
                (target.ypixel or 0) / target.rows,
            )
            action("set-window-width", "--id", str(window_id), str(width))
            action("set-window-height", "--id", str(window_id), str(height))
            size = self.wait_for_change(target.size, size)
        if held := stays(size):
            return PinResult(PINNED, held)
        size = field.read_size(target.size) or size
        got = "nothing" if size is None else f"{size.cols}x{size.rows}"
        return fall_back_to_stty(
            target,
            size,
            f"niri left the window at {got}, not the reference "
            f"{target.cols}x{target.rows}",
        )

    def wait_for_change(
        self, path: Path, previous: field.Size, timeout: float = 2.0
    ) -> field.Size | None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            size = field.read_size(path)
            if size is not None and size != previous:
                return size
            time.sleep(0.25)
        return field.read_size(path)

    def focus_for_typing(self, launch_pid: int) -> tuple[int | None, str]:
        """Focus the launch's window and confirm niri agrees it is focused.

        Confirmed rather than assumed: `focus-window` returns as soon as
        niri has taken the request, and an injected keystroke sent into
        the gap before the focus actually moves lands in the previous
        window.
        """
        window_id, detail = self.window_for(launch_pid)
        if window_id is None:
            return None, detail
        if not action("focus-window", "--id", str(window_id)):
            return None, f"niri would not focus window {window_id}"
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            listed = msg("focused-window")
            if listed and json.loads(listed).get("id") == window_id:
                return window_id, ""
            time.sleep(0.2)
        return None, f"window {window_id} did not take the keyboard"

    def release(self) -> None:
        return None

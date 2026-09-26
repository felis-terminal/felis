#!/usr/bin/env python3
"""The macOS tilers, which the bench refuses to run under."""

from __future__ import annotations

import subprocess

from .base import PinResult, PinRefused, Target

# Tiling window managers that override a window's requested size and
# offer no runtime placement to pin it back. The list is short on
# purpose: a name here costs a run nothing, while a missing one costs a
# whole suite's comparability — it would fall through to the floating
# pin, which believes every size flag.
TILING_WMS = ("paneru", "yabai", "amethyst")


def running_window_manager() -> str | None:
    """`pgrep -x`, matching the process NAME rather than its command line.

    The predecessor matched `pgrep -f "paneru launch"`, a command line
    paneru does not have — it runs as a bare `paneru` — so the guard
    passed while paneru was up. Verified on this machine: with paneru
    running, a field pinned to 40 rows came back as 137/137/123/136.
    """
    for name in TILING_WMS:
        if subprocess.run(["pgrep", "-x", name], capture_output=True).returncode == 0:
            return name
    return None


class MacTilerPin:
    """Detected so the run can refuse, not so it can be pinned.

    None of these expose a runtime "put this window here" action the way
    niri does, so there is nothing to pin with: every window is tiled,
    every size flag is overridden, and the numbers would be taken at
    whatever grid the tiler handed out.
    """

    name = "mac-tiler"
    can_resize = False

    def __init__(self, wm: str) -> None:
        self.wm = wm
        self.name = wm

    @classmethod
    def detect(cls) -> MacTilerPin | None:
        wm = running_window_manager()
        return cls(wm) if wm else None

    def prepare(self) -> float | None:
        raise PinRefused(
            f"{self.wm} is running and will override window sizes; stop it "
            "first (paneru is launchd-managed: 'paneru stop', then "
            "'paneru start')"
        )

    def pin(self, _launch_pid: int, _target: Target) -> PinResult:
        raise PinRefused(f"{self.wm} cannot pin a window")

    def focus_for_typing(self, _launch_pid: int) -> tuple[int | None, str]:
        return None, f"the {self.name} pin cannot focus a window by id"

    def release(self) -> None:
        return None

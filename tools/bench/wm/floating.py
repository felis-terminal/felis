#!/usr/bin/env python3
"""The desktop that lets a window keep the size it asked for."""

from __future__ import annotations

from .base import PINNED, PinResult, Target, fall_back_to_stty, wait_settled


class FloatingPin:
    """No placement of its own: each terminal's size flag already holds.

    Stock macOS, GNOME, a plain floating Wayland compositor. There is
    nothing to do beyond waiting for the window to stop moving and
    reading back what it settled on — and, when it settled somewhere
    else, leaving the tty pin for the wrapper. felis is the one leg that
    cannot be asked for a grid at all, which is why the reference is its
    window rather than a number chosen here.
    """

    name = "floating"
    can_resize = False

    @classmethod
    def detect(cls) -> FloatingPin | None:
        return cls()

    def prepare(self) -> float | None:
        return None

    def pin(self, _launch_pid: int, target: Target) -> PinResult:
        size = wait_settled(target.size, 20)
        if target.discovering:
            return PinResult(PINNED, size)
        if size is not None and (size.rows, size.cols) == (target.rows, target.cols):
            return PinResult(PINNED, size)
        got = "nothing" if size is None else f"{size.cols}x{size.rows}"
        return fall_back_to_stty(
            target,
            size,
            f"the window opened at {got}, not the reference "
            f"{target.cols}x{target.rows}",
        )

    def focus_for_typing(self, _launch_pid: int) -> tuple[int | None, str]:
        return None, f"the {self.name} pin cannot focus a window by id"

    def release(self) -> None:
        return None

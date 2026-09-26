#!/usr/bin/env python3
"""The window pin: how a desktop is asked to hold the whole field to one grid.

felis has no size flag, so the reference grid is felis's own window and
every other terminal is brought to it. Who brings it there depends on
the desktop: where windows float, each terminal's own size flag does the
work; where a window manager tiles, that flag is overridden and the only
party that can still place a window is the window manager itself. That
is what a `WindowPin` is — one implementation per desktop, selected by
detection, so supporting another one is a module here rather than an
edit at every launch site.

A pin answers with a status rather than leaving it to be inferred from
the grid afterwards:

- `pinned` — the window holds the reference grid.
- `tty-only` — the window could not be placed, so the kernel tty size
  is pinned instead (`<name>.stty`, applied by the wrapper). Every
  terminal then parses identical bytes, which keeps a throughput number
  meaningful, but the render area still differs and the chart says so.
- `failed` — neither held; the leg still runs and the chart carries why.

The status comes from the pin, never from the final `.size`: after the
`stty` fallback the cell grid matches, so a check that counted cells
alone would call that leg pinned.
"""

from __future__ import annotations

from typing import Protocol, runtime_checkable

from .base import (  # noqa: F401
    FAILED,
    PINNED,
    TTY_ONLY,
    PinRefused,
    PinResult,
    Target,
    settled,
    wait_settled,
)
from .floating import FloatingPin
from .mactiler import MacTilerPin
from .niri import NiriPin

# First match wins, so the entries that refuse or resize come before the
# one that always matches.
PINS = (MacTilerPin, NiriPin, FloatingPin)


@runtime_checkable
class WindowPin(Protocol):
    """What every desktop's pin has to answer."""

    name: str
    # Whether this pin can move a window to a grid it was not asked for
    # at launch. Only a resizing pin can bring felis — which has no size
    # flag — to an explicitly requested grid.
    can_resize: bool

    @classmethod
    def detect(cls) -> WindowPin | None: ...

    def prepare(self) -> float | None:
        """Once per run: the scale of the display the field maps on.

        None means this desktop has no source for it and the caller
        falls back to what `envinfo` recorded. `PinRefused` stops the
        run.
        """

    def pin(self, launch_pid: int, target: Target) -> PinResult: ...

    def focus_for_typing(self, launch_pid: int) -> tuple[int | None, str]:
        """The window id to aim an injected keystroke at, holding the keyboard.

        Only the latency suite asks, and only where the instrument
        injects through the compositor rather than through the OS: the
        id is what such an instrument refuses to type without. A desktop
        with no way to name and focus a window by id answers None with
        the reason, and the leg is skipped rather than typed into
        whatever happens to be in front.
        """

    def release(self) -> None: ...


def select(pins: tuple[type, ...] = PINS) -> WindowPin:
    """The pin for the desktop this run is on.

    The last entry matches unconditionally, so this always answers.
    """
    for candidate in pins:
        if found := candidate.detect():
            return found
    raise RuntimeError("no window pin matched, not even the floating one")

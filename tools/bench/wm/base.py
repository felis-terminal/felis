#!/usr/bin/env python3
"""What every pin answers with, and the waiting all of them share."""

from __future__ import annotations

import sys
import time
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import field  # noqa: E402

PINNED = "pinned"
TTY_ONLY = "tty-only"
FAILED = "failed"


class PinRefused(Exception):
    """The desktop cannot hold a field, so the run stops before it starts."""


@dataclass(frozen=True)
class Target:
    """What one leg is pinned to, and the files the handshake runs through.

    `rows`/`cols` are None in discovery mode — the probe that measures
    felis's own window and turns it into the reference every later leg
    is held to.
    """

    size: Path
    stty: Path
    rows: int | None = None
    cols: int | None = None
    xpixel: int | None = None
    ypixel: int | None = None

    @property
    def discovering(self) -> bool:
        return self.rows is None or self.cols is None


@dataclass(frozen=True)
class PinResult:
    """How a leg ended up, and the geometry the pin last saw."""

    status: str
    size: field.Size | None
    detail: str = ""

    @property
    def pinned(self) -> bool:
        return self.status == PINNED


def settled(size: field.Size | None, previous: field.Size | None) -> bool:
    """Two identical samples that are not the PTY's 24x80 default.

    felis keeps that default until its client attaches and resizes the
    PTY, and a compositor that animates the map would otherwise be
    sampled mid-resize.
    """
    if size is None or previous is None:
        return False
    if (size.rows, size.cols) == (24, 80):
        return False
    return size == previous


def wait_settled(
    path: Path, timeout: float, interval: float = 0.25
) -> field.Size | None:
    """Poll `<name>.size` until it holds still, or give up and return the last."""
    deadline = time.monotonic() + timeout
    previous = None
    size = field.read_size(path)
    while time.monotonic() < deadline:
        if settled(size, previous):
            return size
        time.sleep(interval)
        previous, size = size, field.read_size(path)
    return size


def fall_back_to_stty(
    target: Target, size: field.Size | None, detail: str
) -> PinResult:
    """Hand the tty pin to the wrapper, which is the only party holding the tty.

    `stty` acts on the wrapper's own terminal, so the driver cannot
    apply it from outside; it leaves the request and the wrapper applies
    it before the workload starts.
    """
    if target.rows is None or target.cols is None:
        return PinResult(FAILED, size, detail)
    target.stty.write_text(f"{target.rows} {target.cols}\n")
    return PinResult(TTY_ONLY, size, detail)

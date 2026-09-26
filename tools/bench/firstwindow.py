#!/usr/bin/env python3
"""When a launch's first window reaches the screen.

The startup suite times a launch to its first window rather than to its
exit, and the moment "a window reached the screen" is the platform's to
say, not the terminal's:

- **Wayland (niri).** A toplevel is mapped only once its first buffer is
  committed, and niri announces a window on its event stream only once
  it is mapped. So the event is the time to the first frame: a window
  the terminal has drawn into, not a request for one.
- **macOS.** The window server lists a window as on screen once it is
  ordered in. That proves the window is visible, not that the terminal
  has drawn anything into it: a window ordered in before its first frame
  counts as soon as it is ordered in.

Both watchers match windows over the launch's whole process tree, since
the window is not always the launched process's own (`wezterm start`
spawns `wezterm-gui`), and ignore every window that existed before the
launch.
"""

from __future__ import annotations

import ctypes
import json
import os
import queue
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import field  # noqa: E402

DARWIN = sys.platform == "darwin"


class TreeMatcher:
    """Is this pid one of the launch's? `pgrep` walks only when it has to."""

    def __init__(self, launch_pid: int) -> None:
        self.launch_pid = launch_pid
        self.known: dict[int, bool] = {}

    def __call__(self, pid: int | None) -> bool:
        if pid is None:
            return False
        if pid == self.launch_pid:
            return True
        if pid not in self.known:
            tree = field.process_tree(self.launch_pid)
            self.known.update({p: True for p in tree})
            self.known.setdefault(pid, pid in tree)
        return self.known[pid]


def niri_windows(event: dict) -> tuple[list[dict], bool]:
    """The windows one niri event names, and whether it is the full roster.

    `WindowsChanged` replaces the whole list (niri sends it first, on
    connect); `WindowOpenedOrChanged` names one window that opened or
    changed. Anything else carries no window.
    """
    if "WindowsChanged" in event:
        return list(event["WindowsChanged"].get("windows") or []), True
    if "WindowOpenedOrChanged" in event:
        window = event["WindowOpenedOrChanged"].get("window")
        return ([window] if window else []), False
    return [], False


class NiriWatcher:
    """niri's event stream, read by a thread that stamps each line on arrival.

    One stream for the whole suite rather than one per launch: connecting
    costs a process spawn and a full roster, and a launch timed across
    that would carry it.
    """

    name = "niri event-stream: the window was mapped (first buffer committed)"

    def __init__(self) -> None:
        self.proc = subprocess.Popen(
            ["niri", "msg", "--json", "event-stream"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        self.events: queue.Queue[tuple[float, dict]] = queue.Queue()
        self.existing: set[int] = set()
        threading.Thread(target=self.read, daemon=True).start()
        # The roster niri sends on connect is what every later launch is
        # compared against.
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                _at, event = self.events.get(timeout=0.2)
            except queue.Empty:
                continue
            self.note(event)
            if "WindowsChanged" in event:
                break

    @classmethod
    def detect(cls) -> NiriWatcher | None:
        if not os.environ.get("NIRI_SOCKET") or not shutil.which("niri"):
            return None
        return cls()

    def read(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            at = time.monotonic()
            try:
                self.events.put((at, json.loads(line)))
            except json.JSONDecodeError:
                continue

    def note(self, event: dict) -> None:
        windows, full = niri_windows(event)
        if full:
            self.existing = set()
        self.existing |= {w["id"] for w in windows if "id" in w}
        if closed := event.get("WindowClosed"):
            self.existing.discard(closed.get("id"))

    def settle(self) -> None:
        """Fold in every event so far, so only a later window is new."""
        while True:
            try:
                _at, event = self.events.get_nowait()
            except queue.Empty:
                return
            self.note(event)

    def wait(self, launch_pid: int, timeout: float) -> float | None:
        """The monotonic time the launch's first window was mapped, or None."""
        mine = TreeMatcher(launch_pid)
        deadline = time.monotonic() + timeout
        while (left := deadline - time.monotonic()) > 0:
            try:
                at, event = self.events.get(timeout=left)
            except queue.Empty:
                break
            windows, _full = niri_windows(event)
            fresh = [w for w in windows if w.get("id") not in self.existing]
            self.note(event)
            if any(mine(w.get("pid")) for w in fresh):
                return at
        return None

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()


# CoreGraphics' option bits and the CFNumber/CFString constants they are read with.
ON_SCREEN_ONLY = 1
EXCLUDE_DESKTOP_ELEMENTS = 16
CF_NUMBER_SINT64 = 4
CF_STRING_UTF8 = 0x08000100


class MacWatcher:
    """The window server's on-screen list, polled a few milliseconds apart.

    Polled because nothing announces a window to a process that does not
    own it. Each poll is one `CGWindowListCopyWindowInfo` call, so the
    interval is the resolution and costs no fork.
    """

    name = "CGWindowList: a layer-0 window was on screen (not proof of drawn content)"
    interval = 0.004

    def __init__(self) -> None:
        cg = ctypes.CDLL(
            "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics"
        )
        cf = ctypes.CDLL(
            "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation"
        )
        cg.CGWindowListCopyWindowInfo.restype = ctypes.c_void_p
        cg.CGWindowListCopyWindowInfo.argtypes = [ctypes.c_uint32, ctypes.c_uint32]
        cf.CFArrayGetCount.restype = ctypes.c_long
        cf.CFArrayGetCount.argtypes = [ctypes.c_void_p]
        cf.CFArrayGetValueAtIndex.restype = ctypes.c_void_p
        cf.CFArrayGetValueAtIndex.argtypes = [ctypes.c_void_p, ctypes.c_long]
        cf.CFDictionaryGetValue.restype = ctypes.c_void_p
        cf.CFDictionaryGetValue.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        cf.CFNumberGetValue.restype = ctypes.c_bool
        cf.CFNumberGetValue.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_void_p,
        ]
        cf.CFStringCreateWithCString.restype = ctypes.c_void_p
        cf.CFStringCreateWithCString.argtypes = [
            ctypes.c_void_p,
            ctypes.c_char_p,
            ctypes.c_uint32,
        ]
        cf.CFRelease.argtypes = [ctypes.c_void_p]
        self.cg, self.cf = cg, cf
        self.keys = {
            name: cf.CFStringCreateWithCString(None, name.encode(), CF_STRING_UTF8)
            for name in ("kCGWindowOwnerPID", "kCGWindowLayer", "kCGWindowNumber")
        }
        self.existing: set[int] = set()

    @classmethod
    def detect(cls) -> MacWatcher | None:
        return cls() if DARWIN else None

    def number(self, entry: int, key: str) -> int | None:
        value = self.cf.CFDictionaryGetValue(entry, self.keys[key])
        if not value:
            return None
        out = ctypes.c_int64()
        if not self.cf.CFNumberGetValue(value, CF_NUMBER_SINT64, ctypes.byref(out)):
            return None
        return out.value

    def on_screen(self) -> list[tuple[int, int]]:
        """(window number, owner pid) of every on-screen layer-0 window."""
        listed = self.cg.CGWindowListCopyWindowInfo(
            ON_SCREEN_ONLY | EXCLUDE_DESKTOP_ELEMENTS, 0
        )
        if not listed:
            return []
        try:
            out = []
            for index in range(self.cf.CFArrayGetCount(listed)):
                entry = self.cf.CFArrayGetValueAtIndex(listed, index)
                if self.number(entry, "kCGWindowLayer") != 0:
                    continue
                number = self.number(entry, "kCGWindowNumber")
                pid = self.number(entry, "kCGWindowOwnerPID")
                if number is not None and pid is not None:
                    out.append((number, pid))
            return out
        finally:
            self.cf.CFRelease(listed)

    def settle(self) -> None:
        self.existing = {number for number, _pid in self.on_screen()}

    def wait(self, launch_pid: int, timeout: float) -> float | None:
        mine = TreeMatcher(launch_pid)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            at = time.monotonic()
            for number, pid in self.on_screen():
                if number not in self.existing and mine(pid):
                    return at
            time.sleep(self.interval)
        return None

    def close(self) -> None:
        for key in self.keys.values():
            self.cf.CFRelease(key)


def select() -> NiriWatcher | MacWatcher | None:
    """The watcher for this desktop, or None where there is no way to see a window map."""
    return MacWatcher.detect() or NiriWatcher.detect()

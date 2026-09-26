#!/usr/bin/env python3
"""The field: which binary each terminal is, and what they are all pinned to.

The suites in `suites.py` are only comparable to each other if they
agree, exactly, on how a terminal is launched. One definition here is
what makes that true; four copies of a kitty invocation drift — one
gains a pristine-config flag, another keeps the developer's own — and
the resulting charts silently compare different programs.

What gets pinned is the *workload*, never the strategy for serving it.
Frame pacing (kitty's `repaint_delay`, wezterm's `max_fps`, everyone's
vsync), the renderer backend, damage tracking and ghostty tip's
scrollback compression are all left exactly as shipped: they are the
performance decisions each terminal made, and a field with them
equalised measures a configuration nobody runs. Anything that changes
how much work there is to do gets pinned, because unequal work makes
the bars mean nothing.

Five things are pinned, each because leaving it free changes the
measurement rather than merely adding noise:

1. **The user's config is out.** kitty already took `--config NONE`,
   but alacritty and ghostty read `~/.config/…` unless told otherwise,
   so a developer with a ligature font, a different scrollback or vsync
   off in their own config was benchmarking *that* against a felis
   running its defaults. Every terminal now starts pristine, felis
   included, through a throwaway HOME: `directories::ProjectDirs` has no
   env override on macOS.

2. **The font family.** With configs off the defaults still differ:
   Menlo for kitty and alacritty, bundled JetBrains Mono for wezterm and
   ghostty, whatever fontconfig picks for foot. Different families mean
   different shaping cost and different cell advance widths — so the
   same cell grid becomes a different *pixel* area.

3. **The font size.** Every other terminal takes a point size and
   scales it to the display itself. felis's `font.size_px` is logical
   pixels — the client multiplies it by the window's scale factor
   (`felis-client` `effective_font_size_px`) — so the pin reaches it
   converted at the platform's logical-pixel size: a point is one on
   macOS and 4/3 of one under the Wayland convention the rest of the
   field converts against. Pre-multiplying by the display's backing
   scale would hand felis a value the client then scales a second time,
   which is why the backing scale is recorded as run context and never
   enters the pin.

4. **The scrollback depth.** The defaults span more than an order of
   magnitude — foot 1000, kitty 2000, alacritty 10 000, wezterm 3500,
   ghostty a byte budget — and the flooded half of the memory suite is
   decided entirely by which cap the workload hits first. felis's depth
   is a compile-time constant, so 10 000 rows is not a choice: it is
   the one terminal that cannot move, and the rest are moved to it.
   ghostty 1.3.1 is the one that gets away — it caps by bytes only,
   and no honest byte count maps to a row count — so it keeps its
   default and `unpinned_scrollback` reports that on the chart.

5. **The cursor does not blink.** felis, kitty and ghostty blink out of
   the box; alacritty, wezterm and foot do not. A blink is a repaint
   twice a second forever, which is invisible in a throughput number
   and is most of an idle CPU number.

The grid cannot be pinned the same way: felis has neither a cells-based
size flag nor a config key for one, so the window manager decides its
size and the rest of the field is pinned to whatever that turns out to
be. That makes the pin a request, not a guarantee, so `verify_grid`
reads it back from inside each terminal instead of trusting the flag.
"""

from __future__ import annotations

import contextlib
import ctypes
import glob
import json
import os
import re
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Collection, Iterator, Sequence
from dataclasses import dataclass, field as dc_field
from functools import lru_cache
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import envinfo  # noqa: E402

# The field, in the order the charts lay it out.
FIELD_ORDER = ("kitty", "alacritty", "wezterm", "ghostty", "ghostty-tip", "foot")

# A terminal that shares another's launch recipe — same flags, different
# binary. `ghostty-tip` is upstream's main standing next to the pinned
# release, so "how much did the unreleased work gain?" is one chart under
# one condition rather than two runs a reader has to trust were taken the
# same way. On Linux the bench shell pins it to the commit in
# dev/flake.lock, which lock maintenance advances; on macOS it enters
# only through `--tool ghostty-tip=<path>`.
LAUNCH_ALIAS = {"ghostty-tip": "ghostty"}

DARWIN = sys.platform == "darwin"

# Menlo ships with macOS and DejaVu Sans Mono with every desktop Linux;
# both are ligature-free, so no terminal pays a shaping cost the others
# skip.
DEFAULT_FONT_FAMILY = "Menlo" if DARWIN else "DejaVu Sans Mono"
DEFAULT_FONT_PT = 9.0 if DARWIN else 10.0

# /bin/ps, not ps: a nix procps in PATH lacks the macOS entitlement for
# rss/cputime and prints its keyword list to stdout instead.
PS = "/bin/ps" if Path("/bin/ps").exists() else "ps"


@dataclass(frozen=True)
class Presentation:
    """The condition every terminal in the field is held to."""

    family: str = DEFAULT_FONT_FAMILY
    pt: float = DEFAULT_FONT_PT
    # Physical pixels per logical pixel on the display the windows open
    # on. Run context, not a conversion factor: a 2x display paints four
    # times the pixels under the same pin, which is a fact about the run
    # that belongs beside the numbers.
    scale: float = 1.0
    scrollback: int = 10_000
    # felis's grid, measured by the discovery probe; None before it is
    # known. The pixel pair travels with it because the same cell count
    # at a different cell size is a different window: it is what makes a
    # font pin that did not apply visible.
    rows: int | None = None
    cols: int | None = None
    xpixel: int | None = None
    ypixel: int | None = None

    @property
    def px(self) -> float:
        """felis's `font.size_px`: the pinned point size, in its units.

        `font.size_px` is logical pixels and the client applies the
        window's scale factor to it (`felis-client`
        `effective_font_size_px`), so the backing scale does not belong
        here — but a point is not a logical pixel everywhere. macOS
        makes the logical pixel 1/72 inch, so a point is one and the pin
        transfers unchanged; the Wayland convention is 1/96 inch, which
        is what the rest of the field converts the same point size
        against, so felis has to convert too or it alone runs 25%
        smaller than the terminals it is being compared to.
        """
        return self.pt if DARWIN else self.pt * 96 / 72

    @property
    def pt_str(self) -> str:
        """`9` rather than `9.0` — wezterm is the only one wanting a float."""
        return f"{self.pt:g}"

    def summary(self) -> str:
        grid = f"{self.cols}x{self.rows} cells" if self.cols else "grid not pinned"
        return (
            f"{grid}, {self.family} {self.pt_str}pt "
            f"(felis font.size_px {self.px:g}, {self.scale:g}x display)"
        )

    def as_params(self) -> dict[str, str]:
        """The pinned condition as it lands in meta.json."""
        out = {
            "FONT_FAMILY": self.family,
            "FONT_PT": self.pt_str,
            # What felis's config actually received, in the logical
            # pixels `font.size_px` is defined in. Recorded separately
            # because it is the key that was written, and off macOS it
            # is not the same number as FONT_PT.
            "FONT_PX": f"{self.px:g}",
            # The display, not a conversion factor: at 2x the same pin
            # paints four times the pixels, and every render-bound
            # number here carries that.
            "FONT_SCALE": f"{self.scale:g}",
            "SCROLLBACK": str(self.scrollback),
            # Not a Presentation field: nothing flips it. Recorded
            # because "the cursor was not blinking" is a fact about the
            # run — half the field blinks by default and half does not,
            # and an idle repaint every half second lands in the memory
            # suite's CPU column.
            "CURSOR_BLINK": "off",
        }
        if self.rows and self.cols:
            out |= {"GRID_ROWS": str(self.rows), "GRID_COLS": str(self.cols)}
        return out

    def felis_config(self) -> str:
        """felis's side of the pin.

        No scrollback key appears here because felis has none: the depth
        is `felis_grid::DEFAULT_SCROLLBACK_ROWS`, a compile-time 10_000.
        That constant is why `scrollback` defaults to 10_000 — the one
        terminal that cannot be moved sets the number the rest are moved
        to.
        """
        return (
            "# Written by tools/bench/field.py — pinned to match the rest of\n"
            f"# the field. size_px is the field's {self.pt_str}pt converted to\n"
            "# the logical pixels the key is defined in.\n"
            "[font]\n"
            f'family = "{self.family}"\n'
            f"size_px = {self.px:g}\n"
            "features = []\n"
            "[cursor]\n"
            'blink = "never"\n'
        )


@dataclass
class Launch:
    """One terminal's pinned launch, ready to have a wrapper appended."""

    argv: list[str]
    env: dict[str, str] = dc_field(default_factory=dict)
    # Config files the launch owns; the caller removes them after.
    tmp: list[Path] = dc_field(default_factory=list)

    def command(self, wrapper: str | Path) -> list[str]:
        return [*self.argv, str(wrapper)]

    def shell_command(self, wrapper: str | Path) -> str:
        """One shell string, for hyperfine — which takes only one.

        `shlex.quote` is what keeps a family with a space in it
        ("DejaVu Sans Mono") from splitting into three arguments.
        """
        parts = [f"{k}={shlex.quote(v)}" for k, v in self.env.items()]
        if parts:
            parts.insert(0, "env")
        return " ".join([*parts, *(shlex.quote(a) for a in self.command(wrapper))])


def write_temp(suffix: str, body: str) -> Path:
    handle = tempfile.NamedTemporaryFile("w", suffix=suffix, delete=False)
    with handle:
        handle.write(body)
    return Path(handle.name)


@lru_cache(maxsize=8)
def ghostty_config_keys(binary: str) -> frozenset[str]:
    """The config keys a ghostty build accepts.

    Needed because the scrollback surface changed under us: 1.3.1 caps
    by bytes only (`scrollback-limit`, 10 MB), while tip renamed it to
    `scrollback-limit-bytes` (50 MB) and added
    `scrollback-limit-lines`. Guessing is not an option in either
    direction — an unknown key makes ghostty open an error dialog
    instead of the wrapper, and skipping the pin on a build that has it
    leaves the deepest scrollback in the field unpinned.
    """
    try:
        out = subprocess.run(
            [binary, "+show-config", "--default"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        # An empty set means "pin nothing", which is what a build we
        # cannot interrogate deserves. The launch itself fails later
        # with a better message than this probe could give.
        return frozenset()
    return frozenset(
        line.split("=", 1)[0].strip()
        for line in out.stdout.splitlines()
        if "=" in line and not line.startswith(" ")
    )


def unpinned_scrollback(binaries: dict[str, str]) -> list[str]:
    """Which terminals could not be held to the pinned depth, and why.

    A pin that silently failed to apply is worse than no pin: the chart
    still lines the bars up. So the ones that got away are named, and
    `report.py` prints them under the memory chart.
    """
    out = []
    for name in FIELD_ORDER:
        if name not in binaries or LAUNCH_ALIAS.get(name, name) != "ghostty":
            continue
        if "scrollback-limit-lines" not in ghostty_config_keys(binaries[name]):
            out.append(f"{name}: byte-capped only, ran at its default")
    return out


def build_launch(
    name: str, binary: str, pres: Presentation, caps: frozenset[str] | None = None
) -> Launch:
    """The pinned argv for one terminal.

    Pure — apart from the one ghostty probe, which `caps` lets a test
    supply — so `crossterm_test.py` can assert the whole pinned
    condition without launching a window, which is the half of the
    guarantee that a live check cannot cover.
    """
    grid = pres.rows is not None and pres.cols is not None
    name = LAUNCH_ALIAS.get(name, name)
    if name == "kitty":
        # --config NONE is kitty's pristine-config switch; the window
        # size flags take a `c` suffix to mean cells rather than pixels.
        argv = [
            binary,
            "--config",
            "NONE",
            "-o",
            "macos_quit_when_last_window_closed=yes",
            "-o",
            "remember_window_size=no",
            "-o",
            f"font_family={pres.family}",
            "-o",
            f"font_size={pres.pt_str}",
            "-o",
            f"scrollback_lines={pres.scrollback}",
            "-o",
            # 0 disables; kitty's default (-1) defers to the platform,
            # which on macOS means blinking.
            "cursor_blink_interval=0",
        ]
        if grid:
            argv += [
                "-o",
                f"initial_window_width={pres.cols}c",
                "-o",
                f"initial_window_height={pres.rows}c",
            ]
        return Launch(argv)

    if name == "alacritty":
        # alacritty has no "ignore my config" flag, so an empty file
        # stands in for one; without it `-o` only layers on top of
        # ~/.config/alacritty/alacritty.toml.
        cfg = write_temp(".toml", "")
        argv = [
            binary,
            "--config-file",
            str(cfg),
            "-o",
            f'font.normal.family="{pres.family}"',
            "-o",
            f"font.size={pres.pt_str}",
            "-o",
            f"scrolling.history={pres.scrollback}",
            "-o",
            'cursor.style.blinking="Never"',
        ]
        if grid:
            argv += [
                "-o",
                f"window.dimensions.columns={pres.cols}",
                "-o",
                f"window.dimensions.lines={pres.rows}",
            ]
        return Launch([*argv, "-e"], tmp=[cfg])

    if name == "wezterm":
        lines = [
            # The chunk is evaluated with `require` available but
            # `wezterm` not yet in scope, so it pulls the module in.
            "local wezterm = require 'wezterm'",
            "return {",
            f"  font = wezterm.font('{pres.family}'),",
            f"  font_size = {pres.pt:.1f},",
            f"  scrollback_lines = {pres.scrollback},",
            "  default_cursor_style = 'SteadyBlock',",
            "  cursor_blink_rate = 0,",
        ]
        if grid:
            lines += [
                f"  initial_cols = {pres.cols},",
                f"  initial_rows = {pres.rows},",
            ]
        lines += [
            "  enable_tab_bar = false,",
            "  check_for_updates = false,",
            "  quit_when_all_windows_are_closed = true,",
            "}",
        ]
        cfg = write_temp(".lua", "\n".join(lines) + "\n")
        return Launch(
            [binary, "start", "--always-new-process", "--"],
            env={"WEZTERM_CONFIG_FILE": str(cfg)},
            tmp=[cfg],
        )

    if name == "ghostty":
        # config-default-files=false is ghostty's pristine-config switch
        # (`+show-config --default` lists it); window-save-state=never
        # stops a restored window from re-running the wrapper.
        argv = [
            binary,
            "--config-default-files=false",
            f"--font-family={pres.family}",
            f"--font-size={pres.pt_str}",
            "--window-save-state=never",
            "--quit-after-last-window-closed=true",
            "--confirm-close-surface=false",
            "--cursor-style-blink=false",
        ]
        if caps is None:
            caps = ghostty_config_keys(binary)
        # Only tip has a line-based cap; on 1.3.1 the byte limit is the
        # only lever and no honest number of bytes maps to 10_000 rows,
        # so that build keeps its default and `unpinned_scrollback`
        # says so on the chart rather than the pin quietly missing.
        if "scrollback-limit-lines" in caps:
            argv.append(f"--scrollback-limit-lines={pres.scrollback}")
        if grid:
            argv += [f"--window-width={pres.cols}", f"--window-height={pres.rows}"]
        return Launch([*argv, "-e"])

    if name == "foot":
        # foot's font flag is fontconfig syntax, not a bare point size,
        # and it has no config-less flag — `-c /dev/null` is what keeps
        # a developer's foot.ini out of the run. -W is cells; -w is
        # pixels, and only the former is comparable.
        argv = [
            binary,
            "-c",
            "/dev/null",
            "-f",
            f"{pres.family}:size={pres.pt_str}",
            # foot retains 1000 lines by default — an order of magnitude
            # shallower than the rest of the field, and the shallowest
            # cap is the one that decides a flooded-RSS comparison.
            "-o",
            f"scrollback.lines={pres.scrollback}",
            "-o",
            "cursor.blink=no",
        ]
        if grid:
            argv += ["-W", f"{pres.cols}x{pres.rows}"]
        return Launch(argv)

    raise KeyError(f"no launch defined for {name!r}")


def wrapper_script(body: list[str], settle: float = 1.5) -> Path:
    """The script a terminal runs as its shell.

    Stays a shell script because that is what a PTY's `$SHELL` has to
    be; everything driving it from outside is Python.
    """
    lines = ["#!/usr/bin/env bash", f"sleep {settle}", *body]
    path = write_temp(".sh", "\n".join(lines) + "\n")
    path.chmod(0o755)
    return path


# TIOCGWINSZ through python, because `stty size` reports cells only and
# bash cannot read the struct. `/dev/tty`, not stdout: the caller
# redirects stdout into the `.size` file, and the ioctl on a regular
# file answers ENOTTY.
WINSIZE_READER = (
    "python3 -c 'import fcntl,struct,termios;"
    'f=open("/dev/tty");'
    'w=struct.unpack("HHHH",fcntl.ioctl(f.fileno(),termios.TIOCGWINSZ,b"\\0"*8));'
    "print(w[0],w[1],w[2],w[3])'"
)


def pin_handshake_body(
    size_path: Path, pinned_path: Path, stty_path: Path
) -> list[str]:
    """The wrapper's half of the pin, run before the workload starts.

    Two phases, because a single reading cannot serve both parties: the
    pin needs the geometry *while* it is placing the window, and the
    workload needs it settled. So the loop publishes `.size` for the
    driver to read, waits for the driver's `.pinned`, applies a tty pin
    if one was left, and only then returns to the workload — which
    therefore never starts mid-resize.

    Written through a temporary and renamed: the driver polls this file
    four times a second, and a half-written line would read as a grid
    nobody ran at.
    """
    size, pinned, stty = (
        shlex.quote(str(p)) for p in (size_path, pinned_path, stty_path)
    )
    record = f"{WINSIZE_READER} > {size}.tmp && mv {size}.tmp {size}"
    return [
        # Bounded so a driver that died does not leave a window looping
        # forever; the pin touches `.pinned` even when it gives up, so
        # reaching the bound means nobody is driving this leg any more.
        "for i in $(seq 600); do",
        f"  {record}",
        f"  if [ -e {pinned} ]; then break; fi",
        "  sleep 0.25",
        "done",
        f"if [ -f {stty} ]; then",
        f"  read -r r c < {stty}",
        '  stty rows "$r" cols "$c"',
        f"  {record}",
        "fi",
    ]


@dataclass(frozen=True)
class Size:
    """One leg's tty geometry: cells and the pixels they were drawn in.

    `xpixel`/`ypixel` are 0 for a terminal that does not fill
    TIOCGWINSZ's pixel fields; that leg is verified by cells alone and
    the chart says so, rather than being called a mismatch.
    """

    rows: int
    cols: int
    xpixel: int = 0
    ypixel: int = 0

    @property
    def has_pixels(self) -> bool:
        return self.xpixel > 0 and self.ypixel > 0

    def cells(self) -> tuple[int, int]:
        return self.rows, self.cols

    def text(self) -> str:
        return f"{self.rows} {self.cols} {self.xpixel} {self.ypixel}\n"


def read_size(path: Path) -> Size | None:
    """`"56 212 1696 1344"` → a Size; a two-field file still parses.

    Two fields are what `stty size` wrote before the pixel columns
    existed, and a results root taken then still has to render.
    """
    try:
        parts = path.read_text().split()
    except OSError:
        return None
    if len(parts) not in (2, 4):
        return None
    try:
        values = [int(p) for p in parts]
    except ValueError:
        return None
    return Size(*values)


def read_grid(path: Path) -> tuple[int, int] | None:
    """The cell half of a `.size` file, for the callers that pin by cells."""
    size = read_size(path)
    return None if size is None else size.cells()


def pixel_tolerance(pres: Presentation) -> tuple[float, float]:
    """One reference cell per axis.

    Padding is sub-cell by construction — a terminal cannot draw half a
    column of it and still fit the grid — while a font pin that did not
    apply moves the window by at least a cell.
    """
    if not (pres.rows and pres.cols and pres.xpixel and pres.ypixel):
        return 0.0, 0.0
    return pres.xpixel / pres.cols, pres.ypixel / pres.rows


def verify_grid(name: str, got: Size | None, pres: Presentation) -> str | None:
    """The grid a terminal got against the one it was told to take.

    Cells first, then width and height *separately*: one area compared
    against another accepts a wide-short window as a narrow-tall one,
    and those are not the same picture.

    A mismatch is not fatal — the run still produces numbers — but it
    invalidates the comparison, so it is returned to be recorded rather
    than only printed.
    """
    if got is None or pres.rows is None:
        return None
    if got.cells() != (pres.rows, pres.cols):
        return (
            f"{name} ran at {got.cols}x{got.rows}, not the pinned "
            f"{pres.cols}x{pres.rows}"
        )
    wide, high = pixel_tolerance(pres)
    if not (wide and high and got.has_pixels):
        return None
    if abs(got.xpixel - pres.xpixel) > wide or abs(got.ypixel - pres.ypixel) > high:
        return (
            f"{name} held {got.cols}x{got.rows} cells but drew them in "
            f"{got.xpixel}x{got.ypixel} px, not the reference "
            f"{pres.xpixel}x{pres.ypixel}"
        )
    return None


def frontmost_pid() -> int | None:
    """The pid of the application the keyboard is currently talking to.

    `lsappinfo`, not System Events: reading the process list through
    AppleScript needs an Automation grant the developer has to click
    through, and this is a plain LaunchServices query that needs none.

    Only the latency suite asks, and only to refuse: keystrokes go
    wherever focus is, so a leg whose window did not come to the front
    has to be skipped rather than typed into.
    """
    asn = subprocess.run(
        ["lsappinfo", "front"], capture_output=True, text=True
    ).stdout.strip()
    if not asn:
        return None
    out = subprocess.run(
        ["lsappinfo", "info", "-only", "pid", asn], capture_output=True, text=True
    ).stdout
    # `"pid"=2012`
    match = re.search(r'"pid"\s*=\s*(\d+)', out)
    return int(match.group(1)) if match else None


def raise_window(pid: int, timeout: float = 15.0) -> bool:
    """Bring the window `pid` opened to the front, or say it could not.

    Two terminals in the field need this and are unmeasurable without
    it. ghostty 1.3.1 does not open its window *at all* until the
    application is activated — the launch sits there with no window, the
    wrapper never runs, and the leg burns its whole timeout; measured,
    that was every suite, not only the one that types. wezterm opens one
    but leaves it behind everything else, because a binary started
    outside LaunchServices does not take focus on macOS. kitty,
    alacritty, ghostty tip and felis come forward on their own, for
    which this returns at once.

    System Events rather than `lsappinfo setfront`, which refuses with
    -54 (permErr) for a caller that is not already frontmost. That
    trades the permission-free query for an Automation grant, which the
    suite's documentation lists beside the other two. Retried while the
    launch registers with LaunchServices: an activation aimed at a
    process it has not seen yet fails with -1719.

    Neither reason exists off macOS: a Wayland compositor maps and
    focuses a new toplevel on its own, and both terminals that need the
    raise need it for a LaunchServices behavior. Answering yes there is
    not a stub — there is nothing left to do.
    """
    if not DARWIN:
        return True
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        tree = process_tree(pid)
        # The common case costs nothing: kitty, alacritty and felis take
        # the front by themselves, and every leg calls this.
        if frontmost_pid() in tree:
            return True
        for candidate in sorted(tree):
            script = (
                'tell application "System Events" to set frontmost of '
                f"(first process whose unix id is {candidate}) to true"
            )
            done = subprocess.run(
                ["osascript", "-e", script], capture_output=True, text=True
            )
            # A candidate with no user interface — the shell, `cat` —
            # answers -1719; only the one that took it is worth reading
            # back, and not before the switch has landed.
            if done.returncode != 0:
                continue
            time.sleep(0.5)
            if frontmost_pid() in tree:
                return True
        time.sleep(0.5)
    return False


def keep_display_awake() -> subprocess.Popen | None:
    """Hold the display on for as long as the caller runs.

    Every suite here drives real windows, so a display that parked is a
    terminal drawing to nothing and a number that measures the parking.
    The suites ask for a desktop nobody touches and a run takes over an
    hour, which is how an untouched machine reaches its display-sleep
    timer and then its lock screen — after which the windowed suites are
    measuring the wrong thing and the latency suite cannot run at all.

    macOS: `-u` also wakes a display that has already gone; `-w` is the
    backstop, exiting with the caller even if nothing terminates it.

    Linux: logind's inhibitor, with a poll on the caller's pid for the
    same backstop. It reaches whatever consults logind — the idle action
    itself, swayidle, an ordinary desktop's screensaver — and does not
    reach a compositor that blanks on a timer of its own, which has to
    be turned off by hand. None when there is no logind to ask.
    """
    if DARWIN:
        return subprocess.Popen(
            ["caffeinate", "-d", "-i", "-u", "-w", str(os.getpid())],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    if not shutil.which("systemd-inhibit"):
        return None
    return subprocess.Popen(
        [
            "systemd-inhibit",
            "--what=idle:sleep",
            "--who=felis cross-terminal bench",
            "--why=driving real terminal windows",
            "sh",
            "-c",
            f"while kill -0 {os.getpid()} 2>/dev/null; do sleep 5; done",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def screen_locked() -> bool:
    """Is the lock screen holding the display?

    Named rather than left to read as "the window would not come to the
    front", because it fails every leg the same way and the fix is a
    password, not a re-run. A Mac left alone for its own good — which is
    what this suite asks for — locks itself on the display-sleep timer.
    """
    pid = frontmost_pid()
    if pid is None:
        return False
    comm = subprocess.run(
        [PS, "-o", "comm=", "-p", str(pid)], capture_output=True, text=True
    ).stdout
    return "loginwindow" in comm


def process_tree(pid: int) -> set[int]:
    """`pid` and every process under it.

    The window is not always the process that was launched: wezterm's
    CLI spawns wezterm-gui, so matching on the launch pid alone would
    read as "not frontmost" for a window that is.
    """
    seen: set[int] = set()
    pending = [pid]
    while pending:
        current = pending.pop()
        if current in seen:
            continue
        seen.add(current)
        found = subprocess.run(
            ["pgrep", "-P", str(current)], capture_output=True, text=True
        )
        pending += [int(p) for p in found.stdout.split()]
    return seen


def wait_for(path: Path, timeout: float, alive: subprocess.Popen | None = None) -> bool:
    """Poll for a marker file, giving up if its producer dies first."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return True
        if alive is not None and alive.poll() is not None:
            return path.exists()
        time.sleep(0.5)
    return path.exists()


def write_mismatch(results: Path, note: str) -> None:
    """Append one note to the suite's `grid-mismatch`, once.

    As it happens, not when the suite returns: a run killed mid-suite
    would otherwise leave `.size` files that agree in cells and no
    record that the window behind one of them was never placed, and the
    report would call that leg pinned.
    """
    path = results / "grid-mismatch"
    written = path.read_text().splitlines() if path.exists() else []
    if note not in written:
        path.write_text("\n".join([*written, note]) + "\n")


def record_mismatch(field: Field, results: Path, note: str) -> None:
    """Say the field was not uniform, in the run's own copy and on disk."""
    field.mismatches.append(note)
    write_mismatch(results, note)


def rotate(legs: list[str], by: int) -> list[str]:
    """The same sequence started `by` entries along, so no leg keeps a slot.

    A rotation rather than a seeded shuffle: with a handful of terminals and
    three rounds a shuffle can leave one of them in the same position
    every time, and the position is what this is correcting for —
    whatever drifts over a run (thermal, page cache, compositor state)
    otherwise lands on the same terminals it landed on last time.
    """
    if not legs:
        return []
    step = by % len(legs)
    return legs[step:] + legs[:step]


def launch_terminal(launch: Launch, wrapper: Path) -> subprocess.Popen:
    return subprocess.Popen(
        launch.command(wrapper),
        env={**os.environ, **launch.env},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


@dataclass
class Leg:
    """One open terminal window, pinned and ready to be measured."""

    name: str
    proc: subprocess.Popen
    size_path: Path
    outcome: object
    # Present for felis's leg only: the private daemon and config home
    # the rest of the field has no equivalent of.
    felis: FelisRun | None = None
    # Set once the leg has ended, if the display could no longer present.
    refused: str | None = None

    @property
    def pid(self) -> int:
        return self.proc.pid


@dataclass
class Field:
    """The resolved binaries plus the condition they all run under."""

    binaries: dict[str, str]  # terminal name → path; missing = not in the field
    felis_bin: str
    pres: Presentation
    mismatches: list[str] = dc_field(default_factory=list)
    # The order this round runs its legs in, or None for chart order.
    leg_order: list[str] | None = None
    # The desktop's window pin (`wm.select()`); imported late because
    # the pins read this module.
    pin: object = None

    def __post_init__(self) -> None:
        if self.pin is None:
            import wm

            self.pin = wm.FloatingPin()

    def names(self) -> list[str]:
        return [n for n in FIELD_ORDER if n in self.binaries]

    def legs(self) -> list[str]:
        """Every leg in chart order, felis among them rather than beside them.

        One ordered sequence is what lets a suite iterate instead of
        running felis first and the rest afterwards — and what a caller
        that wants to rotate the order has to rotate.
        """
        return list(self.leg_order) if self.leg_order else ["felis", *self.names()]

    def launch(self, name: str) -> Launch | None:
        binary = self.binaries.get(name)
        return build_launch(name, binary, self.pres) if binary else None

    def has_leg(self, name: str) -> bool:
        return name == "felis" or name in self.binaries

    def check(self, name: str, size_path: Path, results: Path) -> Size | None:
        got = read_size(size_path)
        if problem := verify_grid(name, got, self.pres):
            print(f"  GRID MISMATCH: {problem}", file=sys.stderr)
            record_mismatch(self, results, problem)
        return got

    # ── felis ────────────────────────────────────────────────────────

    def start_felis(self, wrapper: Path) -> FelisRun:
        return FelisRun.start(self.felis_bin, wrapper, self.pres)


@contextlib.contextmanager
def open_leg(
    field: Field,
    tried: Attempt,
    body: list[str],
    settle: float = 1.5,
    discover: bool = False,
) -> Iterator[Leg]:
    """Launch one leg, raise it, pin it, and hand it over ready to measure.

    The one place a window is opened, so the pin exists once rather than
    at every suite's own launch site — and `"felis"` is an entry here
    like any other terminal, which is what lets a suite iterate over
    `field.legs()` instead of running felis first by hand.

    Every file the leg writes lands in the attempt's own directory, so a
    `.pinned`, `.size` or phase marker from an earlier attempt cannot
    release, re-pin or mislead this one.
    """
    import wm

    name = tried.name
    size_path = tried.path("size")
    pinned_path = tried.path("pinned")
    stty_path = tried.path("stty")

    env_before = envinfo.leg_env()
    if reason := envinfo.cannot_present(env_before):
        refuse(tried.dir, name, {"before": env_before}, f"{reason} before the leg")
        raise DisplayUnavailable(reason)
    wrapper = wrapper_script(
        pin_handshake_body(size_path, pinned_path, stty_path) + body, settle=settle
    )
    launch = None
    run = None
    leg = None
    print(f"running {name}...", flush=True)
    if name == "felis":
        run = field.start_felis(wrapper)
        proc = run.proc
    else:
        launch = field.launch(name)
        if launch is None:
            raise KeyError(f"{name} is not in the field")
        proc = launch_terminal(launch, wrapper)
    try:
        # Before the pin and before any wait: ghostty 1.3.1 opens no
        # window at all until its application is activated, so there
        # would be nothing to pin and nothing to mark.
        raise_window(proc.pid)
        target = wm.Target(
            size=size_path,
            stty=stty_path,
            rows=None if discover else field.pres.rows,
            cols=None if discover else field.pres.cols,
            xpixel=None if discover else field.pres.xpixel,
            ypixel=None if discover else field.pres.ypixel,
        )
        outcome = field.pin.pin(proc.pid, target)
        if outcome.status != wm.PINNED:
            said = (
                "tty pinned, window unpinned"
                if outcome.status == wm.TTY_ONLY
                else "unpinned"
            )
            print(f"  PIN: {name} {said} ({outcome.detail})", file=sys.stderr)
            record_mismatch(field, tried.results, f"{name}: {said} ({outcome.detail})")
        # The driver releases the workload, whatever the pin decided: a
        # leg that could not be placed still runs, and the chart says
        # under what.
        pinned_path.touch()
        if run is not None and (problem := run.font_fallback()):
            print(f"  FONT MISMATCH: {problem}", file=sys.stderr)
            record_mismatch(field, tried.results, problem)
        leg = Leg(name, proc, size_path, outcome, run)
        yield leg
    finally:
        # Both ends, because a throttle counter says nothing until it is
        # compared with where it started.
        record = {"before": env_before, "after": envinfo.leg_env()}
        if reason := envinfo.cannot_present(record["after"]):
            reason = f"{reason} when the leg ended"
            if leg is not None:
                leg.refused = reason
            refuse(tried.dir, name, record, reason)
        else:
            tried.path("env.json").write_text(json.dumps(record, indent=2) + "\n")
        tried.seal()
        if run is not None:
            lingering = run.stop()
        else:
            lingering = stop([Held.child(proc)])
        if lingering:
            print(
                f"  LINGERING: {name} left {lingering} running after SIGKILL",
                file=sys.stderr,
            )
        wrapper.unlink(missing_ok=True)
        for path in launch.tmp if launch else []:
            path.unlink(missing_ok=True)


class DisplayUnavailable(RuntimeError):
    """The display could not present a frame, so the leg was not launched."""


REFUSED_DIR = "refused"


def refuse(directory: Path, name: str, record: dict, reason: str) -> None:
    """Say why a leg is not a measurement, in the console and its env record."""
    print(
        f"  REFUSED: {name} — {reason}, so no frame could be presented. "
        "Unlock the screen, wake the display, and re-run.",
        file=sys.stderr,
    )
    (directory / f"{name}.env.json").write_text(
        json.dumps({**record, "refused": reason}, indent=2) + "\n"
    )


# ── leg attempts ─────────────────────────────────────────────────────

ATTEMPTS_DIR = ".attempts"
PROMOTING = ".promoting"
UNFINISHED_DIR = "unfinished"
ASIDE_MARK = ".aside"
# Appended by the harness itself across a leg's attempts, never by the
# leg's processes, so it is the leg's history rather than one attempt's.
ACROSS_ATTEMPTS = (".latency-failed",)


@dataclass
class Attempt:
    """One try at a leg: the directory it writes, and the one it lands in.

    `dir` is the one the leg's processes are given. Once the harness has
    stopped listening to them (`seal`), `path` names a copy only the
    harness writes, and that copy is what lands.
    """

    results: Path
    name: str
    dir: Path
    sealed: Path | None = None

    def path(self, suffix: str) -> Path:
        return (self.sealed or self.dir) / f"{self.name}.{suffix}"

    def seal(self) -> None:
        """Take what the leg's processes wrote as the attempt's output.

        A copy, so neither a write through a path into `dir` nor one
        through a file a surviving process still holds open can change
        it. Taken before the leg is stopped rather than after: a process
        the stop does not reach can write at any moment afterwards.
        """
        if self.sealed is not None:
            return
        holding = self.results / ATTEMPTS_DIR
        holding.mkdir(parents=True, exist_ok=True)
        sealed = Path(tempfile.mkdtemp(prefix=".sealed.", dir=holding))
        for path in self.dir.iterdir():
            if path.is_file():
                shutil.copyfile(path, sealed / path.name)
        self.sealed = sealed


def leg_stems(name: str) -> list[str]:
    """Every artifact stem a leg writes: startup splits felis into two rows."""
    return [name, "felis-cold", "felis-warm"] if name == "felis" else [name]


@contextlib.contextmanager
def attempt(results: Path, name: str, marker: str = "done") -> Iterator[Attempt]:
    """A fresh directory for one try at a leg, promoted if the try returns.

    Everything the leg writes, the harness's own records and the
    workload's output alike, goes into `.attempts/<name>.*` rather than
    into `results`, which only a completed attempt reaches (`promote`).
    An attempt that raised or was interrupted is dropped, so what the
    report reads is always one attempt's whole output, and a process
    that outlives its attempt writes into a directory nobody will read.

    Whatever an earlier attempt left in `results` goes first: the leg is
    being taken again, so it no longer describes the leg even if this
    attempt ends before it can replace it. `marker` is the file whose
    presence says the leg finished, the one a resumed run skips it by.
    """
    holding = results / ATTEMPTS_DIR
    holding.mkdir(parents=True, exist_ok=True)
    if (pending := holding / f"{name}{PROMOTING}").is_dir():
        finish_promotion(results, pending)
    for stem in leg_stems(name):
        for path in results.glob(f"{glob.escape(stem)}.*"):
            if path.is_file() and not path.name.endswith(ACROSS_ATTEMPTS):
                path.unlink()
    tried = Attempt(
        results, name, Path(tempfile.mkdtemp(prefix=f"{name}.", dir=holding))
    )
    try:
        yield tried
    except BaseException:
        shutil.rmtree(tried.dir, ignore_errors=True)
        if tried.sealed is not None:
            shutil.rmtree(tried.sealed, ignore_errors=True)
        raise
    promote(tried, marker)


def promote(tried: Attempt, marker: str = "done") -> None:
    """Land a completed attempt in the suite directory.

    What lands is the sealed copy (`Attempt.seal`), renamed into place
    as one directory, which is the commit; `finish_promotion` moves the
    files out of it, the resume marker last. An attempt that was refused, or that returned without
    its `marker`, lands only its env record; the rest goes to
    `refused/` or `unfinished/`, kept for diagnosis and out of the
    report's reach, since a workload stopped partway can already have
    written a number.
    """
    holding = tried.results / ATTEMPTS_DIR
    tried.seal()
    staging = tried.sealed
    try:
        refused = json.loads(tried.path("env.json").read_text()).get("refused")
    except (OSError, json.JSONDecodeError, AttributeError):
        refused = None
    if refused:
        (staging / ASIDE_MARK).write_text(REFUSED_DIR)
    elif not tried.path(marker).exists():
        (staging / ASIDE_MARK).write_text(UNFINISHED_DIR)
    staging.rename(holding / f"{tried.name}{PROMOTING}")
    shutil.rmtree(tried.dir, ignore_errors=True)
    finish_promotion(tried.results, holding / f"{tried.name}{PROMOTING}")


def finish_promotion(results: Path, staged: Path) -> None:
    """Move a committed attempt's files into `results`; safe to repeat.

    Where the set-aside files go is read from a mark the staging wrote
    rather than from the env record, which a promotion cut short may
    already have moved.
    """
    name = staged.name.removesuffix(PROMOTING)
    try:
        aside = (staged / ASIDE_MARK).read_text()
    except FileNotFoundError:
        aside = None
    files = sorted(
        (path for path in staged.iterdir() if path.name != ASIDE_MARK),
        key=lambda path: path.name.endswith(".done"),
    )
    for path in files:
        if aside and path.name != f"{name}.env.json":
            (results / aside).mkdir(exist_ok=True)
            destination = results / aside / path.name
        else:
            destination = results / path.name
        with contextlib.suppress(FileNotFoundError):
            path.replace(destination)
    shutil.rmtree(staged, ignore_errors=True)


def finish_promotions(results: Path) -> None:
    """Complete every promotion an interrupted run committed but did not finish."""
    for staged in sorted((results / ATTEMPTS_DIR).glob(f"*{PROMOTING}")):
        finish_promotion(results, staged)


def clear_attempts(results: Path) -> None:
    """Before a suite starts: finish committed promotions, drop the rest."""
    finish_promotions(results)
    shutil.rmtree(results / ATTEMPTS_DIR, ignore_errors=True)


# ── stopping a leg's processes ───────────────────────────────────────


@dataclass(eq=False)
class Held:
    """A process held by its identity rather than its pid.

    A pid is reused once its process is reaped, so a signal sent to a
    pid read earlier can reach an unrelated process. The harness's own
    child is held through its `Popen`, which is not reaped behind its
    back; any other process through a pidfd on Linux, and on macOS,
    which has none, through its start time, read again before each
    signal. That leaves macOS the instant between the read and the
    signal, and its pids are handed out in sequence, so a reuse inside
    it needs the whole pid space to wrap in that instant.
    """

    pid: int
    proc: subprocess.Popen | None = None
    fd: int | None = None
    start: int | None = None

    @classmethod
    def child(cls, proc: subprocess.Popen) -> Held:
        return cls(proc.pid, proc=proc)

    def alive(self) -> bool:
        if self.proc is not None:
            return self.proc.poll() is None
        if self.fd is not None:
            return not select.select([self.fd], [], [], 0)[0]
        return self.start is not None and start_time(self.pid) == self.start

    def send(self, sig: int) -> None:
        with contextlib.suppress(ProcessLookupError, PermissionError):
            if self.proc is not None:
                if self.proc.poll() is None:
                    self.proc.send_signal(sig)
            elif self.fd is not None:
                signal.pidfd_send_signal(self.fd, sig)
            elif self.alive():
                os.kill(self.pid, sig)

    def close(self) -> None:
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None
        self.start = None


def start_time(pid: int) -> int | None:
    """macOS: when `pid` started, in Mach ticks; None once it is gone."""
    info = darwin_rusage(pid)
    return None if info is None else info.ri_proc_start_abstime


def hold(pid: int, belongs: Callable[[], bool] = lambda: True) -> Held | None:
    """Hold `pid` if `belongs()` says it is the process meant.

    `belongs` runs after the identity is taken and before it is checked
    again, so a process that was still alive at the second check is the
    one `belongs` looked at.
    """
    if DARWIN:
        start = start_time(pid)
        if start is None or not belongs() or start_time(pid) != start:
            return None
        return Held(pid, start=start)
    try:
        fd = os.pidfd_open(pid)
    except OSError:
        return None
    held = Held(pid, fd=fd)
    if belongs() and held.alive():
        return held
    held.close()
    return None


def parent_pid(pid: int, proc: Path = Path("/proc")) -> int | None:
    if DARWIN:
        out = subprocess.run(
            [PS, "-o", "ppid=", "-p", str(pid)], capture_output=True, text=True
        ).stdout.strip()
        return int(out) if out.isdigit() else None
    try:
        stat = (proc / str(pid) / "stat").read_text()
    except OSError:
        return None
    fields = stat[stat.rfind(")") + 1 :].split()
    return int(fields[1]) if len(fields) > 1 and fields[1].isdigit() else None


def children_of(pid: int, proc: Path = Path("/proc")) -> list[int]:
    """`pid`'s children: `/proc` on Linux, which every thread lists its own in."""
    if DARWIN:
        found = subprocess.run(
            ["pgrep", "-P", str(pid)], capture_output=True, text=True
        )
        return [int(p) for p in found.stdout.split()]
    out: set[int] = set()
    for task in (proc / str(pid) / "task").glob("*"):
        with contextlib.suppress(OSError):
            out.update(int(p) for p in (task / "children").read_text().split())
    return sorted(out)


def held_children(parent: Held, known: Collection[int] = ()) -> list[Held]:
    """`parent`'s children but `known`, each confirmed to be its child once held."""
    out = []
    for pid in children_of(parent.pid):
        if pid in known:
            continue
        held = hold(
            pid, lambda pid=pid: parent_pid(pid) == parent.pid and parent.alive()
        )
        if held is not None:
            out.append(held)
    return out


def stop(
    roots: Sequence[Held],
    spare: Collection[int] = (),
    grace: float = 5.0,
    escalate: bool = True,
) -> list[int]:
    """Terminate `roots` and everything under them but `spare`; what outlived it.

    The tree is walked before the first signal, because a child whose
    parent has died is reparented and no longer found under it, and
    again while waiting, so a child forked after the first walk is
    signalled too. A process that ignores SIGTERM for `grace` seconds
    gets SIGKILL, unless `escalate` is off. Every signal goes through a
    `Held` identity. A process that was reparented before the first
    walk is out of reach; the attempt directory, not this, is what keeps
    such a process's writes out of the report.
    """
    held = list(roots)

    def walk() -> list[Held]:
        fresh = []
        pending = [h for h in held if h.alive()]
        while pending:
            live = {h.pid for h in held if h.alive()}
            for child in held_children(pending.pop(), live):
                held.append(child)
                fresh.append(child)
                pending.append(child)
        return fresh

    def targets(among: list[Held]) -> list[Held]:
        return [h for h in among if h.pid not in spare]

    def wait(sig: int, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while True:
            for h in targets(walk()):
                h.send(sig)
            if not any(h.alive() for h in targets(held)):
                return True
            if time.monotonic() >= deadline:
                return False
            time.sleep(0.05)

    try:
        walk()
        for h in targets(held):
            h.send(signal.SIGTERM)
        if not wait(signal.SIGTERM, grace) and escalate:
            for h in targets(held):
                h.send(signal.SIGKILL)
            wait(signal.SIGKILL, 1.0)
        return sorted(h.pid for h in targets(held) if h.alive())
    finally:
        for h in held:
            h.close()


def hold_felis_daemon(socket: Path) -> Held | None:
    """The daemon bound to `socket`, held so a later signal cannot miss it."""
    pid = felis_daemon_pid(socket)
    if pid is None:
        return None
    return hold(pid, lambda: felis_daemon_pid(socket) == pid)


def felis_home(pres: Presentation) -> tuple[Path, Path]:
    """A throwaway HOME holding the pinned config, and a directory for the socket."""
    home = Path(tempfile.mkdtemp(prefix="felis-bench-home."))
    for rel in ("Library/Application Support/felis", ".config/felis"):
        cfg = home / rel
        cfg.mkdir(parents=True, exist_ok=True)
        (cfg / "config.toml").write_text(pres.felis_config())
    return home, Path(tempfile.mkdtemp(prefix="felis-bench."))


def felis_daemon_pid(socket: Path) -> int | None:
    """The daemon bound to `socket`, matched by pid.

    Never `pkill -f <socket>`: that also matches the driver's own
    command line.
    """
    found = subprocess.run(
        ["pgrep", "-x", "felis-daemon"], capture_output=True, text=True
    )
    for pid in found.stdout.split():
        cmd = subprocess.run(
            [PS, "-o", "command=", "-p", pid], capture_output=True, text=True
        )
        if str(socket) in cmd.stdout:
            return int(pid)
    return None


@dataclass
class FelisRun:
    """A felis client on a private socket, with a pinned config."""

    proc: subprocess.Popen
    home: Path
    sockdir: Path
    log: Path

    @property
    def socket(self) -> Path:
        return self.sockdir / "daemon.sock"

    @classmethod
    def start(cls, felis_bin: str, wrapper: Path, pres: Presentation) -> FelisRun:
        home, sockdir = felis_home(pres)
        log = sockdir / "felis.log"
        env = {
            **os.environ,
            # felis resolves its config through `directories::ProjectDirs`,
            # which has no env override on macOS (it is
            # `$HOME/Library/Application Support`), so redirecting HOME is
            # what keeps the developer's own config out of a measurement.
            # XDG_CONFIG_HOME covers the Linux path.
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "RUST_LOG": "warn",
            "SHELL": str(wrapper),
        }
        sink = log.open("w")
        proc = subprocess.Popen(
            [felis_bin, "--socket", str(sockdir / "daemon.sock")],
            env=env,
            # stdout as well as stderr: the client's tracing subscriber
            # writes to STDOUT (felis-client main.rs), so discarding it
            # would throw away the one warning that says the pinned font
            # did not load. The window's own output goes through the PTY,
            # never through here.
            stdout=sink,
            stderr=subprocess.STDOUT,
        )
        return cls(proc, home, sockdir, log)

    def daemon_pid(self) -> int | None:
        return felis_daemon_pid(self.socket)

    def font_fallback(self) -> str | None:
        """felis's warning that the pinned family was not installed.

        A run where only felis fell back to another font is not a
        comparison, and nothing else would reveal it.
        """
        if not self.log.exists():
            return None
        if "font family not found" in self.log.read_text(errors="replace"):
            return (
                "felis could not load the pinned font family; it fell back to monospace"
            )
        return None

    def stop_client(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()

    def stop(self) -> list[int]:
        """Stop the client, the daemon and the daemon's sessions; what outlived it."""
        roots = [Held.child(self.proc)]
        if daemon := hold_felis_daemon(self.socket):
            roots.append(daemon)
        lingering = stop(roots)
        shutil.rmtree(self.sockdir, ignore_errors=True)
        shutil.rmtree(self.home, ignore_errors=True)
        return lingering


# ── memory sampling ──────────────────────────────────────────────────


@dataclass(frozen=True)
class MemorySample:
    """What one leg holds, in KB, and which accounting says so.

    The metric travels with the number because "memory" is not one
    quantity: the chart has to say which one it drew, and two runs taken
    under different ones do not belong on the same axis.
    """

    metric: str
    kb: int
    # The high-water mark, where the OS keeps one that means the same
    # thing. macOS keeps only a lifetime maximum, which is its startup
    # peak (`darwin_sample`), so it is None there.
    peak_kb: int | None


def total(samples: list[MemorySample | None]) -> MemorySample | None:
    """One leg's processes added up, or None if any of them was unreadable.

    Adding is only sound because the metric is proportional: PSS divides
    each shared page among its mappers, so felis's client and daemon
    contribute one copy of what they share between them and not two.
    Under RSS the same sum bills felis twice for libc, the driver and
    the mmap'd font, which reads on the chart as a cost of the split
    rather than an artifact of the counting.

    None if any sample is missing: a leg that lost half of itself
    between the mark and the sample undercounts by exactly the quantity
    the chart is about.
    """
    if not samples or any(s is None for s in samples):
        return None
    peaks = [s.peak_kb for s in samples]
    return MemorySample(
        samples[0].metric,
        sum(s.kb for s in samples),
        # Peaks summed, not the peak of the sum — the OS keeps one per
        # process. That bounds the real peak from above, which is the
        # direction that cannot hide a spike.
        None if any(p is None for p in peaks) else sum(peaks),
    )


class RusageInfoV4(ctypes.Structure):
    """`struct rusage_info_v4` from `sys/resource.h`.

    Each version appends to the one before it, so the v0 fields keep
    their offsets and a v4 buffer answers every question an older
    flavor could.
    """

    _fields_ = [
        ("ri_uuid", ctypes.c_uint8 * 16),
        ("ri_user_time", ctypes.c_uint64),
        ("ri_system_time", ctypes.c_uint64),
        ("ri_pkg_idle_wkups", ctypes.c_uint64),
        ("ri_interrupt_wkups", ctypes.c_uint64),
        ("ri_pageins", ctypes.c_uint64),
        ("ri_wired_size", ctypes.c_uint64),
        ("ri_resident_size", ctypes.c_uint64),
        ("ri_phys_footprint", ctypes.c_uint64),
        ("ri_proc_start_abstime", ctypes.c_uint64),
        ("ri_proc_exit_abstime", ctypes.c_uint64),
        ("ri_child_user_time", ctypes.c_uint64),
        ("ri_child_system_time", ctypes.c_uint64),
        ("ri_child_pkg_idle_wkups", ctypes.c_uint64),
        ("ri_child_interrupt_wkups", ctypes.c_uint64),
        ("ri_child_pageins", ctypes.c_uint64),
        ("ri_child_elapsed_abstime", ctypes.c_uint64),
        ("ri_diskio_bytesread", ctypes.c_uint64),
        ("ri_diskio_byteswritten", ctypes.c_uint64),
        ("ri_cpu_time_qos_default", ctypes.c_uint64),
        ("ri_cpu_time_qos_maintenance", ctypes.c_uint64),
        ("ri_cpu_time_qos_background", ctypes.c_uint64),
        ("ri_cpu_time_qos_utility", ctypes.c_uint64),
        ("ri_cpu_time_qos_legacy", ctypes.c_uint64),
        ("ri_cpu_time_qos_user_initiated", ctypes.c_uint64),
        ("ri_cpu_time_qos_user_interactive", ctypes.c_uint64),
        ("ri_billed_system_time", ctypes.c_uint64),
        ("ri_serviced_system_time", ctypes.c_uint64),
        ("ri_logical_writes", ctypes.c_uint64),
        ("ri_lifetime_max_phys_footprint", ctypes.c_uint64),
        ("ri_instructions", ctypes.c_uint64),
        ("ri_cycles", ctypes.c_uint64),
        ("ri_billed_energy", ctypes.c_uint64),
        ("ri_serviced_energy", ctypes.c_uint64),
        ("ri_interval_max_phys_footprint", ctypes.c_uint64),
        ("ri_runnable_time", ctypes.c_uint64),
    ]


RUSAGE_INFO_V4 = 4


class MachTimebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


def darwin_rusage(pid: int) -> RusageInfoV4 | None:
    """`proc_pid_rusage`, which answers for any process of the same user."""
    info = RusageInfoV4()
    libc = ctypes.CDLL(None)
    if libc.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(info)) != 0:
        return None
    return info


@lru_cache(maxsize=1)
def mach_ns_per_tick() -> float:
    """Nanoseconds per Mach absolute-time tick: 125/3 on Apple Silicon, 1 on Intel.

    `ri_user_time` and `ri_system_time` are counted in these ticks, not
    in nanoseconds, so reading them as nanoseconds undercounts an Apple
    Silicon process's CPU time by a factor of about 42.
    """
    base = MachTimebase()
    ctypes.CDLL(None).mach_timebase_info(ctypes.byref(base))
    return base.numer / base.denom if base.denom else 1.0


def rusage_cpu_seconds(info: RusageInfoV4, ns_per_tick: float) -> float:
    return (info.ri_user_time + info.ri_system_time) * ns_per_tick / 1e9


def darwin_sample(pid: int) -> MemorySample | None:
    """`phys_footprint`: the ledger Activity Monitor shows and jetsam judges by.

    Read through `proc_pid_rusage` rather than `vmmap --summary` or
    `footprint`, which print the same number but fork a tool per
    process — too slow for a sampler that reads a leg four times a
    second.

    Unlike RSS the footprint follows a page the memory compressor took,
    so an idle number stops recording what else the machine was doing.

    No peak: the only one the kernel keeps is the lifetime maximum,
    which on this platform is the 280-440 MB a GUI terminal touches
    while it starts, not anything the workload did.
    """
    info = darwin_rusage(pid)
    if info is None:
        return None
    return MemorySample("phys_footprint", info.ri_phys_footprint // 1024, None)


def proc_kb(text: str, key: str) -> int | None:
    """`Pss:   1234 kB` → 1234. Exact key: `Pss_Anon` is not `Pss`."""
    for line in text.splitlines():
        name, _, rest = line.partition(":")
        if name == key:
            parts = rest.split()
            if parts and parts[0].isdigit():
                return int(parts[0])
    return None


def linux_sample(pid: int, proc: Path = Path("/proc")) -> MemorySample | None:
    """PSS from `smaps_rollup`, plus the peak resident size from `status`.

    `VmHWM` is the high-water mark of the *resident* size — the kernel
    keeps none for PSS — so it is carried separately and never drawn on
    the same axis as the bars.
    """
    try:
        rollup = (proc / str(pid) / "smaps_rollup").read_text()
        status = (proc / str(pid) / "status").read_text()
    except OSError:
        return None
    pss = proc_kb(rollup, "Pss")
    return None if pss is None else MemorySample("pss", pss, proc_kb(status, "VmHWM"))


def memory_sample(pids: list[int]) -> MemorySample | None:
    """What the memory suite plots, summed over a leg's processes.

    Not `ps -o rss`, which counts every shared page in full against
    every process mapping it (see `total`). What replaces it is each
    platform's own accounting, so the number is the one that platform's
    own tools would show for the same process.

    Each number is what the OS charges to the processes the suite
    samples under its ledger: PSS (the process's proportional share of
    its resident pages) on Linux, `phys_footprint` (the process's
    physical-footprint ledger, the number Activity Monitor's Memory
    column shows) on macOS. Whatever the GPU driver allocates and the OS
    charges to the process is inside; whatever it holds elsewhere
    (device memory the process does not map, kernel-side driver state)
    is outside — and how the split falls depends on the API, the driver
    and the GPU, so a GPU terminal's bar carries a driver share the
    reader cannot separate from the rest, while a CPU renderer (foot)
    carries none.
    """
    sampler = darwin_sample if DARWIN else linux_sample
    return total([sampler(pid) for pid in pids])


# ── cpu sampling ─────────────────────────────────────────────────────


def linux_cpu_ticks(pid: int, proc: Path = Path("/proc")) -> int | None:
    """`utime + stime` from `/proc/<pid>/stat`, in `SC_CLK_TCK` ticks.

    Not `ps -o cputime` here: procps prints whole seconds (`00:00:04`),
    and a flood that costs a fast terminal a third of a second then
    charts as zero. The comm field is parenthesised and may itself
    contain spaces and a `)`, so the fields are counted from the last
    one.
    """
    try:
        stat = (proc / str(pid) / "stat").read_text()
    except OSError:
        return None
    fields = stat[stat.rfind(")") + 1 :].split()
    try:
        return int(fields[11]) + int(fields[12])
    except (IndexError, ValueError):
        return None


def linux_cpu_centiseconds(pid: int, proc: Path = Path("/proc")) -> int | None:
    ticks = linux_cpu_ticks(pid, proc)
    return None if ticks is None else ticks * 100 // os.sysconf("SC_CLK_TCK")


def cpu_seconds(pid: int) -> float | None:
    """One process's CPU time so far, at the platform's own resolution.

    macOS reads `proc_pid_rusage` rather than `ps -o cputime`, whose
    10 ms resolution and one fork per pid made a four-per-second
    sampler cost more than some of the workloads it was measuring.
    """
    if DARWIN:
        info = darwin_rusage(pid)
        return None if info is None else rusage_cpu_seconds(info, mach_ns_per_tick())
    ticks = linux_cpu_ticks(pid)
    return None if ticks is None else ticks / os.sysconf("SC_CLK_TCK")


def cpu_centiseconds(pids: list[int]) -> int | None:
    """Centiseconds, not seconds; `None` if any pid's accounting was unreadable.

    The flood costs well under a second of CPU on the fast terminals, so
    whole-second resolution reads as an uninformative 0 everywhere.

    A pid that exited or whose accounting could not be read is not
    folded in as 0: that would undercount by exactly the quantity the
    chart is about, the same reason `memory_sample` fails the leg
    instead of summing around a missing process.
    """
    total = 0
    for pid in pids:
        if DARWIN:
            seconds = cpu_seconds(pid)
            sampled = None if seconds is None else round(seconds * 100)
        else:
            sampled = linux_cpu_centiseconds(pid)
        if sampled is None:
            return None
        total += sampled
    return total


# Processes a terminal spawns that are part of its footprint although
# they do not carry its name. kitty runs `kitten __atexit__` beside every
# window; wezterm's CLI spawns `wezterm-gui`, which the name already
# covers.
HELPERS = {"kitty": ("kitten",)}


def comm_matches(comm: str, name: str) -> bool:
    """Is this `ps -o comm=` line one of `name`'s own processes?

    The basename, not the whole line: macOS prints the executable's full
    path, so a substring test against it matches anything installed
    under a directory named after the terminal.
    """
    base = os.path.basename(comm.strip()).lower()
    return any(want.lower() in base for want in (name, *HELPERS.get(name, ())))


def child_pids(parent: int, name: str) -> list[int]:
    """The launch pid plus its children that belong to the terminal.

    A child counts when its executable is named after the terminal or is
    one of its `HELPERS`; the shell and the workload under it do not, so
    their cost never lands on the terminal's bar.
    """
    pids = [parent]
    found = subprocess.run(["pgrep", "-P", str(parent)], capture_output=True, text=True)
    for pid in found.stdout.split():
        comm = subprocess.run(
            [PS, "-o", "comm=", "-p", pid], capture_output=True, text=True
        ).stdout
        if comm_matches(comm, name):
            pids.append(int(pid))
    return pids

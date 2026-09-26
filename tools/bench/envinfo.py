#!/usr/bin/env python3
"""Machine and toolchain provenance for a cross-terminal benchmark run.

A cross-terminal number means nothing without the machine it came from:
the same suite on a laptop running on battery, or with another kitty
build, is a different measurement. Everything recorded here is something
that has silently changed a result — core counts and RAM, the GPU and the
display the window mapped on, the power source and any thermal throttling,
and the exact binary of every terminal in the field.

Collection shells out; parsing is pure functions over the captured text,
so `crossterm_test.py` can pin the parsers against recorded output
without a machine to run on. Anything that cannot be read is recorded as
None rather than guessed — a blank field is honest, an invented one is not.

Run standalone to see what a run would record:

    python3 tools/bench/envinfo.py
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import plistlib
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

# ── shelling out ─────────────────────────────────────────────────────


def run(*cmd: str, timeout: float = 20.0) -> str | None:
    """Capture a command's stdout, or None if it is unavailable/fails."""
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    return proc.stdout.strip() or None


def sysctl(key: str) -> str | None:
    return run("sysctl", "-n", key)


def sysctl_int(key: str) -> int | None:
    value = sysctl(key)
    try:
        return int(value) if value is not None else None
    except ValueError:
        return None


def read_text(path: str) -> str | None:
    try:
        return Path(path).read_text()
    except OSError:
        return None


# ── parsers (pure; pinned by crossterm_test.py) ──────────────────────


def parse_pmset_batt(text: str) -> dict:
    """`pmset -g batt` → power source and battery percentage."""
    source = None
    if match := re.search(r"drawing from '([^']+)'", text):
        source = match.group(1)
    percent = None
    if match := re.search(r"(\d+)%", text):
        percent = int(match.group(1))
    return {"source": source, "battery_percent": percent}


def parse_pmset_therm(text: str) -> dict:
    """`pmset -g therm` → the speed limit, i.e. whether we are throttled."""
    out = {}
    for key, field in (
        ("CPU_Speed_Limit", "cpu_speed_limit"),
        ("CPU_Scheduler_Limit", "cpu_scheduler_limit"),
    ):
        if match := re.search(rf"{key}\s*=\s*(\d+)", text):
            out[field] = int(match.group(1))
    return out


# Where Linux counts the times a core was clamped for heat. A counter,
# not a state: it says a leg was throttled while it ran, which is the
# question here, and unlike a cpufreq reading it cannot be confused
# with a governor's own decision.
THROTTLE_ROOT = Path("/sys/devices/system/cpu")


def linux_throttle(root: Path = THROTTLE_ROOT) -> dict | str:
    events, found = 0, False
    for path in sorted(root.glob("cpu*/thermal_throttle/*_throttle_count")):
        text = (read_text(str(path)) or "").strip()
        if text.isdigit():
            events, found = events + int(text), True
    return {"kind": "counters", "events": events} if found else "unavailable"


def throttle_state() -> dict | str:
    """Whether the CPU is being held back, in whichever terms the OS has.

    macOS reports a limit it is currently holding the CPU to and Linux a
    count of throttling events, so the two are recorded as they are
    read; a machine that exposes neither says so rather than reporting
    an unthrottled CPU it did not check.
    """
    if sys.platform != "darwin":
        return linux_throttle()
    if raw := run("pmset", "-g", "therm"):
        if parsed := parse_pmset_therm(raw):
            return {"kind": "speed_limit", **parsed}
    return "unavailable"


def leg_env() -> dict:
    """The machine as one leg found it.

    `machine()` records this once, at the start; a full pass is over an
    hour, so a leg that ran beside a background job or on a throttled
    CPU is otherwise indistinguishable from a slow terminal.
    """
    return {
        "loadavg": round(os.getloadavg()[0], 2),
        "throttle": throttle_state(),
        "display": display_state(),
    }


def parse_flag(text: str | None) -> bool | None:
    """A yes/no a platform tool printed, or None for anything else."""
    value = (text or "").strip().lower()
    if value in ("1", "yes", "true"):
        return True
    if value in ("0", "no", "false"):
        return False
    return None


def parse_console_lock(raw: bytes) -> bool | None:
    """`ioreg -a -n Root -d1` → whether the console session is locked.

    `IOConsoleLocked` first; the on-console session's
    `CGSSessionScreenIsLocked` stands in where the root lacks it, and
    that key is absent rather than false in an unlocked session.
    """
    try:
        root = plistlib.loads(raw)
    except (plistlib.InvalidFileException, ValueError):
        return None
    if not isinstance(root, dict):
        return None
    if isinstance(locked := root.get("IOConsoleLocked"), bool):
        return locked
    for session in root.get("IOConsoleUsers") or []:
        if isinstance(session, dict) and session.get("kCGSSessionOnConsoleKey"):
            return bool(session.get("CGSSessionScreenIsLocked", False))
    return None


MAIN_DISPLAY_ASLEEP = (
    "ObjC.import('CoreGraphics'); $.CGDisplayIsAsleep($.CGMainDisplayID())"
)


def display_state() -> dict:
    """Whether a window opened now could present a frame.

    macOS presents nothing behind the lock screen or to a sleeping
    display, and a felis window that cannot present re-arms its frame
    pull as fast as the acquire fails, so the throughput, CPU and memory
    such a leg records describe that loop rather than a terminal
    drawing. The main display, because that is where a fresh window
    opens.

    Linux reports the lock through logind's `LockedHint` on the seat's
    active session, which only a locker that tells logind sets, and has
    no display-sleep reading to take: both unread halves are None.
    """
    if sys.platform == "darwin":
        raw = run("ioreg", "-a", "-n", "Root", "-d1")
        asleep = run("osascript", "-l", "JavaScript", "-e", MAIN_DISPLAY_ASLEEP)
        return {
            "locked": parse_console_lock(raw.encode()) if raw else None,
            "asleep": parse_flag(asleep),
        }
    session = run("loginctl", "show-seat", "seat0", "-p", "ActiveSession", "--value")
    locked = None
    if session:
        locked = parse_flag(
            run("loginctl", "show-session", session, "-p", "LockedHint", "--value")
        )
    return {"locked": locked, "asleep": None}


def cannot_present(sample: dict) -> str | None:
    """Why a leg sampled as `sample` could not have presented, or None."""
    display = sample.get("display") or {}
    reasons = [
        why
        for key, why in (
            ("locked", "the screen was locked"),
            ("asleep", "the display was asleep"),
        )
        if display.get(key) is True
    ]
    return " and ".join(reasons) or None


def parse_displays(payload: dict) -> dict:
    """`system_profiler -json SPDisplaysDataType` → GPU and displays.

    Resolution and refresh matter because the client renders at the
    window's backing scale and presents on the display's clock: the same
    binary on a 60 Hz panel and a 120 Hz panel is not the same workload.
    """
    gpus, displays = [], []
    for card in payload.get("SPDisplaysDataType", []):
        gpus.append(
            {
                "model": card.get("sppci_model"),
                "cores": card.get("sppci_cores"),
                "metal": card.get("spdisplays_mtlgpufamilysupport"),
            }
        )
        for screen in card.get("spdisplays_ndrvs", []):
            displays.append(
                {
                    "name": screen.get("_name"),
                    "resolution": screen.get("_spdisplays_resolution"),
                    "pixels": screen.get("_spdisplays_pixels"),
                    "refresh": screen.get("_spdisplays_refresh")
                    or screen.get("spdisplays_refresh"),
                    "retina": screen.get("spdisplays_retina"),
                    "main": screen.get("spdisplays_main") is not None,
                    "backing_scale": backing_scale(screen),
                }
            )
    return {"gpus": gpus, "displays": displays}


def backing_scale(screen: dict) -> float | None:
    """Physical pixels per logical point, from native vs "UI looks like".

    Part of the run's conditions rather than of the font pin: the same
    pinned size paints four times the pixels at 2x, so a number compared
    against another machine's has to carry it. Deriving it from the two
    resolutions rather than the "Retina" flag also gets scaled modes
    right: a 3456-pixel panel driven at a 1728-point desktop is 2x
    whether or not it is advertised as Retina.
    """
    native = parse_wh(screen.get("_spdisplays_pixels"))
    logical = parse_wh(screen.get("_spdisplays_resolution"))
    if not native or not logical or not logical[0]:
        return None
    return round(native[0] / logical[0], 3)


def parse_wh(text: str | None) -> tuple[int, int] | None:
    """`"3440 x 1440 @ 60.00Hz"` → (3440, 1440)."""
    if not text:
        return None
    parts = text.split("@")[0].split("x")
    if len(parts) != 2:
        return None
    try:
        return int(parts[0].strip()), int(parts[1].strip())
    except ValueError:
        return None


def main_backing_scale(machine: dict) -> float | None:
    """The scale of the display the benchmark windows will open on.

    The main display, since that is where a WM puts a fresh window; with
    no main flag (Linux, or a parse miss) the first one stands in. None
    means "unknown", and the caller assumes 1 rather than guessing.
    """
    displays = machine.get("displays") or []
    for screen in displays:
        if screen.get("main") and screen.get("backing_scale"):
            return screen["backing_scale"]
    for screen in displays:
        if screen.get("backing_scale"):
            return screen["backing_scale"]
    return None


def parse_meminfo(text: str) -> int | None:
    """/proc/meminfo → total bytes."""
    if match := re.search(r"^MemTotal:\s+(\d+) kB", text, re.M):
        return int(match.group(1)) * 1024
    return None


def parse_cpuinfo(text: str) -> str | None:
    """/proc/cpuinfo → the CPU model string."""
    if match := re.search(r"^model name\s*:\s*(.+)$", text, re.M):
        return match.group(1).strip()
    if match := re.search(r"^Model\s*:\s*(.+)$", text, re.M):
        return match.group(1).strip()
    return None


def first_line(text: str | None) -> str | None:
    """Version banners are often multi-line; the first line is the version."""
    if not text:
        return None
    return text.splitlines()[0].strip() or None


def paren_hash(line: str | None) -> str | None:
    """`"felis 0.1.0 (b27da75c9f1e-dirty)"` → `"b27da75c9f1e"`.

    The commit `felis --version` was compiled against, embedded by
    `crates/felis-cli/build.rs`. This is the only revision that names the
    binary that ran; everything else names a directory. The `-dirty`
    suffix is dropped here and reported by `revision_drift` instead.
    """
    if not line:
        return None
    if match := re.search(r"\(([0-9a-f]{7,40})(?:-dirty)?\)", line):
        return match.group(1)
    return None


def revision_drift(binary: str | None, checkout: str | None) -> str | None:
    """A sentence naming why `binary` is not `checkout`, or None if it is.

    Attributing a measurement to the wrong commit is the failure this
    whole block exists to prevent, and it is silent: a `nix build` result
    or a stale `target/release` is a perfectly good binary, just not the
    one the tree the report is read next to would produce.

    Compared by prefix because the two hashes are abbreviated
    independently (`git describe --always` and the build script's
    `rev-parse --short` need not pick the same length). A dirty checkout
    is called out even when the hashes agree: the binary cannot carry a
    hash for uncommitted work, so the revision does not identify it.
    """
    if binary is None or checkout is None:
        return None
    head = checkout.removesuffix("-dirty")
    if not (binary.startswith(head) or head.startswith(binary)):
        return f"binary is {binary}, the checkout is {checkout}"
    if checkout != head:
        return f"binary is {binary} plus whatever the checkout has uncommitted"
    return None


# ── collectors ───────────────────────────────────────────────────────


def machine_darwin() -> dict:
    displays = {}
    if raw := run("system_profiler", "-json", "SPDisplaysDataType", timeout=60):
        try:
            displays = parse_displays(json.loads(raw))
        except json.JSONDecodeError:
            displays = {}
    power = {}
    if raw := run("pmset", "-g", "batt"):
        power = parse_pmset_batt(raw)
    if raw := run("pmset", "-g", "therm"):
        power |= parse_pmset_therm(raw)
    return {
        "model": sysctl("hw.model"),
        "cpu": sysctl("machdep.cpu.brand_string"),
        "cpu_logical": sysctl_int("hw.logicalcpu"),
        # Scheduling on a P/E-core split machine is not uniform, and a
        # benchmark that lands on the efficiency cluster reads slow for
        # reasons that have nothing to do with the code.
        "cpu_performance_cores": sysctl_int("hw.perflevel0.logicalcpu"),
        "cpu_efficiency_cores": sysctl_int("hw.perflevel1.logicalcpu"),
        "memory_bytes": sysctl_int("hw.memsize"),
        "power": power or None,
        **displays,
    }


def machine_linux() -> dict:
    gpus = []
    if raw := run("lspci"):
        gpus = [
            line.split(": ", 1)[-1]
            for line in raw.splitlines()
            if re.search(r"\b(VGA|3D|Display)\b", line)
        ]
    ac = None
    for supply in sorted(Path("/sys/class/power_supply").glob("A*/online")):
        if (value := read_text(str(supply))) is not None:
            ac = value.strip() == "1"
            break
    return {
        "model": (read_text("/sys/devices/virtual/dmi/id/product_name") or "").strip()
        or None,
        "cpu": parse_cpuinfo(read_text("/proc/cpuinfo") or ""),
        "cpu_logical": os.cpu_count(),
        "memory_bytes": parse_meminfo(read_text("/proc/meminfo") or ""),
        "gpus": [{"model": g} for g in gpus] or None,
        "governor": (
            read_text("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor") or ""
        ).strip()
        or None,
        "power": {"source": "AC Power" if ac else "Battery Power"}
        if ac is not None
        else None,
        "session": {
            "type": os.environ.get("XDG_SESSION_TYPE"),
            "desktop": os.environ.get("XDG_CURRENT_DESKTOP"),
        },
    }


def machine() -> dict:
    info = machine_darwin() if sys.platform == "darwin" else machine_linux()
    # A one-minute load average, sampled when the run starts. Not a
    # property of the machine so much as of the moment, but it belongs
    # with them: a busy desktop is the difference between a terminal
    # opening in 1s and appearing not to open at all, which is how a
    # whole field once read as broken here.
    load = os.getloadavg()[0]
    return {"arch": platform.machine(), "load_average": round(load, 2), **info}


def os_info() -> dict:
    if sys.platform == "darwin":
        return {
            "name": "macOS",
            "version": run("sw_vers", "-productVersion"),
            "build": run("sw_vers", "-buildVersion"),
            "kernel": platform.release(),
        }
    name, version = None, None
    if raw := read_text("/etc/os-release"):
        fields = dict(line.split("=", 1) for line in raw.splitlines() if "=" in line)
        name = fields.get("NAME", "").strip('"') or None
        version = fields.get("VERSION_ID", "").strip('"') or None
    return {"name": name, "version": version, "kernel": platform.release()}


VERSION_FLAGS = {
    # ghostty and wezterm print a banner; first_line() trims it to the
    # version. tb has no version flag at all, hence the empty tuple, and
    # DOOM-fire has no flags at all — it would start drawing instead.
    "tb": (),
    "doom-fire": (),
    "ghostty": ("--version",),
}


def tool_version(path: str, name: str) -> str | None:
    flags = VERSION_FLAGS.get(name, ("--version",))
    if not flags:
        return None
    return first_line(run(path, *flags, timeout=30))


def describe_tool(name: str, path: str | None, pinned: bool) -> dict:
    """One row of the tool table: what ran, from where, which version."""
    if path is None:
        return {"name": name, "found": False}
    row = {
        "name": name,
        "found": True,
        "path": path,
        # "flake" means the .#bench shell handed us this exact store path;
        # "path" means we took whatever the host had, which is the half of
        # a comparison that does not reproduce elsewhere.
        "source": "flake" if pinned else "path",
        "version": tool_version(path, name),
    }
    if not pinned:
        # A store path names its own contents, so a pinned tool needs
        # nothing further. An unpinned one is only as identifiable as its
        # own banner chooses to be — ghostty's tip happens to carry a
        # commit, but `--tool` accepts any binary, including a local
        # build whose version string names nothing. The digest identifies
        # the file either way.
        row |= digest(Path(path))
    return row


def digest(path: Path) -> dict:
    """sha256 and mtime of a binary, for the tools the flake did not pin."""
    try:
        h = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                h.update(chunk)
    except OSError:
        return {}
    row = {"sha256": h.hexdigest()}
    if built := build_time(path):
        row["built"] = built
    return row


def build_time(path: Path) -> str | None:
    """When a binary was written, or None when its mtime dates nothing.

    Nix normalizes every store path's mtime to the epoch, so a store
    binary's timestamp reads as 1970 — a field that looks like an answer
    and is not. The store path names its own contents anyway.
    """
    try:
        mtime = path.stat().st_mtime
    except OSError:
        return None
    if mtime < 86400:
        return None
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(mtime))


def felis_info(repo: Path, felis_bin: Path) -> dict:
    """Which felis was measured, and whether it is this checkout's.

    `revision` is the binary's own embedded hash, because the run
    measures a binary: a `nix build` result or a stale `target/release`
    can be any number of commits behind the tree the report is read next
    to, and reporting the tree's revision there is how a win gets
    attributed to a commit that does not contain it. The checkout is
    recorded beside it, and a disagreement is stated outright in `drift`
    rather than left as two fields for a reader to compare.
    """
    version = first_line(run(str(felis_bin), "--version"))
    checkout = run("git", "-C", str(repo), "describe", "--always", "--dirty")
    binary = paren_hash(version)
    info = {
        "revision": binary or checkout,
        "checkout": checkout,
        "binary": str(felis_bin),
        "version": version,
        "built": build_time(felis_bin),
    }
    if drift := revision_drift(binary, checkout):
        info["drift"] = drift
    return info


def collect(
    repo: Path,
    felis_bin: Path,
    tools: list[dict],
    params: dict,
    probed: dict | None = None,
) -> dict:
    """The full provenance block written to <results-root>/meta.json.

    `probed` lets the caller pass a machine block it already collected —
    the orchestrator needs the display's backing scale before the run,
    to record it with the pinned condition it prints, and
    `system_profiler` is slow enough that collecting it twice is worth
    avoiding.
    """
    return {
        "host": run("scutil", "--get", "ComputerName") or platform.node(),
        "machine": probed if probed is not None else machine(),
        "os": os_info(),
        "felis": felis_info(repo, felis_bin),
        "tools": tools,
        "params": params,
        # Whether the pinned shell was active at all: the single fact that
        # tells a later reader if the field was reproducible.
        "pinned_shell": bool(os.environ.get("FELIS_BENCH_SHELL")),
    }


def format_bytes(value: int | None) -> str | None:
    if not value:
        return None
    return f"{value / 1024**3:.0f} GB"


if __name__ == "__main__":
    probe = [
        describe_tool(name, shutil.which(name), False)
        for name in ("kitty", "alacritty", "wezterm", "ghostty", "foot")
    ]
    print(
        json.dumps(
            collect(Path.cwd(), Path("target/release/felis"), probe, {}),
            indent=2,
            ensure_ascii=False,
        )
    )

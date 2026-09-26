---
name: producer-traffic-debug
description:
  Drive the felis terminal unattended on Linux/Wayland (niri) to observe REAL producer traffic — yazi, presenterm, timg,
  hand-rolled Kitty escapes — and debug it from the logs. Use when investigating a rendering/graphics/Kitty-protocol bug
  (image placement, Unicode placeholders, APC dispatch, capability detection) and you need the daemon's and client's
  actual event flow without a human at the keyboard. Covers the $SHELL-wrapper launch trick, the log lines that are the
  reliable signal, and the daemon/socket/screenshot gotchas. For throughput benchmarking/profiling instead, see the
  perf-trace skill.
license: same as the felis repository
compatibility:
  Linux + Wayland (niri) host with felis built (niri msg helps for output scale / screenshots). macOS also works — the
  launch trick is cross-platform; see the "macOS" section for the host-specific bits, and the felis-macos-gui-debug
  skill for screenshots/pixel probes.
metadata:
  author: felis
  version: "1.0"
allowed-tools:
  Bash(./target/release/felis:*) Bash(.agents/skills/producer-traffic-debug/scripts/*) Bash(pgrep:*) Bash(grep:*)
  Bash(niri:*) Read
---

# Debugging felis from real producer traffic

felis is a `winit`/`wgpu` Wayland app, but you can drive it unattended to watch what a real producer (yazi, presenterm,
timg, raw Kitty escapes) actually sends: this is useful for graphics/Kitty-protocol bugs where the question is "what did
the producer emit and where did felis lose it".

## Run it: drive.sh

```
.agents/skills/producer-traffic-debug/scripts/drive.sh 'yazi /some/dir' 6
.agents/skills/producer-traffic-debug/scripts/drive.sh 'timg -p kitty x.png'
```

The trick the script encodes: point `$SHELL` at a wrapper that runs the producer then idles, start `felis-daemon serve`
on a **unique `--socket`** in the background with its stderr in the log, then open the window on that socket, all with a
debug `RUST_LOG`. An auto-spawned daemon would not do: its console is `/dev/null` or the systemd journal, and its lines
reach only `daemon.log` in the log directory (`docs/reference/cli.md` "Log files").

## The reliable signal is the logs, not a screenshot

Grep these (the script tails them for you):

- `felis_daemon::graphics: dispatch_apc_bodies … events=[…]`: what the producer actually sent
  (`Transmit`/`Placement`/`VirtualPlacement`/`Delete`/… + image ids; reveals `U=1` vs anchored, and id churn).
- `felis_client::event_handler: image header`: image reached the client.
- `felis::placeholder: placeholder pass … ph_cells/ph_tiles/ph_no_slot/ ph_no_vp`: the Kitty Unicode-placeholder render
  lifecycle, cell by frame.

## Gotchas that cost real time

- **Daemons detach and persist.** Kill leftovers by pid filtered on the full socket path. Never `pkill -f <socket>`: the
  pattern matches your own shell's argv and kills it. Never `basename "$SOCK"` either, because that matches the user's
  real daemon too. The script cleans up on exit.
- **Reuse a socket → you connect to the stale daemon** (no fresh logs). Fresh `--socket` per run, in a dedicated
  directory (`mktemp -d`), since `bind` requires a `0700` parent you own and refuses a socket straight under `/tmp`.
- **niri**: `niri msg outputs` gives the output scale (decisive for HiDPI questions).
  `niri msg action screenshot-screen` / `screenshot-window` can grab a frame, but it shoots the _focused_ window (often
  your terminal, not the felis tile), which is a bonus over logs, not a primary signal.
- **Capability detection** (presenterm/yazi probing for Kitty support) needs the window pumping frames, which needs
  focus (flaky under this launch mode). An ASCII-fallback run may be a focus artifact, not a felis bug; re-run or
  confirm with the logs before believing it.

## macOS (darwin): what changes

The `$SHELL`-wrapper launch trick is cross-platform and works as-is. The host-specific bits differ:

- **Socket path.** The same rule as Linux, and the same `mktemp -d` line
  (`RTDIR=$(mktemp -d /tmp/felis_dbg.XXXXXX); SOCK="$RTDIR/d.sock"`), never bare `/tmp/x.sock`: the daemon refuses to
  bind under a parent that is not a `0700` directory it owns. Point `FELIS_BIN` and `FELIS_DAEMON_BIN` at the same
  `./target/<prof>/` build so the window and the daemon match.
- **Daemon cleanup.** No `/proc`. Filter on the full socket path as one whole `ps` token, not `/proc/$p/cmdline`:
  `for p in $(pgrep -x felis-daemon); do ps -o command= -p "$p" | tr ' ' '\n' | grep -qxF -- "$SOCK" && kill "$p"; done`
- **Sandbox.** Under the Claude Code Bash sandbox the daemon's socket `bind()` is blocked (EPERM); these runs need the
  sandbox disabled.
- **Screenshots / pixel probes: use the `felis-macos-gui-debug` skill.** It finds the CGWindowID via JXA (no python, no
  Accessibility permission, only Screen-Recording rights) and captures the window alone with `screencapture -l<id>`
  regardless of z-order, plus exact RGBA probes. Logs remain the primary signal for producer-traffic bugs; reach for the
  capture only when confirming a rendered frame (color emoji, image placement).

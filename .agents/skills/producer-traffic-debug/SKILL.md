---
name: producer-traffic-debug
description:
  Drive the felis terminal unattended to observe REAL producer traffic — yazi, presenterm, timg, hand-rolled Kitty
  escapes — and debug it from the logs. Use when investigating a rendering/graphics/Kitty-protocol bug (image placement,
  Unicode placeholders, APC dispatch, capability detection) and you need the daemon's and client's actual event flow
  without a human at the keyboard. Covers the $SHELL-wrapper launch trick, the log lines that are the reliable signal,
  and the focus/screenshot gotchas on Linux (niri) and macOS. For throughput benchmarking/profiling instead, see the
  perf-trace skill.
compatibility:
  Linux + Wayland (niri) or macOS host with felis built. Screenshots and pixel probes on macOS are the
  felis-macos-gui-debug skill.
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

The trick the script encodes: point `$SHELL` at a wrapper that runs the producer then idles, and open the window on a
private daemon (the `isolated-daemon` skill) whose stderr goes into the same log, all with a debug `RUST_LOG`.
`FELIS_BIN` and `FELIS_DAEMON_BIN` name the binaries; point both at the same `./target/<prof>/` build.

## The reliable signal is the logs, not a screenshot

Grep these (the script tails them for you):

- `felis_daemon::graphics: dispatch_apc_bodies … events=[…]`: what the producer actually sent
  (`Transmit`/`Placement`/`VirtualPlacement`/`Delete`/… + image ids; reveals `U=1` vs anchored, and id churn).
- `felis_client::event_handler: image header`: image reached the client.
- `felis::placeholder: placeholder pass … ph_cells/ph_tiles/ph_no_slot/ ph_no_vp`: the Kitty Unicode-placeholder render
  lifecycle, cell by frame.

## Gotchas that cost real time

- **Daemons detach and persist.** The script tears its own down on exit; a run you start by hand follows the
  `isolated-daemon` skill's teardown, never a `pkill`.
- **niri**: `niri msg outputs` gives the output scale (decisive for HiDPI questions).
  `niri msg action screenshot-screen` / `screenshot-window` can grab a frame, but it shoots the _focused_ window (often
  your terminal, not the felis tile), which is a bonus over logs, not a primary signal.
- **Capability detection** (presenterm/yazi probing for Kitty support) needs the window pumping frames, which needs
  focus (flaky under this launch mode). An ASCII-fallback run may be a focus artifact, not a felis bug; re-run or
  confirm with the logs before believing it.

## macOS

`drive.sh` runs as-is (the `isolated-daemon` helper covers the platform differences in socket handling and teardown).
For screenshots and pixel probes use the `felis-macos-gui-debug` skill: it captures the felis window alone regardless of
z-order and reads exact RGBA. Logs remain the primary signal for producer-traffic bugs; reach for the capture only when
confirming a rendered frame (color emoji, image placement).

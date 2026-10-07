---
name: felis-macos-gui-debug
description:
  Drive the felis client unattended on macOS to verify rendering, window-chrome, and mouse-gesture changes — find the
  CGWindowID via JXA, capture the window alone with screencapture -l, read exact RGBA (incl. per-pixel alpha for
  transparency checks) via NSBitmapImageRep, synthesize keystrokes, chords, clicks and drags with CGEventPost, and run
  an isolated daemon so none of it touches the user's sessions. Use for blur/opacity/corner/glyph rendering bugs, for
  selection / mouse-reporting behavior, and for driving any [keymap] chord (pipe, run, session switches) on macOS; for
  producer-traffic/log debugging use producer-traffic-debug, for throughput use perf-trace.
compatibility: macOS with felis built; synthesized input may need an Accessibility grant for the calling terminal.
allowed-tools:
  Bash(.agents/skills/isolated-daemon/scripts/*) Bash(./target/release/felis:*) Bash(./target/release/felis-daemon:*)
  Bash(osascript:*) Bash(screencapture:*) Bash(swift:*) Bash(pgrep:*) Bash(ps:*) Bash(grep:*) Bash(kill:*) Read Write
---

# macOS GUI debugging for felis

Verify a rendering or window-chrome change by launching the real client, capturing **just its window**, and probing
pixels. Capture and pixel probing need no accessibility permission; synthesized input may (if a post silently does
nothing, check Accessibility for the terminal running `osascript`).

Keyboard and mouse events both reach the window through `CGEventPost`, so every `[keymap]` chord (`pipe`, `run`, the
switch actions) is drivable from here (see "Keyboard input", "Mouse gestures"). What a chord does _below_ the client is
still worth an integration test against a real daemon: `crates/felis-client-core/tests/transient_spawn.rs` spawns the
session the chord would have spawned and asserts on what the child received, without a window in the loop.

## Launch → window id → capture

Every run starts on a private daemon (the `isolated-daemon` skill), never the user's:

```sh
eval "$(.agents/skills/isolated-daemon/scripts/isolated-daemon.sh start)"   # sets SOCK and LOG
./target/release/felis --socket "$SOCK" >/tmp/felis.log 2>&1 & sleep 4
WID=$(osascript -l JavaScript -e '
ObjC.import("CoreGraphics");
const list = ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, $.kCGNullWindowID));
const arr = ObjC.deepUnwrap(list);
const w = arr.filter(x => x.kCGWindowOwnerName === "felis")[0];
w ? w.kCGWindowNumber : "none";')
screencapture -o -l$WID /tmp/felis-win.png   # window only, no shadow
```

- `ObjC.castRefToObject` is required: `ObjC.deepUnwrap` on the raw CFArrayRef fails with "Ref has incompatible type".
- `screencapture -o -l<id>` captures the window's own composited content **with per-pixel alpha**: a translucent
  background shows its real alpha (e.g. 153 for `opacity = 0.6`), and rounded-corner pixels are fully transparent. This
  is the ground truth for transparency / corner-clip checks; a full-screen capture is not.
- A tiling WM may resize the window after launch; re-read the bounds (`kCGWindowBounds`) instead of assuming 960×600.
  The post-resize capture is also the regression test for AppKit relayout bugs (sublayer reordering happens on the first
  layout pass).

## Pixel probe (exact RGBA, no python needed)

```sh
osascript -l JavaScript -e '
ObjC.import("AppKit");
const rep = $.NSBitmapImageRep.imageRepWithContentsOfFile("/tmp/felis-win.png");
function p(x,y){const c=rep.colorAtXY(x,y);return [c.redComponent,c.greenComponent,c.blueComponent,c.alphaComponent].map(v=>Math.round(v*255)).join(",");}
const h = rep.pixelsHigh*1, w = rep.pixelsWide*1;
JSON.stringify({w:w,h:h,center:p(480,Math.floor(h/2)),tl:p(2,2),bl:p(2,h-3),br:p(w-3,h-3)});'
```

Corner pixels `0,0,0,0` = rounded clip works. With `window.backdrop = "blur"` the center alpha is 255 (the
NSVisualEffectView behind the Metal layer is opaque frosting). That is correct, not a transparency failure; test raw
alpha with `backdrop = "none"`.

## Keyboard input (plain keys and chords)

```js
// osascript -l JavaScript key.js <pid> <keycode> [flags]
ObjC.import("CoreGraphics");
ObjC.import("AppKit");
function run(argv) {
  const pid = parseInt(argv[0], 10);
  const app = $.NSRunningApplication.runningApplicationWithProcessIdentifier(pid);
  if (!app.isNil()) app.activateWithOptions($.NSApplicationActivateIgnoringOtherApps);
  delay(1.5); // let focus and the WM settle
  const src = $.CGEventSourceCreate(1); // kCGEventSourceStateHIDSystemState
  const down = $.CGEventCreateKeyboardEvent(src, parseInt(argv[1], 10), true);
  const up = $.CGEventCreateKeyboardEvent(src, parseInt(argv[1], 10), false);
  const flags = argv[2] ? parseInt(argv[2], 10) : 0;
  if (flags) {
    $.CGEventSetFlags(down, flags);
    $.CGEventSetFlags(up, flags);
  }
  $.CGEventPostToPid(pid, down);
  delay(0.12);
  $.CGEventPostToPid(pid, up);
}
```

- `CGEventPostToPid` and the tap-wide `CGEventPost(0, …)` both arrive; the per-pid form is the safer default, since a
  mistimed tap post lands in whatever the WM focused instead.
- Flag masks: shift `0x20000` (131072), control `0x40000` (262144), option `0x80000`, command `0x100000`; OR them for a
  chord: `ctrl+shift+h` is keycode 4 with 393216.
- Virtual keycodes are ANSI positions, not characters: `h` 4, `c` 8, `d` 2, `x` 7, PageUp 116. Ctrl-D to end a transient
  is keycode 2 with 262144.
- **Do not `rm` the probe shell's log between posts.** The session's writer holds the unlinked inode and keeps writing
  to it, so the path reads as missing while every keystroke lands, which is the failure mode that reads as "keyboard
  synthesis does not work". Truncate before the session starts, or restart the pair.

## Mouse gestures (clicks, drags) into the window

`CGEventPost` from JXA reaches the window (it posts without any prompt from a terminal-run `osascript`, so an
accessibility grant may or may not be in play). This is the only way to exercise the mouse path: the client→daemon mouse
encode path has no headless driver. If the events silently do nothing, check Accessibility for the terminal.

```js
// osascript -l JavaScript click.js <pid> <fracX> <fracY> [dragFracX dragFracY]
ObjC.import("CoreGraphics");
ObjC.import("AppKit");
function run(argv) {
  const pid = parseInt(argv[0], 10);
  const app = $.NSRunningApplication.runningApplicationWithProcessIdentifier(pid);
  if (!app.isNil()) app.activateWithOptions($.NSApplicationActivateIgnoringOtherApps);
  delay(1.5); // let the WM settle
  const list = ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, $.kCGNullWindowID));
  const b = ObjC.deepUnwrap(list).filter((w) => w.kCGWindowOwnerPID === pid && w.kCGWindowLayer === 0)[0]
    .kCGWindowBounds;
  const at = (rx, ry) => ({ x: b.X + b.Width * rx, y: b.Y + b.Height * ry });
  const src = $.CGEventSourceCreate(1); // kCGEventSourceStateHIDSystemState
  const post = (type, p) => {
    $.CGEventPost(0, $.CGEventCreateMouseEvent(src, type, $.CGPointMake(p.x, p.y), 0));
    delay(0.15); // kCGHIDEventTap
  };
  // types: 1 down, 2 up, 5 moved, 6 dragged
  const p0 = at(parseFloat(argv[1]), parseFloat(argv[2]));
  post(5, p0);
  post(1, p0);
  post(2, p0);
}
```

- **Resolve the window bounds _after_ activating**, never before: a tiling WM (paneru) moves the window on focus, and a
  click computed from stale global coordinates lands on some other app's window. Stop the WM (`paneru stop`, restart
  after) for anything multi-step.
- Use fractions of `kCGWindowBounds`, not absolute pixels: the window is wherever the WM put it.
- Step a drag through several `dragged` events; one teleport to the end point does not produce the intermediate cell
  crossings the client needs to promote a click into a drag.
- To read what the program actually received, give the session a `SHELL` wrapper (set on the client invocation) that
  puts the tty in raw mode, enables reporting, and logs bytes:
  `stty raw -echo; printf '\033[?1002h\033[?1006h'; exec cat -uv > /tmp/felis-mouse-in.log`. Without `stty raw` the tty
  stays canonical and no escape byte ever reaches the reader.
- Stray `^[[<64…67;…M` wheel reports appear in that log from real trackpad / WM activity: read the button-0
  press/release pair, not the line count.

## Catching an animation mid-flight

A capture takes ~200 ms to set up, so a one-shot animation (the cursor trail, a blink phase) is over before
`screencapture` runs. Make the animation repeat instead of chasing it: a `SHELL` wrapper that loops the trigger faster
than the effect decays turns "capture at the right moment" into "capture at any moment".

```sh
printf '\033[2J'
while :; do printf '\033[2;3Hstart'; sleep 0.25; printf '\033[28;92H*'; sleep 0.25; done
```

To prove the _opposite_ (that the animation terminates and the window returns to idle), run a quiet `SHELL`
(`sleep 600`), set `cursor.blink = "never"` so the caret arms no timer of its own, and read `ps -o pcpu= -p <pid>`:
`0.0` across several seconds is the idle-zero check. `ps -o time=` / `utime=` need an entitlement on current macOS and
fail; `pcpu` does not.

## Config experiments

The config path is `~/Library/Application Support/felis/config.toml`. Run a modified copy through `--config`, as the
`isolated-daemon` skill describes:

```sh
sed -e 's/^opacity = .*/opacity = 0.6/' \
  "$HOME/Library/Application Support/felis/config.toml" \
  > /tmp/felis-dbg-config.toml
./target/release/felis --socket "$SOCK" --config /tmp/felis-dbg-config.toml 2>/dev/null &
```

An experiment that needs its own `SHELL` wrapper sets it on the client:
`env SHELL=/path/to/probe.sh ./target/release/felis --socket "$SOCK"`.

## Teardown and host quirks

- End every run with `isolated-daemon.sh stop "$SOCK"`; the skill explains why a `pkill` by name or pattern is never the
  teardown.
- Comparing against an older commit: the `isolated-daemon` skill's "Comparing against an older commit".
- Multi-display layouts make `screencapture -x -R` fail with "could not create image from rect"; capture per display
  (`-D 1`) or stick to window captures.

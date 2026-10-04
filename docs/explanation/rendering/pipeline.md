---
title: Render pipeline
sidebar:
  order: 1
---

The client owns all rendering. This page covers how that rendering is organized: the process model and the per-frame
flow, the GPU resources, how frames are paced across the IPC boundary, window chrome, the optional post-process stage,
and the prior art behind the draw and atlas choices.

## Process model

The client is one OS process whose screen state has a single owner. A tokio task reads grid, image, and control frames
from the daemon and forwards each one to the winit event loop as a user event (`AppEvent`); the event loop applies it to
the in-memory **shadow screen**, and the renderer runs synchronously inside the redraw callback. Reader and renderer
never touch the shadow at the same time, so it needs no lock: running both on the event loop serializes "apply a frame"
against "paint a frame" by construction.

A lock over a shared shadow is the alternative, and it costs more than it saves. The populate walk reads the grid and
its output (atlas slots, instance rows) is coherent only against one version of it, so the renderer would hold the lock
for the walk anyway; the reader would then block exactly where it was supposed to run free. Routing frames through the
event-loop queue also gives them a coalescing point the lock does not: the loop drains the queue and sets one sticky
redraw request, so a burst of frames costs one paint ([damage-tracking.md](damage-tracking.md) "Client repaint").

## Per-frame flow

```
1. winit dispatches RedrawRequested.
2. The renderer walks the rows the shadow marked since the
   last paint for glyphs and rebuilds their cell instances
   (damage-tracking.md), plus live image placements.
3. Each row is cut into shaping units: a ligature run where the
   fast path applies, a single cell otherwise (text-shaping.md).
   Shaped runs are memoized.
4. New glyphs land in the glyph atlas; images whose pixels
   changed are uploaded into the image atlas, and every image a
   live placement references after an atlas reset.
5. The renderer issues draw calls inside one render pass, in the
   layered order:
     a. clear (load op) + images z below the bg tier
     b. cell backgrounds (incl. cursor/selection markers, which are
        background-layer instances, not a separate pass)
     c. images z below text but above bg
     d. cell glyphs (incl. box-drawing and hyperlink underlines)
     e. SGR line decorations (underline family, strikethrough,
        overline), drawn over the glyphs
     f. images z>=0
6. Surface is presented at vsync.
```

While a chrome bar (search, confirmation, link preview) holds the bottom row, every quad built from producer-controlled
content (cell glyph, decoration, and image alike, at any `z`) is clipped at that row's top edge before the bar's own
instances are appended. Skipping the reserved row in the grid walk is not enough: a tall bitmap or an OSC 66 run scaled
across rows anchors above the row and reaches into it, and it rides the same foreground pass as the bar's text, where
nothing occludes anything. A bar a producer can paint over guarantees nothing; see
[the security model](../security-model.md) "OSC 8 hyperlinks and OSC 7 CWD" for what rests on that guarantee.

## wgpu surface and resources

### Backend and surface format

wgpu maps to one native backend per platform (Metal on macOS, Vulkan on Linux, DX12 on Windows) while all three share a
single WGSL shader path ([`shader.wgsl`](../../../crates/felis-render-wgpu/src/shader.wgsl)). One codebase, three
platforms; see [the implementation doc](../implementation.md) for why wgpu over a per-platform native renderer.

The surface format is whatever the surface advertises as sRGB-capable first, falling back to its first advertised format
when none is, rather than a hard-coded BGRA8. Hard-coding it is the thing that works on the developer's machine and
fails on a reviewer's: a surface that does not advertise the assumed format either refuses configuration or silently
mismatches the shader's color space. Losing the surface (`Lost` / `Outdated`) reconfigures and retries, because a
compositor may hand one back at any resize; `OutOfMemory` is fatal and the client exits, because there is nothing to
retry with.

### Offscreen rendering

A renderer can also draw into a texture of its own instead of a window surface, for a consumer with no window that reads
each frame back as RGBA, such as a recording-to-video converter or a screenshot test. The format is fixed at
`Rgba8UnormSrgb`, because there is no surface to negotiate one with, and `background_opacity` is ignored, because no
compositor places the result behind anything.

An offscreen renderer has no post-process stage and refuses a configured shader. The stage's time uniforms read the host
clock, and the bundled trail shader paints the whole frame until the client's easing loop has driven the trail state, so
an offscreen frame would depend on when it was rendered rather than on the screen it shows.

_Revisit if_ a windowless consumer needs shader-styled frames; the renderer would then take the frame clock and the
trail state from the caller.

### One instance buffer per draw tier

Everything downstream of the surface is per draw tier: one geometrically grown, frame-pooled instance buffer for each of
backgrounds, glyphs, decorations, and the three image z tiers, over one shared unit-quad vertex buffer. A tier is a
separate draw and its instances have to be contiguous, so the tiers cannot share a buffer.

Pipelines are fewer than tiers: backgrounds, glyphs and decorations get one each, and the three image tiers share a
single image pipeline, since they differ in when they are drawn and in nothing the GPU state has to know about. The
argument for the tiers themselves, and for the atlases they sample, is in "Prior art and alternatives considered" below.

There is no separate cursor pipeline: the cursor and the selection are background-layer instances, since both are
rectangles behind text and a pipeline of their own would buy a draw call. Both cover a wide character's two cells
together, from either half, or the glyph drawn across them would be half on the highlight and half off it.

The cell tiers hold one fixed-size slot per grid row, so a frame writes only the byte ranges of the rows it rebuilt,
except while the rows are packed because the slots would not fit ([damage-tracking.md](damage-tracking.md) "Client
repaint"); a buffer in one of the unified-memory rings below takes every row rebuilt since it was last written.

### Unified-memory uploads skip the staging copy

On a unified-memory GPU nothing a frame uploads goes through a copy. The instance buffers and the uniform blocks are
rings of three CPU-mappable buffers written in place, and glyph and image bitmaps are drawn into their atlas by a render
pass that reads the texels from a mappable storage buffer, instead of `Queue::write_buffer` and `Queue::write_texture`.

Both of those stage the data and copy it with a blit, and Apple's GPU driver charges the process a blit working set of
about 160 MB, on top of about 240 MB for render passes, until a second passes with no blit. A blinking cursor or a
workload that meets new glyphs keeps that set resident (measured on an M4: a focused, blinking, otherwise idle window at
424 MB against 264 MB, a sustained flood peaking at 460 MB against 302 MB). Uploading through a render pass adds only
draws the frame's working set already covers.

A discrete GPU keeps the staged path, since a mappable buffer there lives in host memory the GPU reads across the bus.

_Revisit if_ wgpu writes unified-memory resources without a staging copy, or the driver stops holding a working set per
encoder type.

### Image quad sizing

Each placement is one textured quad, bilinear-sampled from the atlas. A _naturally_ sized placement (`c=`/`r=` absent)
is drawn at the image's **native pixel size**, not stretched to the rounded-up cell box. The daemon resolves natural
sizing to a cell count of `ceil(px / cell)` for cursor and clip bookkeeping, so the renderer would otherwise blow, say,
a 1690-px mpv frame up to 212×8 = 1696 px: a non-integer resample that leaves a faint **vertical seam** wherever a
high-contrast edge crosses a half-texel boundary. It is invisible on flat color and a visible line on video, and
vertical because a producer lands the height on a cell multiple but not the width.

The renderer detects "the box is exactly `ceil(native)`" and collapses to native pixels, matching kitty's "natural size
in pixels"; the last partial cell shows a few px of background, as kitty's does. An explicit `c=`/`r=` asking for a
_different_ box is scaled to fill it, since there the producer has said what it wants.

Box-drawing and block-element glyphs (U+2500–U+259F) take the opposite treatment for the same reason: they are
CPU-rasterized to the exact cell box rather than scaled from a font outline, so adjacent rows' lines connect instead of
landing a fraction of a pixel apart.

## Synchronization with the daemon

The shadow screen is the client's local copy of what the daemon believes is on screen.

When a producer enters synchronized output mode (`CSI ? 2026 h`), the daemon holds diff emission: dirty rows accumulate
and nothing ships until the matching `CSI ? 2026 l` or a 150 ms deadline (`Grid::SYNC_OUTPUT_TIMEOUT`) flushes them, so
a producer that crashes mid-update cannot freeze the screen. The client carries no synchronized mode of its own: it
renders whatever arrives, and a synchronized update arrives as one batch.

This guarantees that, e.g., a TUI rewriting a dozen rows in a single update appears as one frame, not as a partial
paint.

## Frame pacing

- Default: redraw on every IPC update that touches a visible cell, at most once per vsync.
- Cursor blink and animation: an internal timer requests redraw at the blink / animation rate.
- Idle: zero redraws. The renderer does not poll.

This paces _rendering_. The daemon→client _emission_ side is paced by the same clock, pulled across the IPC boundary:
the client's vsync is the only sampling clock anywhere; the daemon never owns a frame rate. This mirrors the Wayland
frame-callback model (`wl_surface.frame`, <https://wayland-book.com/surfaces/frames-events.html>; foot renders this way,
<https://codeberg.org/dnkl/foot>).

### Demand-driven emission

#### A pull per painted frame

The daemon advances the grid as PTY data arrives (the producer blocking on a full PTY buffer is the desired
backpressure, and is preserved), but emits a grid diff **only when a client pull is pending**. The client sends
`InputMsg::NextGridFrame` on the existing input wire once per painted frame, at the end of the `RedrawRequested`
handler, so winit's vsync paces the pulls; a per-connection `pull_pending` flag in the daemon's serve loop records that
the next dirty cycle should emit.

The unit that answers one pull is a **cycle**, and the daemon states its end on the wire with `GridMsg::CycleEnd`
([ipc.md](../../reference/ipc.md) "Grid (kind = 2)"), which lists what a cycle contains. The client holds both its
outstanding pull and its grid paint until that marker, so one outstanding pull denotes one complete cycle and no redraw
(pending, blink, animation, facet push) can paint a `Scrolled` without the rows and cursor behind it.

Inferring the end from whichever message arrived first is the alternative, and it cannot hold either property: almost
every compose variant is a plausible first message, so a split row batch or a registry entry ahead of its rows would
release the pull and paint a screen the daemon was still describing. The rehydrate burst is a second boundary rather
than a cycle: it is eager, answers no pull, and ends at `RehydrateEnd`.

The pull is not armed from `about_to_wait`, which only flushes the coalesced redraw request: during a burst it runs once
per inbound frame, far faster than vsync, and pacing there would degenerate into a request-reply loop that floods the
daemon and starves the PTY drain. The pull is payload-free: the daemon tracks no per-client send position, and reattach
replays a full snapshot.

#### A frame that fails to present sends no pull

A frame counts as painted only once it is presented: a frame whose surface acquire fails sends no pull. A window that
cannot present (on macOS, an occluded window, or any window on a locked screen or a sleeping display) fails the acquire
at once instead of waiting for vsync, so pulling behind it would decode cycles as fast as the acquire fails and paint
none of them.

The window instead retries its redraw on a timer that starts at the refresh interval and doubles after each failure up
to one second, or at once when winit reports it visible again (`WindowEvent::Occluded(false)`), and its first presented
frame sends the one pull that the daemon answers with everything dirty since. Between retries every other redraw source
(a blink, a post shader's clock, an eager-push frame) waits for the next retry. The doubling is what keeps a window
hidden for hours near idle-zero: retrying at the refresh rate would run the frame's CPU preparation at that rate for as
long as the window stays hidden.

#### Emission coalesces; the parse runs decoupled from it

Everything accumulated since the last emission coalesces: the shifts of a band into one `Scrolled`, and the rows written
since into one `RowDelta` batch ([ipc.md](../../reference/ipc.md); the marks that make the two agree are
[damage-tracking.md](damage-tracking.md) "From damage to the wire"). A band shifted by its whole height drops its
directive and ships as rows, so the per-pull cost is bounded at ~screen-height rows no matter how many rows scrolled in
between: ~150 KB/s and <1% CPU even at 144 Hz.

The grid advance is fully decoupled from emission: the parser runs on the PTY **parse thread**
(`felis_daemon::parse_sink`; see [implementation.md](../implementation.md) "Tokio (multi-thread)"), so it drains the PTY
at full speed regardless of pull cadence, and the child blocks on `write` only when the parse falls a full swap buffer
behind, the desired backpressure. The owner's async task only replays the parsed grid effects and composes diffs, gated
to emit on a pending pull.

Because the parse is not on a Tokio task at all, a parse-bound flood cannot keep an always-ready read arm monopolizing a
runtime worker and starving the connection pump: the screen scrolls smoothly even when the parser is the bottleneck,
with no scheduler-yield workaround required.

Decoupling advance from emission means the diff must compose correctly across many intervening scrolls: the
`pending_scrolls` accounting in [ipc.md](../../reference/ipc.md) has to survive the gap, not just one tick.

#### The effect replay coalesces to a ~4 ms window

Because the parse-thread sink and the async task share the grid behind one lock, the async task must not re-lock on
every ~1 KiB chunk, or the two threads ping-pong the lock and throttle the parse, the pace that (via the swap-buffer
cap) bounds the child. Under a sustained flood the async task therefore **coalesces** its effect-replay to a ~4 ms
window (well inside a frame), leaving the lock to the sink between drains; an isolated update after a lull (a keystroke
echo) drains immediately, so echo latency is unchanged.

The window is counted from the end of a drain, not its start. A drain waits out the sink's hold on the lock, which under
a flood is about as long as the window, so a window counted from the start has elapsed by the time the drain returns and
the task drains again without yielding. The runtime worker it occupies then neither polls the connection socket nor runs
the outbound pump the cycle woke, so the next pull sits unread while the window stops repainting.

#### A fair lock hands the grid to the owner task

Coalescing keeps the async task off the lock between drains, but when it does ask, the lock must hand off to it. Under a
flood the sink relocks as soon as it unlocks, once per swap buffer. `std::sync::Mutex` (a futex lock on Linux) lets the
thread that just unlocked take the lock again before a woken waiter runs, so the async task cannot reliably acquire the
grid to answer a pull. The client holds its next pull until `CycleEnd`, so one unanswered pull stops emission: the
window stops repainting while the child keeps writing.

The grid therefore sits behind `parking_lot::Mutex`, whose eventual fairness forces a hand-off to a waiter on average
about every 0.5 ms (<https://docs.rs/parking_lot/0.12/parking_lot/type.Mutex.html>). That is an average, not a deadline,
and liveness is all a pull needs.

In a pull-paced window on Linux, a 10 s DOOM-fire flood gets 3 pulls answered behind the std lock and 161 behind the
fair one, while DOOM-fire's frame rate (1,120 against 1,095 fps) and a 150 MB `cat` stay within run-to-run noise.

#### The owner task takes the lock once per cycle

A fair lock bounds how long each acquisition waits, not how many acquisitions a cycle makes, so the owner task takes the
lock **once per cycle**. The effect replay, the damage fan-out, the facet collection, the compose for every pull-pending
subscriber, and the check whether a `DECSET 2048` resize report is owed all run under one guard. The pushes into the
subscribers' outboxes run after the guard drops, because an outbox that has died evicts its subscriber, and the eviction
reads the grid again (a cycle with an eviction therefore takes the lock a second time, as does one that owes the resize
report, whose write reads the grid for the geometry it states).

Letting each step take the lock for itself makes a pull wait out one fairness slice per step, about six per answered
pull. In a pull-paced window on Linux (a 188x82 grid on a 60 Hz output) under a 16 s DOOM-fire flood, that answers 12 to
14 pulls per second against 50 to 53 with one acquisition per cycle and the coalescing window counted from the drain's
end, while DOOM-fire's frame rate stays within run-to-run noise.

#### Rejected ways to keep the pull answerable

- **`MutexGuard::unlock_fair` after every sink chunk.** A strict hand-off on each swap buffer the async task contends,
  paying a context switch per chunk on the throughput path where eventual fairness pays one per ~0.5 ms.
- **A yield or sleep in the sink after each chunk.** A constant tuned to one machine's scheduler, paid as parse
  throughput on every chunk whether or not a pull waits.
- **A client-side deadline that paints without a cycle.** The daemon has nothing composed while it cannot take the lock,
  so the forced frame repaints the same screen.
- **A fair unlock in the sink only while a pull is pending.** Shortens each wait but not the number of waits a cycle
  makes, and one acquisition per cycle leaves it little to shorten.
- **Composing off the shared lock** (a grid snapshot or double buffer). Removes the contention instead of arbitrating
  it, but reshapes the parse/compose boundary for a liveness problem a fair lock already solves.

_Revisit if_ a flood still starves pulls behind the fair lock on a platform whose parking backend differs (macOS,
Windows), the one acquisition per cycle still leaves pulls waiting on the lock, or compose under the lock grows long
enough that the forced hand-offs measurably cut parse throughput (the snapshot shape is then the next step).

#### Client demand is the only clock that is correct everywhere

- The daemon is headless; the refresh rate exists only on the client (winit reports it via
  `MonitorHandle::refresh_rate_millihertz`,
  <https://docs.rs/winit/latest/winit/monitor/struct.MonitorHandle.html#method.refresh_rate_millihertz>). No daemon-side
  rate can serve 60, 120, and 144 Hz panels at once.
- Variable-refresh displays (ProMotion / G-Sync / FreeSync) have no fixed rate; only a live per-frame pull can track
  them.
- One daemon serves several clients; each connection paces independently through its own `pull_pending` gate, and
  dragging a window between monitors changes its rate for free (a monitor change is a resize; see below).
- An occluded or minimized window stops pulling, so the daemon stops emitting: idle-zero-redraw holds across the IPC
  boundary.

Pull is request→reply, so a bulk grid update can cost up to one extra frame of latency; that is acceptable for bulk
output, and the input-echo path is independent and unaffected. `NextGridFrame` rides the never-dropped `input` channel,
so it must stay cheap and coalescible to avoid crowding real input.

#### Push fallback

Pull pacing is stated by `Hello.pull_paced` ([ipc.md](../../reference/ipc.md) "Versioning"); the GUI client states it by
default, while cross-host (ssh) attaches stay on eager push. For those peers the PTY drain is bounded by a small
daemon-side time slice so a burst cannot starve diff emission indefinitely. That time slice is the only daemon-side
interval anywhere, and it never claims to match a refresh rate.

#### Rejected pacing models

- **Unbounded eager push.** The daemon drains every queued PTY chunk before emitting anything; a producer faster than
  the parser (~14.7 MiB/s end-to-end vs the bare parser's multi-GiB/s) starves emission for the whole burst, so an 11
  MiB `cat` freezes for ~0.75 s and lands as one jump instead of a scroll. Fine emission is not the cost (`Scrolled`
  bounds each diff to the screen height); the missing piece is a pacing clock.
- **Daemon fixed-interval timer (16 ms ≈ 60 fps).** Bakes a frame rate into a refresh-rate-blind component: wrong for
  120/144 Hz, impossible for VRR, one rate for all clients, keeps emitting to hidden windows, and its phase beats
  against real vsync.
- **Daemon push at a fixed high cap; client coalesces down.** Cannot exceed the cap, wastes encode for low-refresh or
  occluded clients, and is still a daemon-owned rate that cannot represent VRR.
- **Client declares its refresh interval at handshake; daemon paces push to it.** Goes stale on every monitor change
  unless re-sent, cannot represent VRR, and still runs a daemon timer whose phase beats against vsync: strictly worse
  than pull, where the client is already the clock.
- **Client-side scroll-rate heuristic** (slow the apparent scroll so it is readable). Behavior driven by the _content_
  of shell output, which [principles.md](../principles.md) Principle 4 forbids; readability of fast output is the
  pager's job.

_Revisit if_ a non-interactive consumer (recorder, relay, headless test client) that has no vsync to pull with becomes
common (it would have to drive its own cadence or use the push fallback, which might then deserve to be first-class);
`NextGridFrame` at high refresh rates measurably crowds genuine input on the shared `input` channel (would justify a
separate logical channel or priority); a frozen or stuttering GUI is reported on a specific refresh rate / compositor
(no env escape hatch back to eager push exists, so one would need adding); or winit/wgpu expose per-frame VRR deadlines
to pace against more precisely than one pull per painted frame.

## DPI and multi-monitor

- The window asks winit for a logical-to-physical scale factor and configures cell metrics from it.
- A monitor change (drag the window to another display) is a resize event. The client recomputes cell metrics and asks
  the daemon to resize the grid (`Resize` over IPC).
- Glyphs rasterize with grayscale anti-aliasing on every platform; there is no subpixel (LCD) anti-aliasing.

## Window transparency and OS backdrops

Two window-chrome knobs ride on the wgpu surface; the keys, their per-OS material vocabularies and their failure rows
are in [config.md](../../reference/config.md).

### `window.opacity` is background opacity

`window.opacity` is _background_ opacity, not whole-window opacity (the kitty `background_opacity` model): only the
default background bleeds through, and text and cells a program paints with an explicit background color stay solid. The
OS compositor does the blending, and felis adds no in-terminal blur of its own, because on Linux blur belongs to the
compositor and duplicating it there would fight the WM. It is decided at startup because the surface is: crossing the
opaque↔translucent boundary needs a relaunch, not a live reload.

### One backdrop key, per-OS materials

The OS-native backdrop is one key, `window.backdrop`, whose materials are per-OS. The user's intent (frost the window)
is one intent, so it gets one key; only the vocabulary of materials is platform-specific, because the OSes name theirs
differently.

A key per platform (a macOS blur bool beside a Windows material enum) is the rejected alternative, and it buys nothing.
Two such keys are mutually exclusive in practice, since no window hosts both, so the pair can spell a state the domain
does not have; and a user who finds one of them still has to go looking for the other to learn what their own OS honors.

Nor does the split buy validation, because neither key can be checked against the running OS at parse time: one
`config.toml` is read on all three platforms, which is why a material the running OS does not provide is inert and logs
a note.

A _unified material taxonomy_ stays rejected from the other side: `mica` and `acrylic` are Windows terms with no macOS
equivalent (AppKit exposes a different set), so folding them into shared names like `light` / `heavy` would misdescribe
both.

How the two keys couple splits per material, which is why `blur` is not simply the macOS spelling of the same thing.
macOS vibrancy shows only _through_ a translucent surface, while the DWM backdrops always paint the OS-drawn caption
whatever the body opacity. "Title-bar-only glass" is therefore a first-class combination on Windows rather than an
accident, and it is the only one there: the DX12 swapchain advertises no translucent alpha mode, so a translucent body
renders opaque with a warning, and whole-window frost needs a DirectComposition surface path.

No value is a blur-strength number. Both platforms are driven through the window-vibrancy crate's wrappers over the
public OS APIs, and those APIs fix the blur by preset with no numeric radius; the only numeric knob on macOS is a
private SPI felis refuses to call.

_Revisit if_ winit grows a first-class vibrancy/backdrop attribute (map the key onto it); a portable Wayland blur
protocol stabilizes (add a Linux material); a public macOS radius API appears (a strength would need a companion key,
since `backdrop` names materials); or a consumer needs the DWM caption _colors_, which is a separate FFI surface not
covered here.

## Cursor trail and user post-process shaders

felis has one optional post-process stage: when `shader.post` names a shader, the cell pass renders to an offscreen
texture instead of the swapchain, and a fullscreen pass samples it back through a user-replaceable fragment shader
driven by a versioned uniform contract. The first consumer is a **cursor trail**, shipped as a bundled WGSL shader on
that contract; a user may substitute their own. There is no built-in-only code path: the bundled trail exercises the
same contract users get. With no shader configured the stage does not exist and the pipeline stays single-pass. The
normative surface (the resolution rules, the entry point, and every uniform) is the reference twin,
[shaders.md](../../reference/shaders.md); this section holds the argument.

### Relation to the evaluator ban

Principle 1 rejects any runtime evaluator inside the felis process ([principles.md](../principles.md)), and
[design.md](../design.md) "Composition across a process boundary" states what the ban guards: logic that runs _because_
the terminal did something belongs in a separate process. That is a control-plane boundary. A post-process shader sits
outside it: it is a pure function from typed uniforms and the rendered frame to pixels. It cannot emit actions, cannot
observe anything beyond its uniform block, and has no I/O. The only in-process work performed on user input is naga
validation, the same bounded pass felis's own shader goes through (`felis-render-wgpu`'s shader-validation test).

The user's code executes on the GPU, where non-termination is contained by the driver's watchdog reset (e.g. Windows
TDR, <https://learn.microsoft.com/en-us/windows-hardware/drivers/display/timeout-detection-and-recovery>): WGSL has no
recursion and no unbounded memory, but it can loop, so the containment is the driver's, not the language's. The matching
carve-out on the scripting non-goal is recorded in [non-goals.md](../non-goals.md) "Scripting and extensibility".

### The shader contract

The uniform struct, the bind-group layout, and the fragment entry point form a public, versioned surface: additive
evolution only. WGSL makes that natural, because a pipeline accepts any bound buffer at least as large as the declared
struct's minimum binding size (<https://www.w3.org/TR/webgpu/#minimum-buffer-binding-size>), so new fields append at the
tail and an existing shader keeps validating against a newer felis.

The field set ([shaders.md](../../reference/shaders.md) lists it) is chosen around two target uses. Mouse fields are
first-class because input-reactive presentation (a highlight following the pointer during a talk) is a target use, and
the full 256-color palette lets an effect track the user's theme instead of hard-coding colors. The accepted format is
WGSL text only because it is naga's first-class frontend and stays readable in a config review; naga's SPIR-V frontend
is second-class and is not accepted.

Source compatibility with kitty or ghostty shaders is a non-goal, but the uniform vocabulary those two converged on
independently (cursor current/previous geometry, cursor change time, time and frame, theme colors) is kept semantically
aligned, so a port is a mechanical GLSL-to-WGSL rewrite plus a uniform rename.

### Idle-zero under animation

The frame-pacing invariant above (idle: zero redraws) is this design's binding constraint, and the two prior arts sit on
either side of it. kitty animates custom shaders through a dedicated event machinery (`animation_start` /
`animation_stop` directives with easing curves; kitty `docs/custom-shaders.rst`). ghostty instead runs a continuous draw
timer whenever a custom shader is loaded and the window is focused (`custom-shader-animation` in ghostty's
`src/config/Config.zig`; the timer in `src/renderer/Thread.zig`), because the host cannot tell when a Shadertoy-style
shader has visually finished: the shader is a pure function of elapsed time, and only it knows its own duration.

felis closes that gap by keeping the animation state on the CPU, where the host can see the motion end. The trail's
eased corners are fields of the uniform contract rather than something the shader integrates
([shaders.md](../../reference/shaders.md) "Animation and the frame clock" states the easing model and the threshold that
arms it), and its redraw clock has the cursor-blink shape: armed by a cursor move, self-terminating once the corners
settle, and unarmed by the short moves ordinary typing makes. Mouse-reactive shaders redraw per mouse event, which is
input-driven, so idle still means zero frames; a session with no shader configured pays nothing at all.

That covers every effect whose motion felis can see the end of. It does not cover the other kind, and the prior art's
shaders show the gap: kitty's water caustics are a pure function of `time_s`, and under the rules above they hold still
on an idle window and step at the cursor-blink rate on a busy one, which reads as a bug rather than as a policy.

`shader.animate = "focused"` is the lever for that case: while a shader is loaded and the window holds focus, the client
redraws at 60 Hz. It is off by default, so nothing changes for a session that has not asked, and the _user_ asks, in
their own config: the contract still has no field through which a shader can request frames, because a shader that could
turn on a redraw loop by being installed would put the invariant back in the shader author's hands.

There is deliberately no unfocused variant (ghostty's `custom-shader-animation = always`). A background window burning a
core is the case the invariant most obviously exists to prevent, and the effect is not on screen to be watched.

The SGR-blink rejection below rests on who triggers the flashing: a remote program can set blink on content, and the
user has no lever. A shader flashes only if the user installs one that does, in their own config. The two decisions
share the axis and do not conflict.

_Revisit if_ a real consumer needs an animated window while it is not focused: a screen recording is the plausible one.

### Loading and failure

The config names each shader explicitly and says which kind it is, rather than covering both in one string classified by
its own shape (a bare name resolving under a `shaders/` subdirectory, anything with a path separator taken as a path,
one reserved word meaning the bundled trail). That form is rejected on two counts: it is content-sniffing on a value the
user wrote, which principle 4 rules out ([principles.md](../principles.md)), and the reserved word costs a capability,
since a file named `trail.wgsl` becomes unreachable and a shader sitting in the config directory cannot be spelled as
the path it is. What it buys, typing `"glow"` for a file under a directory felis picked, does not pay for either.

A shader that fails validation drops the post-process pass rather than falling back to the bundled one: substituting a
cursor trail for the effect the user asked for is a silent change of behavior, where an absent pass reads as "your
shader did not run". Validation runs before the device sees the source, so the window keeps painting either way; the
failure rows are in [shaders.md](../../reference/shaders.md) "Failure handling". ghostty's failure mode is the
counterexample: an invalid shader can black the window and the error reaches only the log (ghostty
`src/config/Config.zig`, the `custom-shader` doc comment).

### Rejected alternatives

- **In-process runtime compilation of a source language.** kitty bundles `slangc` and compiles Slang at startup
  (`kitty/shaders/slang.py`); ghostty bundles glslang plus spirv-cross and compiles Shadertoy GLSL on load
  (`src/renderer/shadertoy.zig`). Either shape brings a compiler toolchain into the felis process for a job the user can
  run offline; accepting WGSL directly needs no dependency wgpu does not already carry (naga).
- **A closed enum of built-in effects, no user shaders.** A smaller surface (no user file, no contract), but it reopens
  this decision for every new effect and delivers none of the customization that motivated the feature. With a contract,
  the decision is made once and effects become data.
- **A continuous animation loop as the default, or with no way to turn it on** (the former is the ghostty model). Both
  extremes are wrong: running the loop whenever a shader is loaded charges every user for what few effects need, and
  offering no loop at all leaves the whole Shadertoy-shaped body of prior art broken in a way the user cannot fix.
  `shader.animate` splits them: default off, opt-in per config, focus-gated (previous section).
- **Source compatibility with kitty `.slang` or ghostty Shadertoy GLSL.** Would chain the contract to two moving
  external vocabularies and require exactly the in-process compilers rejected above. Semantic alignment of the uniform
  vocabulary is kept instead.

_Revisit if_ a real consumer needs the vertex stage, multi-pass composition, or a persistent texture (kitty's
`.pipeline` groups and `persist` texture are the shape to study); Slang's WGSL target leaves experimental status and
offline-compiled Slang becomes the convenient authoring path (still offline; the in-process ban stands); or naga stops
being the validation layer wgpu ships with.

## Performance principles

- **No allocation in the inner draw loop.** All buffers are pooled, including the scratch that images are converted
  through, because a video producer re-uploads a multi-megabyte frame 24-30 times a second.
- **Uploads are deferred to one flush point per frame.** A glyph rasterized during the populate walk is queued and
  written after the walk, one upload per new entry, so the walk never interleaves texture writes with reads of the
  packer.
- **Damage decides what the daemon ships and how much of a frame the client rebuilds, not whether the client paints.**
  On the client, anything that changes sets one sticky redraw request, and the next frame rebuilds the rows the shadow
  marked ([damage-tracking.md](damage-tracking.md)). felis does not skip frames for energy reasons (a screen with a
  blinking cursor still ticks).

## Prior art and alternatives considered

felis's rendering and atlas choices are mostly the modern-terminal consensus; this records the alternatives and why
felis landed where it did. (Frame-pacing alternatives are covered inline above, under "Rejected pacing models".)

### Cell drawing: instanced quads, not per-cell draws or compute raster

One instance buffer of ~32-byte cell records drawn over a unit quad is what Kitty, Alacritty, WezTerm, Ghostty, and foot
all use: a constant 1–2 draw calls regardless of grid size, over a linear GPU-friendly buffer.

The alternatives are worse at terminal scale: a draw call per cell (old xterm backends) collapses past 80×24, and
compute-shader glyph rasterization (a WezTerm experiment) wins nothing because the bottleneck is the parser and shaper,
not rasterizing. The atlas region is looked up from a `glyph_id` table rather than stored in the instance, keeping the
per-cell record flat.

### Layered draws, procedural decorations

One render pass issues layered draws through four pipelines (bg, fg, decoration, image, with images interleaved by z).
Backgrounds draw in a separate tier from glyphs because backgrounds need full cell coverage while a glyph's outline (an
ascender, a ligature, a wide emoji) may exceed the cell box; felis rasterizes pixel-aligned, so this is glyph overshoot,
not sub-pixel positioning.

SGR underlines (five styles: single, double, curly, dotted, dashed), strikethrough, and overline ride a dedicated
decoration pass drawn after the glyphs, computed procedurally in the fragment shader (a sine wave for curly, a modulo
for dotted/dashed) rather than stored as atlas sprites, which would multiply with every sized-run width. Kitty does the
same for curly underlines.

`FAINT` dims the foreground toward the background and `CONCEAL` drops the glyph entirely: a concealed cell (a shell's
password echo) must leave no ink, so conceal is a rendering guarantee, not a cosmetic attribute.

### SGR blink is never drawn

The one SGR attribute the grid tracks but the renderer deliberately never draws is **blink** (SGR 5/6): the grid parses
and ships the flag for fidelity, but rendering a blinking cell is rejected on accessibility grounds: flashing content is
a photosensitivity and distraction hazard, and unlike the cursor's blink there is no lever to stop program-driven text
blink once a hostile or careless program sets it.

_Revisit if_ a user-controlled "honor SGR blink" opt-in is asked for; it would default off.

### Atlases

#### Glyph atlas: a first-fit shelf packer at a fixed size

Glyphs are cell-sized, so felis shelf-packs them into an `R8` mono sheet of `min(2048, max_texture_dimension_2d)` per
side, floored at 256 because no useful shelf fits below that. The ceiling is 2048 rather than the image atlas's 8192
because a sheet that size costs 4 MiB against 64 MiB to hold the same thousands of one-cell slots. Color glyphs cost
four bytes per pixel, so they take a second, `RGBA8` sheet of `min(mono side, 1024)` per side, which at half the mono
side costs the same bytes and still holds hundreds of emoji.

Alacritty and Ghostty pack the same way; guillotine and max-rects (WezTerm) buy a density that a grid of one-cell boxes
does not need. There is no per-glyph eviction, and neither sheet grows: both are recycled whole (on a font / size / DPI
change, and on a shelf that fills mid-frame) rather than leaving the new glyph permanently unplaced (and invisible).

A full sheet bounds the slot map only as tightly as the glyphs are large. The output picks the slot keys (every
character in every OSC 66 sizing and style is a separate one), and a producer walking fractional sizings rasterizes each
character to a pixel or two, millions of which fit one sheet. So the sheets also count as full once 65 536 placed slots
exist, and recycle the same way. The cap is twice the one-cell slots of an 8×16 cell that a 2048² sheet holds, so text
at a readable size fills the sheet before it reaches the count, and the map's table stays under 8 MiB, spare hash
capacity included. A per-entry eviction is not an option: the shelf packer frees no single slot, so dropping an entry
would leave its pixels allocated.

The slot map also remembers which glyphs rasterize blank, so a screen of spaces does not rasterize again every frame.
Those entries need a bound of their own: a blank takes no sheet space, so no recycle clears them, and the output picks
the key (each OSC 66 sizing and style of a space is a separate one). Past 4096 blank entries the map drops all of them
at once and keeps the placed slots. Dropping one cannot change a frame, since a missing slot and a blank one both draw
nothing; the next walk that meets the key resolves it again.

A mid-frame reset invalidates the slots the frame's populate walk has already handed out, so the walk runs a second time
before instances are built. Without it those cells paint blank and stay blank: the client repaints only when something
asks it to, so nothing revisits them until unrelated traffic arrives. The retry is cheap: the CPU bitmap cache and the
run-shape memo are position-independent and survive a reset, so the second walk re-allocates rather than re-rasterizing.
It happens at most once: a second reset in one frame means the screen's distinct glyphs exceed the whole sheet, which no
number of resets fixes, so the frame keeps its blanks, logs once, and requests a repaint.

Overlay text (a chrome bar's label, the link preview and its clipping mark, the pre-edit) is laid out in the cells the
grid would print it in, and a multi-codepoint cluster in it is shaped the way a cluster cell is, so a flag or a skin
tone draws as itself rather than as its first codepoint. It is primed after the grid walk, since a walk that resets the
sheet would otherwise drop the overlay's slots and leave the bar's label absent for the frame. That ordering makes the
overlay the one thing that can overflow _after_ the grid has its slots, so the reported reset is read once more after
priming and drives the same bounded second walk.

_Revisit if_ CJK-heavy sessions show the sheets churning.

#### No SDF / MSDF

Signed-distance-field text scales without re-rasterizing, but its one win (free zoom) does not apply to a terminal at
one or two fixed sizes; it is also mildly fuzzy next to native-size grayscale rasterization, which terminal users
notice.

Revisit only if Kitty text sizing's 7× max scale produces ugly upscaling.

#### Image atlas: one large, lazily-allocated texture

A single RGBA8 atlas (`min(8192, max_texture_dimension_2d)` per side, a 256 MiB byte cap, shelf packer), allocated on
the first Kitty-graphics upload so an image-free session pays no GPU memory. Kitty's per-size-class atlases reduce a
fragmentation felis accepts; one texture keeps batching simple. The atlas is the client's; the daemon owns the image
bytes and reships them on rehydration.

There is no per-image eviction. A sheet that fills is recycled whole, and every image a live placement references is
re-uploaded before the same frame is built; the client already holds every live image's pixels in its image shadow, so
recovery costs an upload, not a round trip. The alternative, letting the dropped images reappear whenever their producer
retransmits, is not a policy at all: a producer has no reason to re-send an image it already delivered, so those
placements would draw nothing indefinitely. An image too large for even an empty sheet is skipped (logged once);
resetting again would free nothing it could use.

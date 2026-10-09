---
title: Kitty graphics design
sidebar:
  order: 3
---

This page records the architecture and decision record behind felis's Kitty graphics implementation: where the
dispatcher lives and why, the deliberate deviations from kitty, the rejected alternatives, and the edges felis will not
cross. The wire surface (formats, actions, parameters, limits) lives in the
[reference twin](../../reference/protocols/kitty-graphics.md).

felis implements the Kitty graphics protocol as a first-class feature. This is one of the three core value propositions
(see [`vision.md`](../vision.md)).

The authoritative reference is the Kitty specification at <https://sw.kovidgoyal.net/kitty/graphics-protocol/>. felis
aims for behavioral compatibility with that document at the protocol level; where the Kitty implementation has
undocumented quirks, felis follows the documented behavior and treats deviations as bugs in felis.

## Dispatcher architecture

The protocol dispatcher is a `felis-daemon::graphics` module (a free function taking `&mut Session` plus the drained APC
bodies; per body it borrows the session's graphics state as an `ApcCtx`), not part of `felis-grid`. The grid stays
decoder-free, filesystem-free, and tokio-free: hosting the dispatcher there would drag the PNG decoder, `rustix` file
access, and a response-write callback into the one crate the workspace's one-way dependency rule keeps display-only, and
would put per-grid copies of the 256 MiB image budget and the 64 MiB reassembly buffer in the wrong place. Both bounded
buffers co-locate on `Session` so a future per-session memory-budget knob touches one place.

`Grid::apc_dispatch` is a short relay: it queues each body as a `PtyEffect::Apc` on the grid's stream-ordered
side-effect queue, carrying the raw bytes plus the cursor position at delivery time; a trailing DECRC / CUP in the same
`Parser::advance` burst would otherwise move the cursor away from the placement anchor before the daemon drains. This
mirrors the dirty-flag posture the grid already uses for OSC titles / cwd / theme overrides: the daemon polls after
every `advance`; the grid stays passive. Chunk reassembly (`m=1`) also runs at the daemon
(`Session.graphics_reassembler`, capped at 64 MiB per [`security-model.md`](../security-model.md)); the grid keeps only
the payload-free half of that state machine, enough to know when a body completes a command.

That knowledge serves one rule: kitty moves the cursor after a placement inside the parse, so text written after the
image in the same write prints beside it, not under it. The daemon dispatches after the parse, so when a body completes
an `a=T` or `a=p` without `C=1` or `U=1`, `Parser::advance_until_yield` stops right after it and the PTY reader waits
until the session task has drained the queue, cursor move included, before parsing the rest. The wait has no timeout: a
timed-out wait is a misplaced line under load, which is the bug itself; liveness comes from the session task releasing
the reader for good when it exits. Two cheaper shapes are rejected. Moving the cursor in the grid at delivery time needs
the cell box, and with `c`/`r` omitted that box comes from the decoded image's pixel size and the client's cell size,
both daemon-side. Running the dispatcher under the parse lock puts the PNG decoder and the 64 MiB reassembly on the
reader thread's critical section for every APC, not just the placements that move the cursor.

Because the dispatcher's entry holds `&mut Session`, it reaches the image store, the placement table, the response
writer, and `&mut session.grid` through one borrow root: no callback ping-pong, no interior mutability. A
`Sink`-decorator wrapper intercepting `apc_dispatch` in the daemon is rejected: `Session` owns its `Grid`, so the
wrapper would have to split one owned struct into two mutable borrows (interior mutability or an `unsafe` split), both
worse than the outbox relay. The only grid mutation the dispatcher makes is the visible side-effect of a placement
(`a=T` or `a=p`): `Grid::advance_cursor_after_image_placement`, honoring `C=1`. Unicode-placeholder cells land through
the ordinary `print` path: producer-driven cell mutations, not dispatcher writes. That path is the one most modern
image-displaying TUIs drive (yazi, ranger preview, jp2a, presenterm), which is why placeholder cells get the full cell
pipeline rather than a special-cased overlay.

The `U=1` _extent_ (`c=`/`r=`/`z=`) is the one thing the cell pipeline cannot carry: a placeholder cell encodes only an
image id and a tile coordinate, so tile sizing needs the extent replayed on attach, and the daemon therefore records it
in the session's `Placements` beside the anchored entries. Its lifetime is deliberately tied to the image, not to a
placement id: `U=1` producers churn a fresh random image id every render with no delete, so the image is left unpinned
(refcount 0, LRU bait) and the extent record dies with the image in the same `free_image` funnel. A
`VirtualPlacementRemoved` wire message is rejected: with image-tied lifetime the existing `Delete` already tells the
client everything, and the client shadow purges its mirror table on exactly that message. Revisit if a producer appears
that re-uses one image id across renders _and_ expects an extent to outlive an explicit placement delete.

Decoding (`f=24/32/100`, `o=z`) is the daemon's job; the grid stores already-decoded bytes. The decoder is the `png`
crate (<https://docs.rs/png/latest/png/>; `Decoder::set_limits` enforces the zip-bomb budget). PNG is the protocol's
only compressed format (`f=24`/`f=32` are raw), so a single-format decoder is the whole surface. Adding JPEG / WebP
decoders is rejected to keep the format supply-chain surface closed: producers transmit raw RGB(A) or PNG, as in Kitty.
Responses are formatted by a decision-free `format_response` and pushed onto the PTY-master writer channel; the `q=`
check happens at the dispatcher call site so matched ids are recorded before suppression. The animation timer also lives
on the daemon: the async runtime is daemon-only, and a daemon-side clock is what keeps detached sessions advancing (see
[Playback](#playback-rejected-alternatives)).

The grid does not decode graphics commands; it copies each APC body into the effect queue the daemon drains. That copy
is what needs a budget, because a producer chooses both the body size and how many arrive inside a single
`Parser::advance`. The queue admits 64 bodies and 1 MiB per drain, and the body that fills it stops the parse until the
daemon has drained, the same stop a cursor-moving placement takes. Pausing beats both growing the buffer and dropping:
the bound keeps a hostile producer from turning a command flood into daemon memory, and a producer that places an image
one cell at a time (yazi) sends thousands of bodies in one write, none of which may be lost. The bound is a property of
the queue type, not a check its callers remember: the queue holds the APC count it is compared against and mints every
APC entry itself, because a count maintained beside the queue is one that eventually disagrees with it, and a budget
that has drifted off what it names bounds nothing.

Revisit if: a second dispatcher consumer appears (a CLI debug tool or headless renderer would justify extracting a
`felis-image` crate, with the daemon as a thin caller); placement-heavy output shows the per-placement pause in
throughput (batch the drain handshake); or the decoder moves to an isolated worker process per
[`security-model.md`](../security-model.md)'s open question (the outbox pattern already accommodates that: the daemon
forwards bodies instead of decoding inline).

## Error replies on malformed input

A malformed graphics command is answered with an `EINVAL`, not silently dropped. The reason is the producer's read loop:
a program that sets `q=0` (or queries with `a=q`) blocks waiting for the terminal's response, so dropping the command on
the floor hangs it. An `EINVAL` (the same status the action handlers return for a bad control _value_) lets that loop
terminate and fall back. The reference states the two paths that reach this
([reference twin](../../reference/protocols/kitty-graphics.md#error-responses-on-malformed-input)); they differ in what
they can honor. A malformed envelope has no parsed controls, so its reply is id-less and unconditional (there is no `q=`
to read and quiet-0 means "respond"). A reassembly overflow arrives after the head chunk already parsed, so its reply
honors that chunk's `q=` and echoes its ids, matching how the action handlers treat a mid-stream error.

The silence for a non-`G` APC body is the load-bearing edge. felis answers only the Kitty graphics dialect
(`ESC _ G …`); another APC dialect (a `felis-vt` non-graphics APC, or a different terminal's extension a producer probes
for) is not felis's to answer. Emitting a Kitty `EINVAL` into it would inject bytes into a protocol felis does not
speak, which a naive producer could misread as a valid response in _that_ protocol. Staying silent on non-`G` bodies
keeps the error surface confined to the dialect felis owns.

## Shared-memory transfer: deviations from kitty

The `t=s` behavior in the [reference twin](../../reference/protocols/kitty-graphics.md#transmission-methods) deviates
from kitty in three load-bearing ways; each exists because of a real producer, mpv's `--vo-kitty-use-shm`.

**Why the segment is not unlinked per read.** mpv reuses one segment name for every video frame, reopening it with
`O_CREAT`; a per-read unlink forces that reopen to allocate a fresh inode whose `ftruncate` zero-fills the pages before
mpv writes them, so the terminal's next (slower) read catches a zeroed bottom: the "black bottom half" bug. It also
opens an ENOENT race in the gap between the unlink and mpv's recreate that drops frames. Leaving the segment in place
lets mpv reuse the inode at constant size; the copy decouples the producer's lifetime from the image's in the daemon,
and teardown honors the spec's "the terminal must delete the object".

**Why the deferral is bounded and success-gated.** The deferred names are producer-chosen (they arrive on the wire), so
an unbounded set is a set whose size the producer picks. Held for the life of the session, it grows the daemon by a
string per escape sequence; measured, 400k `t=s` commands carrying 226-byte names took one session from 1.9 MB to 51 MB
in thirteen seconds, and none of those names existed: the record happened before anyone asked whether the read had
succeeded. Two properties close it. A name is deferred only once its command returned OK, which is the only outcome that
proves an object was opened, so the cheap version of the attack has to produce real segments, and those the OS already
bounds. And the queue is capped (`ShmDeferral::CAP`, 16 names, LRU), unlinking the name it drops on the way out. The cap
is safe precisely because the deferral exists for _reuse_: mpv re-sends its one name every frame, which refreshes its
slot, so eviction can only reach names a producer has stopped using. A producer that rotates names instead degrades to
kitty's own per-read unlink (the behavior felis deviates from) rather than to unbounded memory. The residue is a `t=s`
command that opened its object but then failed to store the image: felis will not unlink that one at teardown. It
belongs to a same-UID producer that can delete it itself, and the OS bounds the total, so the trade is worth the far
larger hole it closes.

_Revisit if_ a real producer legitimately rotates more than `ShmDeferral::CAP` live segment names at once: the symptom
would be that producer seeing its own segments disappear mid-stream, which means raising the cap, not removing it.

**Why the leading slash is restored off Linux.** mpv creates `/mpv-kitty-<ptr>` yet transmits the base64 of
`shm_path + 1` (the slash stripped). On Linux glibc maps a slashed and slash-less name to the same `/dev/shm` inode, so
the stripped name opens as-is. On macOS/BSD the POSIX shm namespace is _literal_ (`shm_open` requires the slash and
`/foo` ≠ `foo`), so a terminal that opens the name verbatim (kitty's `safe_shm_open` does, so mpv's `--vo-kitty-use-shm`
is itself broken under kitty on macOS) misses the producer's object entirely and the video stays blank. felis restores a
missing leading slash off Linux (`normalize_shm_name`), so it opens mpv's real segment and plays where even kitty does
not.

**Why trailing padding is trimmed.** mpv sends raw (`f=24`) frames with no `S=`, sizing the segment with
`ftruncate(w*h*3)`. Linux tmpfs makes that object _exactly_ `s × v × bpp`, but macOS rounds an shm object up to the page
boundary, so the bytes read back are longer than the frame. felis accepts a raw payload `≥ s × v × bpp` and keeps the
leading `s × v × bpp`; kitty does the same (`mapped_file_sz < data_sz` is the only rejection). A _short_ payload stays
an `EINVAL` (an under-declared frame). Without this, every macOS `t=s` frame would be rejected even once the segment
opened.

**Why the byte-copy is platform-split.** Linux uses `pread` (no `unsafe`, and on a concurrent `ftruncate`-shrink it
merely short-reads); macOS uses `mmap` because Apple shm fds reject `pread`. The `mmap` path is an audited `unsafe` site
in `felis-daemon` and carries the same shrink-`SIGBUS` caveat kitty has on every platform (mpv's constant-size reuse
never triggers it).

## File-path allowlist (`t=f`)

`t=f` opens any absolute path the daemon's UID can read; the root allowlist [`security-model.md`](../security-model.md)
prescribes (default: the user's runtime dir) is not enforced. The reason enforcement can wait: the daemon runs under the
user's own UID, so its filesystem reach equals the producer's, and the checks that _are_ enforced (absolute path, no
`..`, `O_NOFOLLOW` on the final component, `t=t` opened by `openat` under a pinned parent, regular files only) close the
races rather than the reach. Revisit when the allowlist config key lands (enforce it in the same change), or if a
producer routinely feeds paths outside the runtime dir and the equal-reach argument stops covering the exposure.

## Windows takes `t=d` only

`t=f`, `t=t` and `t=s` are rejected on `x86_64-pc-windows-msvc` with `ENOTSUP`. This is recorded as an **allowed
platform shim**: terminal protocol compatibility may carry OS-specific transmission differences, because the protocol's
own fallback covers it. A producer's `a=q` probe runs the real decode path, so it learns the gap before transmitting and
drops to `t=d`, the same negotiation it already performs against a terminal that never implemented the file methods.
Nothing a user sees is lost; only the copy that `t=s` avoids comes back.

Two properties of the rejection carry the weight:

- **`ENOTSUP`, not `EIO`.** `EIO` is felis's retryable class: a missing path, a busy file, a racing producer's own
  cleanup. A producer that reads `EIO` may reasonably try the same transmission again, and on Windows that loop can
  never terminate. `ENOTSUP` says the method is absent, which is the fact.
- **It is not a stub awaiting a port.** The Unix readers are built on `O_NOFOLLOW` opens (under a pinned parent
  directory with `openat` for `t=t`) and POSIX shared memory; a Windows equivalent is a second security-sensitive
  file-read implementation to write, audit and keep in step ([security-audits.md](../../reference/security-audits.md)
  "`O_CLOEXEC` + `O_NOFOLLOW` audit (standing)" records the Unix sites it would have to match), and no producer has
  asked for one. Carrying the shim costs three `const fn`s.

_Revisit if_ a real producer needs `t=f` / `t=t` / `t=s` on Windows; demand is the trigger, not infrastructure. The
Windows runtime gate ([workspace.md](../../reference/workspace.md#build-and-platform-matrix)) is a prerequisite for
acting on that trigger, since the port would need a gate to prove it; it is not itself a reason to do the work.

## Frame storage: pre-coalesced

Kitty stores each animation frame as its transmitted rectangle plus a base-frame reference, coalescing lazily at render
time with a capped reference chain. felis instead stores **coalesced** frames (each frame a complete `width × height`
buffer, flattened by the daemon before storage) because it keeps `felis-grid` decode- and compose-free, lets the
renderer treat every frame as "upload this pixel buffer to an atlas slot" keyed by `(id, frame)`, and keeps the wire a
dumb pixel pipe with no base-frame reference graph for a cross-host or non-Rust grid consumer to replicate: the geometry
alone states how many bytes each frame's transfer must deliver. The cost is memory: N frames cost N full canvases even
when each `a=f` touches a small rectangle; the per-session image byte cap bounds it (storage details in
[`image-store.md`](../data-model/image-store.md)). Revisit with lazy coalescing, as kitty does, if a real producer (say
a spinner editing a tiny region across hundreds of frames) is starved by the cap where kitty would not be.

The rectangle compositor (`a=f` base + rectangle, `a=c` region copy, `C=` replace vs alpha-blend) is
`felis-daemon::graphics::compose`, beside the decoder and reusing its zip-bomb budget; the grid never composites.

## Playback: rejected alternatives

The daemon owns the animation clock; the client only swaps to the frame the daemon names (wire details in the
[reference twin](../../reference/protocols/kitty-graphics.md#playback-and-wire-protocol)). Two cheaper-to-build
alternatives are rejected. Re-shipping the composited current frame each tick under the image id needs zero client
changes, but it puts a full canvas on the wire every 40 ms: ruinous on a network link, and contrary to the "ship pixels
once, then indices" framing that makes animation cross-host-cheap. A client-side clock (ship all frames plus gaps and
let the client animate) would freeze animation while detached and leave two clocks to drift; one daemon clock and a dumb
client wins. The frame cadence is therefore tokio-tick-bound, not wall-accurate
(<https://docs.rs/tokio/1/tokio/time/fn.interval.html>); the drift is invisible at the 40 ms-class gaps real producers
use. Revisit with a dedicated `Instant`-clocked thread if sub-30 ms animation visibly stutters under daemon load.

The client re-uploads the current frame's pixels into its single per-id atlas slot on each `ShowFrame` rather than
caching texels per frame; that is cheap for the small images that dominate terminal animation. If a large (4K-class)
animation makes that per-advance upload the bottleneck, cache an atlas slot per `(id, frame)`: upload once on the
frame's `Complete`, select by UV on `ShowFrame`. A frame edit (`a=f` onto an existing frame, `a=c`) re-ships that whole
frame's pixels, not a diff; add a frame-region delta message only if a producer that re-edits frames every tick shows
up.

## Scrollback-anchored placements: rejected shapes

The signed live-relative anchor model (see the
[reference twin](../../reference/protocols/kitty-graphics.md#scrollback-anchored-placements)) wins over these coordinate
/ synchronization shapes:

- **Absolute rows** (counted in the total-pushed space prompt marks use): placements would never mutate on scroll and
  need no shift directive, but every live-coordinate consumer (ED intersection, `d=` filters, the quad math) would need
  a conversion against a live-top offset the client must also be told, and that offset changes exactly as often as the
  shift directive fires. Same wire chatter, more conversion sites.
- **Re-sending each surviving placement after a shift**: simplest client, but O(placements) messages per scroll effect,
  widening the `Placement` upsert into a hot-path message.
- **Client-side derivation from `GridMsg::Scrolled`**: no new wire variant, but `Scrolled` also fires for region scrolls
  that do _not_ enter scrollback, so the client would re-implement the daemon's distinction against a coarser signal:
  exactly the drift-prone mirroring the daemon-is-authoritative posture forbids.
- **Unsigned rows plus a per-placement scrollback flag**: avoids a signed wire type, but two fields that must agree
  instead of one signed value.

The quad math the first bullet leans on: the client computes `origin_row = anchor_row - 1 + viewport`, so a
scrollback-anchored image surfaces when the user browses back to its line. Quads fully outside the window are culled;
straddling quads keep a negative y-origin and clip in clip space, with no scissor involvement.

Revisit if Kitty ever specifies placement behavior on scrollback _trim_ more precisely than "image is freed when its
text leaves the buffer": re-check the eviction horizon against upstream.

## Anonymous transmits

A transmit may omit `i=`/`I=`; felis then allocates the id itself (the rule is in the
[reference twin](../../reference/protocols/kitty-graphics.md#lifecycle)). The rule is load-bearing for a real producer:
yazi's direct (old-Kitty) preview path transmits `a=T` with no `i=`, so previews render only in a terminal that accepts
anonymous images.

## Why the z-order split at `INT32_MIN / 2` matters

Only the deeply-negative tier sits beneath the cell background colors (Kitty's "image as a background" use); the common
negative-z case (`z=-1`, yazi's direct / old-Kitty preview path) sits above the opaque cell backgrounds but below the
text. Collapsing both into one "below everything" tier lets the cell-background pass paint over `z=-1` placements, so
yazi previews never appear.

## Chrome rows are not drawable

Images with `z >= 0` paint after the cell glyphs, which is what Kitty specifies and what a producer drawing over its own
output needs. The client's own chrome (the search, confirmation, and link-preview bars) is not the producer's output,
and a bar a producer can cover is a bar that guarantees nothing: an image over the link preview shows a URL other than
the one Ctrl+Click activates, and an image over a confirmation prompt hides the question a keystroke is about to answer.
So while a bar holds the bottom row, every image quad is clipped at that row's top edge, at any `z`, as is every
producer-controlled glyph and decoration quad, which can reach into the row from the row above
([rendering pipeline](../rendering/pipeline.md) "Per-frame flow").

Clipping the quads rather than moving the chrome into a pass of its own after the images keeps the image order against
the terminal's own content exactly as specified above; a fourth pass would be a second place where that order is
decided. Rejected: **letting the chrome lose the row**, on the argument that the producer owns the screen. It does not
own the client's chrome, and the bars appear only in response to the user's own action. Revisit if felis grows chrome
that is not confined to whole rows, where clipping stops being expressible as a `max_y`.

## Eviction under the byte cap

When a transmit would overflow the per-session byte cap, the oldest images are evicted outright, placements included,
matching kitty's `ensure_space_for`. This is what keeps a frame-streaming producer alive: mpv's `--vo=kitty` sends one
anonymous `a=T` per video frame with a `C=1` placement that no erase ever drops, so a refuse-to-evict-pinned policy
would freeze playback the moment the cap filled.

## The frame count cap

The per-image and per-session byte caps do not bound how many frames an image holds, because frames are charged
individually: a 1×1 RGBA frame is four pixel bytes, so 64 MiB of budget admits something like sixteen million of them.
Every frame also costs a slot in the store's vector and, on any client mirroring it, a record of its own. That is the
shape of the problem: a run of appends, each of them affordable on its own bytes, whose count no byte cap bounds. A
count cap is what bounds it, and 4096 is that cap.

The number is chosen to be unreachable rather than tuned. kitty's own animation tests run to tens of frames, and the
producers surveyed (`timg`, `chafa`, mpv's `--vo=kitty`) either send stills or re-transmit whole images per video frame
instead of accumulating frames under one id; 4096 is two to three orders of magnitude above any of them.

A frame past the cap is refused, not evicted, which is the opposite of what the byte caps do. The reason is that the
byte cap's unit is an image and this one's unit is a frame _within_ an image: evicting the oldest images to fit a new
one loses whole pictures a producer can notice and re-send, while evicting the oldest frames of a live animation would
silently rewrite the thing being played, with nothing in the protocol to tell the producer it happened. The refusal
reuses `ENOTSUP` because kitty's status codes are a closed set with no `ENOSPC`, and the producer's remedy is the byte
cap's anyway: delete frames, or split the animation across ids.

**Revisit if** a real producer meets 4096 (a long GIF re-encoded frame-per-frame under one id is the plausible case), or
if per-frame cost stops being dominated by the frame vector's slot.

The cap bounds the count; it does not pay for it. A four-byte image appended to 4096 frames holds 16 KiB of pixels in
128 KiB of records, so a budget counting pixels alone bills an eighth of what the mirror holds: the tiny-image flood
again, one level down. So `ImageShadow` charges its aggregate in the same unit the store's `ImageEntry::byte_len` uses
("Why the budget counts more than pixels" below): the entry, every frame record, and the pixels. The mirror being no
looser than the store is what keeps this direction safe: a client can only refuse a session the daemon would itself have
evicted.

## The live outbox carries markers, not pixels

Between two fan-outs the dispatcher accumulates what changed about the session's images in `Session::image_events`. The
obvious content for that queue is the wire messages themselves, and it is the wrong one: wire messages include
`ImageMsg::Chunk`, so the queue would hold a second full copy of every decoded image until the session task next woke,
with a producer that transmits faster than the fan-out drains deciding how many copies pile up. mpv's `--vo=kitty` does
exactly that, one image per video frame; sixty-four 4 MiB transmissions arriving in a single read cost a quarter
gigabyte of pixels the store already had.

So the queue holds `ImageEvent`, an enum that spells out the pixel-free controls and reduces every transmission to a
_marker_ naming an image (or one animation frame). The bytes are read back out of the store when the session task ships,
by the same `image_sync_messages` the attach-time rehydrate uses. Deliberately no catch-all `Wire(ImageMsg)` variant:
one would let a `Chunk` back into the queue, and the invariant would go back to being a convention that review has to
enforce. Spelled-out variants make "pixels sit in the outbox" unrepresentable, and the duplication they cost is the
price of that.

Two properties fall out. A marker whose image the _same_ drain later evicted materializes to nothing rather than to
stale bytes, because the store is consulted at ship time and does not have it, which is also what lets the eviction path
stay a pair of tiny markers. And with no subscribers the markers are dropped without materializing at all, so a detached
session pays nothing to keep an animation advancing.

Because a `Transmit` marker re-states the whole image (root frame, every animation frame, the displayed index), a later
marker for the same id makes an earlier one redundant, and only the last is materialized. That is what bounds the mpv
case: one materialization per id per fan-out rather than one per video frame. Controls are never deduped and never
reordered, which is not symmetry-breaking laziness but a correctness requirement. `PlacementsShifted` is a _delta_ (it
moves every anchor by `lines`), so collapsing two would silently halve a scroll. And the client's whole-image delete is
a no-op for an image it never received, so a control that moved or vanished could strand a placement on screen.
Transmissions have neither hazard: dropping one is invisible because the survivor says everything it said.

The safety of that asymmetry rests on a daemon-side ordering the five delete paths all share: eviction, `d=a`, the
filtered deletes, `d=I`, and the alt-screen switch each emit `PlacementRemoved` for an image's placements _before_
`Delete` for the image. Client-side placement cleanup therefore never depends on the client holding the image, which is
what makes reordering pixels around it safe.

Rejected: making the whole outbox a dirty-_set_ (one entry per changed id, materialized entirely from current state),
which would bound the control events too. It cannot work while `PlacementsShifted` exists, because a delta has no
current state to read back; materializing shifted anchors _and_ replaying the shift directive would double it. Revisit
if the placement mirror ever moves to whole-table replacement, at which point the delta disappears and the set becomes
expressible.

## Why the budget counts more than pixels

Comparing only the sum of frame payloads against the cap measures the wrong thing. A 1x1 RGBA image would be charged 4
bytes but really costs the `ImageEntry`, the map slot holding it, the `Frame` header, and two heap allocations: roughly
40 real bytes for 4 charged, with no limit on entry _count_. Transmitting 400 000 of them would charge 1.5 MB against
the cap while growing the daemon by 16.6 MB, linear in the count with no ceiling short of the id space: a nominal 256
MiB budget would admit about 2.5 GB.

So `ImageEntry::byte_len` charges the entry's own footprint plus each frame's, expressed in `size_of` terms rather than
a measured constant so the charge follows the struct if a field is added later. The invariant is that the store charges
at least what it holds; charging a little more than the allocator really spends is the safe direction, and for real
images (a 1080p frame is 8.3 MB) the fixed cost rounds to nothing. Only the tiny-image flood, which is the attack,
notices.

The cached total is not serialized either: it is a derived value, and a restored store whose cache disagreed with its
entries would carry a cap that bounds nothing. `ImageStore` recomputes it on deserialization, so a dump, stale or
tampered, cannot weaken the budget.

_Revisit if_ the fixed charge ever becomes visible to a legitimate producer (a real workload rejected while far under
the nominal cap). The fix then is a larger cap, not a cheaper accounting: the point is that the number in the config
means something.

## Why `a=a` / `a=d` are never acknowledged

kitty's dispatcher sends no response for successful animation-control and delete commands, and real producers depend on
that: kitten icat and mpv both emit these without `q=` and never read responses, so an OK would land in the user's shell
as literal `_G…;OK` text after the producer exits.

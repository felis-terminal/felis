---
title: Image store
sidebar:
  order: 3
---

The image store is the daemon's per-session repository of Kitty graphics protocol images: one pool of decoded pixels
that every placement in the session draws from. No cell holds a handle into it, and the two placement paths reach an
image differently. A direct placement (`a=T` / `a=p`) is named only by the `Placements` side table beside the store, so
placing, moving or deleting one touches no cell and dirties no row. A Unicode-placeholder placement is grid content: the
cell's own grapheme and foreground color spell the image id out, and the resolver reads it back from there
([grid-and-cells.md](grid-and-cells.md) "Image references"), so such a placement moves with the text it is written into
and changing it means rewriting cells.

## Structure

The store maps a Kitty image id to one entry holding that image's decoded frames, its playback state, and a refcount; a
still image is a one-frame entry, so animation adds no second representation. Placements live in a sibling `Placements`
side table rather than inside the store, which is what lets an image be placed twice, or placed and then scrolled,
without the store knowing.

Entries sit in one insertion-ordered map, front to back oldest to newest, and eviction walks it in that order. A
`HashMap` beside a `VecDeque<ImageId>` is the alternative and is rejected because it makes the id-to-entry mapping and
the eviction order two structures that can drift apart. A real LRU is rejected for a plainer reason: sessions hold tens
of images, not thousands, so recency ordering would buy accuracy nobody can measure.

Ids for images transmitted without one are assigned counting **down** from the top of the id space, away from the low
ids producers pick by hand. A collision here is not a rejected transmission but a silently overwritten image, which is
why the two id sources are kept at opposite ends rather than merely checked against each other.

## Memory accounting

`bytes_total` tracks what the store actually costs, not what its pixels measure: each entry is charged its own footprint
and every frame record's, on top of the frame payloads. A budget counting pixels alone would let a flood of 1x1 images
grow the daemon by an order of magnitude more than it charged, which is the whole reason for the extra terms
([kitty-graphics.md](../protocols/kitty-graphics.md) "Why the budget counts more than pixels"). Producers transmit
compressed (zlib) where useful; the daemon decodes once on intake, so the payloads are raw pixel buffers.

The store also tracks a per-id refcount, bumped by `retain` (a placement pins its image) and dropped by `release`. The
store's own eviction (`insert` and the frame mutators) respects it: refcount-0 entries go first, and when every
remaining entry is pinned the store fails with `InsertError::OverCapacity`. But the dispatcher never surfaces that
failure to producers: it pre-evicts the oldest images outright, placements included, before inserting (see Limits).

Placements anchored in scrollback keep their image refcount while the anchor line stays in the retained ring, so pixel
memory for a scrolled-out image lives until the line is trimmed, bounded by `bytes_cap` and the scrollback depth, with
the refcount releasing exactly when the anchor line leaves the ring. Revisit a separate pixel budget for history-only
images if scrollback depth ever becomes configurable to very large values.

## Limits

The numbers (the per-session byte cap, the per-image decode cap, the frame-count cap) are in
[the reference](../../reference/protocols/kitty-graphics.md) "Limits". Two things about them are decisions.

They are `felis-protocol` constants rather than daemon-private ones because a client mirrors this store and sizes its
buffers from the headers the daemon emits (REQ-1008). A client holding to a smaller number than the daemon would refuse
an honest header and cost the window its connection; the shared constant is what makes "no looser than the store" a
property of the type rather than of two codebases staying in step.

The frame count needs a cap of its own because the byte caps do not imply one. A 1x1 frame is four pixel bytes, so the
session budget alone would admit millions of frame slots ([kitty-graphics.md](../protocols/kitty-graphics.md) "The frame
count cap"). It is refused before anything is evicted to fit, so a producer cannot spend other images to buy a frame
slot it will not be allowed.

When a new image would push past `bytes_cap`, the dispatcher first evicts the **oldest images outright, placements
included** (emitting `PlacementRemoved` + `Delete` so attached clients drop them too), matching kitty's
`ensure_space_for`; the store's refcount-respecting `insert` then runs against the freed space. The cap recycles rather
than rejecting: a frame-streaming producer like mpv pins every frame with a `C=1` placement that no erase ever drops, so
a refuse-to-evict-pinned policy would freeze playback the moment the cap filled (see
[kitty-graphics.md](../protocols/kitty-graphics.md#eviction-under-the-byte-cap)).

The store's own refcount-respecting eviction announces what it drops: `insert`, `push_frame` and `replace_frame` return
the evicted ids, and the dispatcher turns each into the same `Delete` a deliberate removal emits. Swallowing them would
be a real divergence rather than a tidiness point: a client mirrors this store's byte total against the same 256 MiB
cap, so an eviction it never heard about would leave it counting bytes the daemon has already freed, until its own cap
refused an honest header and cost the window its connection. Eviction is also priced before anything is dropped, so a
mutation that ends in `OverCapacity` leaves the store exactly as it found it and there is nothing unannounced to report
on the error path.

## Animation frames

An animated image is an `ImageEntry` whose `frames` vector holds more than one frame; `frames[0]` is the root (Kitty
frame number 1). Each frame is stored **coalesced** (a complete `width × height` buffer), with the daemon's compositor
flattening Kitty's base-frame + transmitted-rectangle form before anything reaches the store. Kitty instead stores the
transmitted rectangle plus a base-frame reference, coalescing lazily at render time; felis pre-coalesces so the store
stays compose-free and the renderer and wire see only finished buffers (the protocol-side rationale lives in
[kitty-graphics.md](../protocols/kitty-graphics.md#frame-storage-pre-coalesced)).

The cost is memory: N frames cost N full canvases even when each `a=f` touches one pixel. Kitty budgets animation
storage separately at `storage_limit × 5`; felis folds frames into the one existing `bytes_cap` budget, so a
pathological producer hits eviction sooner than kitty would. Revisit with lazy coalescing and a bounded reference chain
if a real producer is starved. The frame mutators (`push_frame` / `replace_frame` / `remove_frame`) run the same
refcount-aware eviction as `insert`, excluding the image being grown so it never evicts itself to fit its own frame.
They are also the only way in: the store lends out an `AnimationControl` view (gap, displayed frame, run mode, loop
bound) rather than a mutable entry, so a mutation that moves bytes has nowhere to be written except a method that
charges for it. A `&mut ImageEntry` is rejected for the reason budgets usually fail: it reaches the entry's dimensions
and its frame vector, so the charge stays honest only as long as every caller remembers to make it, and the store must
not depend on that.

Playback state lives on the entry, and the advance state machine (`ImageEntry::advance(now_ms) -> Option<usize>`) is
pure and clock-source-agnostic: it takes monotonic milliseconds, not `Instant`, so it is proptest-able without a real
clock and `felis-grid` imports no time-source opinion. The daemon's per-session timer drives it (`advance_animations` /
`next_animation_due_ms`); only placed images (`refcount > 0`) tick, preserving idle-zero-redraw. Because the current
frame index lives on the entry, a reattaching client's rehydration lands on the live frame.

## Transfer methods

Which `t=` methods felis implements, what each does, and the platform split are in
[the reference](../../reference/protocols/kitty-graphics.md) "Transmission methods"; the safety argument for the
filesystem and shared-memory paths is in [security-model.md](../security-model.md) and
[kitty-graphics.md](../protocols/kitty-graphics.md). What matters to the store is only that every method ends the same
way: the daemon decodes once on intake, so the store holds raw RGB or RGBA buffers and nothing downstream of it knows
how the bytes arrived.

## Rehydration payload

Detaching a client evicts nothing: the store belongs to the session, so a reattach rehydrates from it rather than asking
the producer to retransmit. What the burst carries and in what order is in [ipc.md](../../reference/ipc.md) "Grid (kind
= 2)". This page's decision is that the store ships **whole** rather than filtered to what the visible screen
references, and that it ships **last**. A placement can name an image from a row that has scrolled away and back, so a
subscriber that held only the visible subset would have to fetch on every scroll; the price of shipping everything is
bounded by the per-session store budget, and putting the bytes after the grid keeps a cold attach painting at
"round-trip + cells". _Revisit if_ the store budget grows past what a cold attach over a slow link can absorb; a
referenced-first order or an on-demand fetch would then earn its bookkeeping.

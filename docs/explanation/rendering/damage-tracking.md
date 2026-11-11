---
title: Damage tracking
sidebar:
  order: 3
---

Damage tracking answers two questions with one mechanism: which rows the daemon must ship this cycle, so a quiet
terminal sends nothing and a busy one sends only what changed; and which rows the client must repaint, so one echoed
keystroke costs one row of instance building and upload rather than the whole grid.

The tradeoffs are different from a typical GUI: a terminal usually has many cells, most clean, with sparse changes. The
model must scale to "one row changed in a 200×80 grid" without iterating the whole grid.

## Where damage lives

- **Daemon (`felis-grid`).** A tracker records which logical rows changed since the last diff cycle. Every grid mutation
  (cell writes, scroll, erase, alt-screen switch, resize) marks rows dirty, an erase only the rows whose cells it
  changed; the cycle drains the dirty rows into the wire protocol and clears the tracker.
- **Client.** The shadow's writes run through the same screen buffer the daemon uses, so they mark the same row bitset.
  The renderer reads it when a frame is due and the client clears it after the paint. Whether a frame is due at all is a
  single redraw flag; damage decides only how much of that frame is rebuilt.

## Daemon damage representation

Damage is row-granularity, backed by a `u64`-word bitset rather than a `bool` per row: at the sizes terminals run at, a
whole screen's worth of rows fits in one or two words, so a clear, a mark-all and a region probe each collapse to a
single word operation and draining the dirty rows costs `O(dirty rows)` rather than `O(rows)`. Marking is total: an
out-of-range row is ignored rather than refused, so a resize window needs no separate reset.

## From damage to the wire

A diff cycle emits the dirty rows as one `RowDelta` batch ([the IPC reference](../../reference/ipc.md) carries the
shape), with the side-table entries they reference shipped **before** them. Full-screen events (resize, alt-screen
enter/leave) mark every row, so the next cycle ships them all.

A scroll moves the marks instead of adding to them. The grid rotates the band's rows, shifts the band's dirty bits by
the same count, marks the rows the scroll vacated, and queues a `GridMsg::Scrolled` directive that the shadow replays as
the same rotation. A row written before the scroll stays owed at its new position, so the cycle ships the directive and
only the rows the rotation does not reproduce. Each subscriber's own tracker shifts the same way when it takes the
directive, which is why a subscriber that has not pulled for several cycles can take it too: the rows it still owes move
with the content they describe.

## Client repaint

The client keeps a shadow screen that absorbs each `RowDelta`. Inbound grid traffic, cursor/selection changes, and image
updates all set a sticky redraw flag, which the event loop turns into exactly one redraw request once it has drained its
queue, collapsing a burst of messages into a single paint.

The renderer keeps the cell instances of the previous frame in one fixed-size slot per row and rebuilds only the rows
the shadow marked, plus the rows the cursor left and entered; an unused slot entry is a zero-sized quad, which
rasterizes nothing. It then uploads only the rebuilt rows. Anything every row's instances depend on rebuilds them all:
the grid geometry, the cell metrics, the theme and palette, DECSCNM, the selection, the viewport, a chrome bar or
pre-edit span, and a glyph atlas recycle, which moves every slot. So does a row that outgrows its slot. When padding
every row to the widest one would cost far more than packing them (past half the device's buffer limit, or several times
the packed size), the rows stay packed and every frame rebuilds and uploads all of them until the rows even out. Images
and the overlays are not cell instances and are rebuilt every frame.

The glyph populate walk that runs before the instances are built follows the same marks: it primes atlas slots for, and
shapes, only the marked rows, and keeps the shape results of the other rows from earlier frames. It walks every row when
the grid geometry changes, when the grid starts or stops needing the shaper (the first interned cluster, a feature
change), and after any glyph atlas recycle, since a recycle drops the slots the unmarked rows rely on. The cursor needs
no walk: moving it changes no glyph.

## Correctness

The enforced invariant is **no underdraw**: every cell whose visible content changed since the last diff cycle must
reach the client's next paint. At the grid layer that means the cell's row is dirty, or the cell holds what the pending
scroll directives move into it from the prior screen. On the client, every applied row and every row a scroll directive
moved is marked, and the renderer rebuilds every marked row. Cursor-only moves must leave damage empty, since the cursor
is a client-side overlay, not cell content: a carriage return, a cursor-positioning sequence or a tab marks nothing, and
a line feed marks only what its scroll vacates. A handler that also changes something a row payload carries marks that
row, including a soft-wrap bit set without a cell write, as an `HT` under `?41` does. The cursor's own position ships as
`CursorState` whenever it differs from what the subscriber last received.

Overdraw is allowed by design at both layers: a one-cell write dirties its whole row, and the client draws every row
slot each frame. It costs IPC bytes and GPU time, not correctness; the per-cell tracking that would remove it is
rejected ("Prior art" below).

The harnesses that pin both layers are listed in [the testing reference](../../reference/testing.md) "Damage-tracking
correctness".

## Prior art and alternatives considered

**Granularity: per-row, not per-cell.** Kitty, WezTerm, and Ghostty track a per-cell dirty bitmap; Alacritty and foot
keep per-row damage and hand it to the compositor as partial presentation. felis tracks per-row on both sides of the
wire. Per-cell damage is rejected: a keystroke's echo dirties one row, and rebuilding one row's instances costs tens of
microseconds, so per-cell bookkeeping would save little on the client and nothing on the wire, where a row is the unit a
`RowDelta` ships. A tile model is rejected for the same reason from the other side: it could be cheaper for huge
screens, but the row bitset already collapses the hot operations to single-word ops at the sizes terminals run at.

**Client instances: fixed row slots, not a packed buffer.** Packing each frame's instances back to back, as a full
rebuild does, makes a row's position depend on the instance counts of every row above it, and those counts change with
the content: an echoed character adds a glyph to its row. One new glyph near the top would then shift and re-upload
every later row. A slot per row, sized for the widest row with headroom, pins every row's offset so a changed row is one
ranged write, at the cost of drawing the empty slot entries as zero-sized quads.

**Collapse threshold: none.** Kitty and WezTerm collapse to a full redraw at ~80% dirty cells, Ghostty at ~75%, because
tracking that many cells separately costs more than redrawing everything. felis has no threshold: partial updates ship
and rebuild row-by-row regardless of count, and only full-screen events mark every row. Rebuilding every row one at a
time costs the client the same walk a full rebuild does, and draining a dirty-row bitset is already cheap.

**Scroll shift: grid rotation, not a texture copy.** When a line pushes in, Kitty and Ghostty copy the previous frame's
texture region up and paint the new bottom row; Alacritty and WezTerm rotate row indices in the cell store. felis
rotates: the cell store is a ring, so a scroll is an index update (not a copy), and the daemon marks only the rotated-in
rows and the rows written since. The client's instances carry absolute positions, so its scroll directive marks every
row of the scrolled region and the renderer rebuilds them; the GPU never sees the rotation.

**A scroll over written rows: move the marks, not replay the band.** The alternative sends a directive only when no row
of the band is dirty and ships the whole band as rows otherwise, on the argument that a directive on top of a band
replay only adds bytes. That argument holds only while the band is replayed in full, and producers almost never scroll a
clean band: a flood or a streaming printer writes the line before its line feed, a pager rewrites its prompt on the
bottom row before it scrolls, and vim scrolls with DECSTBM plus `DL`. Under that rule a window receives almost no
directives and nearly every row per frame in all four of those workloads. Moving the marks with the rows makes the
directive sound whatever the band held, so the rule costs one directive per band per cycle and saves every row the shift
reproduces. `IL` / `DL` are the same rotation over the band from the cursor row to the bottom margin, so they ship the
same directive.

**Directives of different bands ship separately.** Consecutive shifts of one band fold into one directive, and a
directive whose band ends up owed in full is dropped, since the rows then restate everything it would move. Shifts of
different bands (vim's `DL` band and a full-screen line feed in one cycle) each keep their directive rather than
collapsing into a replay past some count: a directive is a few bytes against a row that costs about two bytes per
column. The per-subscriber queue stops at 1024 directives, past which a band ships as rows.

**A DECSLRM band ships as rows.** A scroll inside left and right margins narrower than the grid moves part of each row,
and `Scrolled` names whole rows. Carrying column bounds would widen the wire message for a mode no measured workload
scrolls per frame.

**A vacated row is restated through a blank.** The shadow's rotation leaves the rotated-out cells in the storage of the
vacated rows, past each row's occupancy watermark, where nothing reads them. A 0.1.0 client's row write compares the
incoming cells with that storage and skips a write that matches, which leaves the row reading blank; a `yes` flood,
whose new line always matches the line that scrolled out, would blank every new line. The daemon therefore precedes the
restatement of a vacated row that is not blank with an entry of default cells for the same row: whatever the storage
held, one of the two entries differs from it and lands. Fixing only the client was rejected because it leaves every
0.1.0 client showing blank rows. The cost is one blank entry per such row per cycle, about one byte per column.

**Images, selection, cursor: not damage at all.** Other terminals mark dirty cells for image bounding boxes,
selection-rectangle diffs, and cursor old/new positions. felis tracks none of these: image placements live in a side
table shipped over IPC before any `RowDelta` that references them; selection is a client-side overlay pass; the cursor
is per-window client state. Each requests a client repaint rather than marking grid cells: images and overlays are
rebuilt every frame, a selection change rebuilds every row, and the renderer rebuilds the cursor's old and new rows
itself.

**Compositor partial-frame: rejected.** foot submits Wayland `wp_damage` rectangles so the compositor can skip unchanged
regions; felis always presents the full surface.

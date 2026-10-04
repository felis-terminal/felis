---
title: Grid and cells
sidebar:
  order: 1
---

The grid is the daemon's authoritative model of "what is on the screen right now." Every byte that comes out of the PTY
ultimately mutates this structure. The client receives diffs of the grid and turns them into pixels.

## Two grids per session

Each session has two grids:

- **Primary**: the screen the shell normally writes to. Has scrollback.
- **Alternate**: temporarily activated by `?1049h` for full-screen TUIs. No scrollback. Reverts to primary on `?1049l`.

The current grid is selected by the alternate-screen-buffer mode, and a resize re-fits whichever one is current. Each
side treats the inactive buffer differently, which is where the two screens stop being symmetric
([scrollback.md](scrollback.md) "Prior art and alternatives considered", reflow decisions D3 and D4).

## Cell

A cell is the smallest addressable rendering unit: one grapheme, the attribute state in effect when that grapheme was
written, and the hyperlink and sizing slots the cell participates in. Every one of those is a **handle** into a side
table the grid owns, not a payload: the pen interns to a four-byte `StyleId` rather than the 16-byte `Attributes` struct
([Style interning](#style-interning)), a multi-codepoint cluster interns to a 32-bit id
([Cluster interning](#cluster-interning)), and the `OSC 8` and `OSC 66` slots are `NonZeroU16` niches. The wire shapes
are in [row-codec.md](../../reference/row-codec.md) "Grapheme records".

Handles rather than payloads is one decision with two consequences, and both are load-bearing. `Cell` stays `Copy`, so
every bulk grid operation is a `memcpy` rather than a per-element clone (["Cluster interning"](#cluster-interning)
measures the difference); and `Cell` stays 16 bytes, four to a cache line, which is the residency the scrollback ring
multiplies across its whole retention window. New per-cell state must fit that budget or move to a side table of its
own.

Two things a reader may expect on the cell are deliberately not there. Damage is per row, tracked in a separate bitmap
([damage-tracking.md](../rendering/damage-tracking.md)), so there is no `dirty` flag. And there is no image reference:
Kitty placements sit off-cell ([Image references](#image-references) below).

The one non-handle distinction the grapheme carries is width. A cell is a glyph's **primary** or one of its continuation
cells, and that role is a grapheme variant rather than a separate field
([Roles in a multi-cell run](#roles-in-a-multi-cell-run)). Printable ASCII gets its own variant so the dominant case
costs one byte and no table lookup at all.

### Building a cluster

felis runs no upstream grapheme segmenter. The parser decodes UTF-8 and hands the grid one Unicode scalar at a time
(`Grapheme::Char(c)`); the grid folds each scalar onto the previous cell's cluster when UAX#29 puts the two in one
grapheme. That incremental fold is what grapheme-cluster mode (DECSET `?2027`, reported permanently set, REQ-602)
promises a running program: the cursor advances by grapheme cluster, not by scalar.

Folding is decided per scalar against UAX#29, and the reason it is not simply "width 0 extends the previous cell" is
that the width table disagrees with the segmentation rules in both directions. An emoji skin-tone modifier is GB9 Extend
but scores width 2; the pictographic base after a ZWJ (GB11) and the second regional indicator of a flag (GB12/GB13) are
ordinary wide scalars that must still fold. Each is recognized explicitly, and the ZWJ continuation is gated to a
non-ASCII follower with a free trailing cell so a stray ZWJ can neither swallow ordinary text nor overrun its base's
footprint. The continuation reaches only the next scalar printed: a control that moves the cursor (BS, HT, LF, VT, FF,
CR) ends it, while BEL, which leaves the cursor at the joiner, does not.

A bidi override is the one zero-width scalar that outlives a missing owner. At column 0 or after an empty cell the grid
holds it and folds it into the next character printed, after that character's own scalar, and a regional indicator
carrying it still pairs into a flag. Any control or escape sequence other than SGR, OSC, BEL or ST discards it first, as
does a resize; an SOS or PM string, which the parser drops whole, does not
([security-model.md](../security-model.md#text-rendering) "Text rendering").

A cluster can also outgrow the cells its base reserved. A narrow base placed at width 1 (❤, U+2764; a keycap digit)
becomes width-2 once a variation selector or keycap combiner requests emoji presentation, yet the base was already
written at its narrow width before the selector arrived. After every fold `widen_cluster_if_needed` reconciles the
cluster's unicode-width against that reserved footprint: a base that grew to two cells claims a trailing `Spacer`, and
when the cursor still parks immediately after the base, advances it past the now-two-cell glyph. A selector arriving
after a cursor move widens the glyph in place without dragging the cursor, matching the rule that a combining mark does
not move the cursor. When the trailing cell is occupied, off-screen or past the DECSLRM right margin, the glyph stays
one cell wide, and a reflow keeps it at that one cell rather than shifting the rest of its line.

The grid decides cluster boundaries and cell footprint; it stays font-blind. Which face paints the cluster (a monochrome
dingbat or the color-emoji glyph) is a client-side shaping decision, covered in
[text-shaping.md](../rendering/text-shaping.md) "Compositing a multi-codepoint cluster".

An application that ignores mode 2027 and sums per-scalar widths still reads 👩‍💻 as four cells and drifts its own cursor.
That is the application's width bug, not the terminal's, and it cannot be reconciled without giving up the clustered
display.

### Cluster interning

Holding a handle inline, rather than a heap-owning `Box<str>` on the cell, is what makes `Cell` `Copy`, and that
property is the single largest throughput lever for plaintext output. With an owning cluster payload, every bulk cell
operation pays a per-element `clone()` (plus drop glue on the overwritten slot) instead of a `memcpy`; the hot paths are
`Grid::scroll_region_up` (the `cat` / shell-output shape: each line-feed at the bottom evicts one row into the history
side of the unified ring and blanks one) and the IL / DL / ICH / DCH shuffles. Measured on an 80-cell row (release), the
scroll-by-1 cell cost is 893 ns with `clone()` against a 71 ns `Copy` ceiling (12.6×), and `scroll_region_up` accounts
for ~58% of the daemon-side `plaintext_scroll` cost. With the handle, the bulk paths use `copy_from_slice` /
`copy_within` / `slice::fill`, and `Cell` is a 16-byte `Copy` struct (see [Style interning](#style-interning)) rather
than the ≥48 bytes an owning payload forces: less scrollback residency and less memory bandwidth on every grid walk. The
regression anchors are `crates/felis-grid/benches/scroll_region.rs` and
`crates/felis-client-core/benches/end_to_end_throughput.rs`.

The registry mirrors the OSC 8 hyperlink registry exactly: a `ClusterTable` owns the entries and a dedup index keyed by
a 64-bit digest of the text (a digest hit is confirmed against the entry, so a collision costs a duplicate row and never
a cell resolving to the wrong text); `intern_cluster(&str)` dedupes, `cluster_str(id)` resolves, and the table grows
monotonically. It serializes as its bare entry vector, like the style and link tables: the index is derived, so it
rebuilds on load rather than riding the dump as a second copy of every cluster. Entries stream to the client as
`GridMsg::Cluster { id, text }` **before any `RowDelta` that references them**: a `RowDelta` is only self-describing
once its `Cluster` messages have arrived, and the same guarantee is what the hyperlink table relies on. The shadow
screen **installs** entries at the daemon-assigned id rather than re-interning, so daemon and shadow share one id space
and `Cell` equality (an id compare) stays meaningful across the hop.

Cluster ids are `NonZeroU32`, not `NonZeroU16` like hyperlinks: a long-lived session with heavy CJK / emoji churn can
mint more than 65 535 distinct clusters, which is why the table's own cap ([below](#why-the-cluster-table-is-capped))
sits above the `u16` space rather than at it. A cluster id is also grid-local, so the niche saving that motivated `u16`
for links does not apply: `Grapheme` is 8 bytes either way once it carries a 4-byte handle.

Rejected alternatives:

- **Inline cluster bytes** (`Cluster { buf: [u8; N], len: u8 }`): keeps `Cell` self-contained with no table, no wire
  message, and every reader unchanged. But a fixed buffer sized for the common case (≤15 bytes: é, Thai / Devanagari
  stacks) cannot hold long ZWJ emoji sequences (👨‍👩‍👧‍👦 is 25 bytes), and truncating them corrupts clipboard / selection
  text; covering them inline pushes `Cell` back to ≥48 bytes, and a hybrid "inline + overflow table" reintroduces the
  table for the rare path _plus_ a second representation to keep consistent. The interner is one representation and
  shrinks `Cell`.
- **Keeping a heap-owning payload per cell** (`Box<str>`, or a shared `Arc<str>`) **and optimizing around it**: the op
  count in `scroll_region_up` is already minimal (one row pushed, one blanked); the cost is per-`Cell` clone / refcount
  / drop traffic, so nothing short of a cheaper, `Copy` `Cell` moves it.
- **Expanding ids to inline strings in the row payload**: what is rejected is the payload, not the custom codec. The row
  codec is hand-written and specified either way ([row-codec.md](../../reference/row-codec.md)), and a string per cell
  is what it would have to carry, where it emits a short varint handle straight from the borrowed `&[Cell]`. It avoids
  the `GridMsg::Cluster` message for no in-memory benefit over streaming the table, and loses the table's dedup: a mark
  repeated down a column would ship its text once per cell.

#### Referenced-before-use is causal, not positional

The delivery rule is causal, not positional ([ipc.md "Registry delivery"](../../reference/ipc.md) states it, and what it
does not promise). The positional reading is what the wire cannot afford: satisfying it means streaming each entry as it
is interned and restating both whole tables ahead of a rehydrate's first row. The registries are scrollback-wide where
the screen is not, so a session that has interned tens of thousands of clusters would make every reattaching window
decode all of them before it could paint a screenful of cells.

The client tables pay for that ordering by holding holes. A shadow installs each entry at the id the daemon minted
(`ClusterTable::install`, `LinkTable::install`), so a table grows to whatever id arrives first and the ids beneath it
stay unfilled until their own messages land. A gap resolves to nothing, where an entry whose text is legitimately empty
draws nothing, so each gap is an explicit empty slot rather than a filler that would trade a missing accent for a wrong
cell. What a gap never has to answer for is a cell: the causal rule means no admitted row names one, and a row that does
ends the attachment ([ipc.md "Grid admission"](../../reference/ipc.md#grid-admission)) instead of degrading a cell the
next message might never repair. Those slots stay bounded by the caps the tables already enforce
([Why the cluster table is capped](#why-the-cluster-table-is-capped), [Hyperlink interning](#hyperlink-interning)): the
id space bounds how many one high id can force, and the byte budgets bound what the occupied ones hold.

Shipping the **dense prefix** up to the highest referenced id is the alternative that keeps those tables append-only. It
is rejected because a visible entry can be numbered anywhere: one hyperlink minted late in a long session drags every
entry beneath it into the attach burst, which is the cost the visible-first order exists to avoid.

Selecting per row is why the per-connection sent-state is a set and not a cursor (`serve/registry_sync.rs`). A cursor
can only say "everything up to N"; a visible-first burst leaves a set with holes in it, and advancing the cursor to the
table tail would record the skipped entries as delivered. Browsing back into scrollback is where that would show:
composed rows name whatever was interned when they were written, which is exactly the part of the registry the burst
skipped. The set is stored as a sent prefix plus its exceptions, so the steady state (a full prefix, no exceptions)
answers "does this connection hold everything?" with an integer compare and keeps the per-cell handle walk off the hot
path.

The drain that converges the rest rides cycles that already carry content, rather than manufacturing a frame of its own:
a client's redraw and its next frame pull both key off frames arriving, so frames that change nothing on screen would
walk the renderer through the whole table at vsync. Convergence is all the drain is; correctness rests on the reference
rule, which is why it can be lazy and why an `Ops` connection is excluded from it. That exclusion is what decides that
such a connection is sent no rows either: an id the reference rule promises is resolvable is a promise the daemon has to
keep on every connection it ships a row to, so the connection that opts out of the registries opts out of the payloads
that name them.

A **snapshot-scoped dictionary** inside the row codec reaches the same ordering guarantee from the other end: the
payload would carry the entries its own rows name, so a snapshot arrives self-contained and no delivery state is needed
at all. It is rejected as the larger change for the same result: the codec would grow a second way to address a cluster,
and the delta path would split, because a live `RowDelta` names ids minted long before it that no snapshot dictionary
carries. Both mechanisms would then run where the causal rule covers snapshots and deltas in one statement. _Revisit if_
the lazy backfill proves racy in practice; a snapshot that carries its own entries is what would answer such a race.

#### Why the cluster table is capped

Monotonic growth is the accepted trade-off, the same one the link table makes for reattach-stable ids. What is _not_
acceptable is growth a running program can drive without limit, and it can drive two dimensions of it. Combining marks
drive **length**: each mark folds into the owner cell's cluster, so the Nth mark interns an N-byte string no earlier
entry dedupes against, at a cost quadratic in the mark count that the table never frees and the screen never shows,
since a mark advances no cursor. Varying the mark drives **variety**: one entry per cell, each cheap and each permanent.
An entry outlives the cell that referenced it (the row evicts from scrollback, the table does not), so what a session
holds tracks everything it has ever displayed rather than what it can still show. Uncapped, either dimension measures in
the hundreds of MB against the ~15 MiB a default scrollback costs
([scrollback.md](scrollback.md#capacity-and-eviction)), which is the line worth holding: an index of the grid should not
outweigh the grid.

So the table bounds both dimensions, at the values [ipc.md](../../reference/ipc.md) "Semantic limits" carries. Each
bound sits on the type or the table rather than at the call sites, because a bound the mint paths must remember is a
bound the next mint path forgets: a cluster's text is a `ClusterText` whose constructor is the check, so `TryFrom`
extends it to a restored dump and to a cluster streamed in from a peer, and `ClusterTable::intern` refuses a _new_ entry
after the dedup lookup, so a full table still serves every cluster already on screen and a mark repeated on a visible
cell keeps folding.

The length bound is UAX #15's Stream-Safe Text Format, which holds that no meaningful text needs more than 30 combining
marks on one base; the longest RGI emoji ZWJ sequence (👨‍👩‍👧‍👦, 25 bytes) sits nearly 4x under it. The count bound admits
legitimate variety several times over, because only multi-scalar clusters intern at all (a lone CJK ideograph is a
`Grapheme::Char`, never an entry): Unicode's registered inventories run to ~29 000 IVD sequences and ~5 000 RGI emoji
sequences, plus the mark stacks Indic and Thai text form.

Past either bound a mark stops folding and the cell keeps rendering the cluster that fit, rather than being truncated or
replaced, so the failure is a missing accent on new text and never a corrupted cell. A refused trailing ZWJ does not arm
the GB11 continuation, or the cap would become a way to swallow the rest of the line. Evicting from the table instead is
rejected: it breaks the monotonic-id contract reattach depends on, and it would let one runaway cell push out every
legitimate cluster on screen. Refusing to mint keeps the ids stable.

The count bound also covers the _shadow_ side, where the id is a peer's choice rather than the table's own: `install`
grows the table to the id it is handed, so a `GridMsg::Cluster` naming an id near `u32::MAX` would resize it to tens of
GB from one message. An id past the cap is one a daemon honoring the same cap never mints, so the shadow refuses it and
the attachment ends, the same answer `MAX_GRID_ROWS` gives a `RowDelta`'s row
([ipc.md "Grid admission"](../../reference/ipc.md#grid-admission)).

_Revisit if_ a real workload is refused, or a real script legitimately needs a longer cluster: raise the cap rather than
adding eviction, and re-measure the per-entry cost first, since the numbers are set against what the scrollback beside
them costs. If the worst case rather than the typical one starts to matter, charge each entry's real footprint against a
byte budget the way the image store does; the count cap is what bounds the id space, so a byte budget would sit beside
it, not replace it. If a second consumer needs cluster text without a grid or shadow in hand, reconsider an inline or
shared-arena representation.

### Hyperlink interning

An `OSC 8` block's URI lives in the grid's `link_table`, and the cells it covers carry a 1-based `NonZeroU16` handle
into it, the same shape as the cluster registry, for the same reason (a `Copy` `Cell`), with the same monotonic-growth
contract: entries are never freed, because a reattaching client receives the table and the handles its cells already
hold must keep meaning what they meant.

Dedup is what makes that contract survive a long log of the same link: a producer re-emitting one URI (or one `id=`)
reuses the row it already minted, which xterm and foot both rely on. The index is keyed by a 64-bit digest of
`(id, uri)` rather than by the strings, so it does not hold a second copy of every URI, and a digest hit is confirmed
against the entry before it is trusted: a collision costs a duplicate row, never a wrong link. A linear scan over the
entries is the rejected alternative, quadratic in the table: measured at 4.5 µs per new link with 8 192 entries and 27.5
µs with 65 535, so a session that reaches the full table pays that on _every_ subsequent `OSC 8`, forever, including the
ones the cap refuses.

Three bounds keep the table affordable, at the values [ipc.md](../../reference/ipc.md) "Semantic limits" carries, and
they bind in different regimes. The `u16` handle bounds the entry **count**: a table that large has no handle left to
hand out, so the row is refused rather than pushed to an index no cell could name. A byte budget bounds what those
entries may **hold**, because a URI is producer-chosen and the count alone bounds almost nothing: the OSC buffer limit
lets one reach several KB, and a full table of those measured hundreds of MB of live heap held for the session's life.
It charges the entry struct, its two string payloads, and the dedup slot that indexes it, written in `size_of` terms so
it follows `HyperlinkEntry` if a field is added, because a budget has to charge at least what the table really holds;
and it is sized to stay under what the default scrollback beside it costs, the rule the cluster table follows too, while
still admitting the whole id space for the URIs sessions really carry.

The third bounds one entry's **text**, and is a type rather than a third check because the halves of an anchor arrive by
two paths. The parser bounds what a producer emits (an over-long `OSC` body is truncated before dispatch), but a
`GridMsg::Hyperlink` off the wire is the peer's choice, and a client's own table would hold whatever length it sent. A
constructor precondition holds both paths to the same bound and `TryFrom` extends it to a restored dump, so the cap is a
property of the entry rather than of the code that happened to build it. The cap _is_ the parser's limit, so nothing a
producer can actually emit is refused: what the parser cannot deliver is exactly what the table need not hold.

Past any of the three the pen is simply not set, so the covered text prints as ordinary characters: a link loses its
target, never its content.

The table serializes as its bare entry vector, exactly as the style table does. The dedup index and the charged total
are derived, so they rebuild from the entries on load rather than riding the dump: a restored table whose total
disagreed with its entries would carry a budget that bounds nothing.

The shadow's mirror of this table needs no _count_ guard of its own, unlike the cluster table's: the `u16` id space is
already the bound, so one message naming a high id can force only that many empty slots however few entries the peer
then sends. It does hold each entry to the text cap and to the byte budget, both charged before the slot vector grows (a
refused entry leaves neither a charge nor a gap), since one message can otherwise carry a URI as long as the frame
allows.

_Revisit if_ a producer legitimately outgrows either bound. The count is the handle type, so raising it is a wire change
(`Cell.link` and every `Hyperlink` id); the budget is a constant, so raise that first and re-measure what the table then
costs against the grid beside it.

### Attributes

The pen a cell was written under is one packed struct of colors and style bits, specified as a wire shape in
[row-codec.md](../../reference/row-codec.md) "Attribute runs". Two things about it are decisions rather than layout. The
hyperlink id is deliberately _not_ in it: a link covers a span of cells whose pens vary freely, so folding it into the
pen would mint a fresh pen per link-and-color combination and defeat the interning below. And the struct is sized so
that "no attributes set" round-trips to zero bytes on the wire, which is what makes a blank row nearly free to ship.

### Style interning

The cell does not hold `Attributes` inline. It holds a four-byte `StyleId` handle into the grid's `style_table`, a
`Vec<Attributes>` with a dedup index, exactly mirroring the cluster / link / sizing registries. This is what puts `Cell`
at 16 bytes rather than the 28 an inline pen costs. Two motivations, both measured:

- **Scrollback residency.** The unified ring keeps a history-row count proportional to what scrolled off, so the
  per-cell unit cost multiplies across the whole live grid _and_ the retained history; dropping it from 28 to 16 bytes
  is the largest single resident-memory lever after the ring itself.
- **The ascii throughput floor.** With an inline pen, `print_str`'s bulk loop stores a whole 28-byte `Cell` per
  character, of which 16 bytes (the pen) are loop-invariant across a run; the read-free parse floor is store-width
  bound, not parse-CPU bound. Interning makes the per-char store a four-byte id copy, moving ghostty's shape into
  felis's hot loop.

Two invariants make the handle a drop-in for an inline pen, both enforced by the dedup index:

- **id 0 is the default pen.** `StyleId::DEFAULT` always resolves to `Attributes::default()`, seeded at construction and
  never evicted. So `Cell::BLANK` stays `const`, and a cell's blankness is a bare `style == DEFAULT` compare with no
  table lookup. No colored blank (BCE) can alias it, because a non-default background interns to a non-default id.
- **id equality iff pen equality, within one table.** Interning dedups, so two cells share an id exactly when they share
  a pen. The wire RLE run-key and the pipe/clipboard SGR re-emitter fall out to a `u32` compare instead of a 16-byte
  struct compare.

The pen is resolved to an id **once per SGR change**, not once per cell: the grid holds a `pen_style` handle kept
eagerly in sync with the pen (`resync_pen_style` runs on every SGR / DECSC-restore / alt-screen switch), and the many
per-cell write paths stamp that constant id. A per-cell refcount (ghostty's shape) is rejected precisely here: it adds
inc/dec traffic to `print_str`, the hot path interning exists to help, so a naive version regresses the benchmark it
targets.

"Once per SGR change" is the ascii win and the truecolor tax. A `DOOM`-fire or video framebuffer emits an SGR before
nearly every cell, so there `intern` runs per cell, where an inline pen would be a plain 16-byte store. The intern
hasher, not the store, is then the truecolor floor's cost center, so `style_table`'s dedup index uses **foldhash**
(`FixedState`, a fixed seed) rather than the standard library's SipHash-1-3. Measured, the read-free truecolor parse
floor sits ~14 % under the inline-pen alternative (with SipHash-1-3, ~25 % under). That is the accepted cost of the
ascii and residency win on the one workload interning cannot help: a genuinely fresh pen per cell has nothing to dedup.
The foldhash win is larger end to end than the parse floor alone shows, because the shadow screen re-interns every run
on decode, so the client pays the same hash per frame and foldhash cuts both sides.

A palette producer is the other half of that regime, and there the hash is avoidable. DOOM-fire emits `38;5` and `48;5`
as separate SGRs, so each one yields a new pen, but the pens cycle among a few hundred. `intern` therefore checks a
1024-slot direct-mapped cache of recent pens, keyed by the packed attribute words, before the dedup index: ~96 % of
DOOM-fire's interns hit it, and a fresh-pen-per-cell flood misses on every cell and pays one extra slot compare.
`compact` clears the cache, because the ids it holds are the ones compaction renumbers. Rejected:

- **Skipping the intern when the SGR left the pen unchanged**: DOOM-fire changes the pen on 99.7 % of its SGRs, so the
  pre-SGR compare is pure overhead there (+2 % instructions on the DOOM floor).
- **A single last-hit entry**: consecutive pens differ in both regimes, so it never hits.

The keys are producer-controlled, so the fixed seed is a real exposure and not a dismissed one. `Attributes` is exactly
what an SGR sequence sets, [security-model.md](../security-model.md) treats PTY output as hostile, and foldhash's fast
variant is not collision-resistant even under a random seed; with `FixedState` its constants are compile-time, so
colliding pens can be computed offline against a known build. A producer that streams them turns each `intern` from a
probe into a walk of the colliding bucket, and the parse pays that factor per cell.

What keeps it inside the standard security-model.md sets for resource exhaustion ("degrade felis, not the rest of the
user's session") is the sweep and the thread layout, not the hash. The sweep bounds the resident pen count, so the
longest bucket is bounded by the live pens plus the headroom below, on the order of 10^5 entries on a full default ring,
rather than growing with the length of the flood. And the cost lands where the flood does: the parse runs on the
session's own thread, so slowing it throttles the child producing the flood, which is the backpressure the design wants,
and it cannot starve the runtime workers other sessions' connections ride. The client's shadow re-interns the same pens
on decode, so an attached window pays it too, on the window already displaying that output.

The same truecolor producer grows the table without bound: it mints a fresh RGB pen per frame, and unlike the cluster
and link registries (bounded by content variety, and capped besides) the style table has no natural ceiling.
`Grid::gc_styles` reclaims unreferenced ids with a mark-compact sweep (roots: every live and saved-screen cell) and
remaps every surviving handle in place; the `pen_style` memo, often on no cell yet, is re-interned after a compaction.
The sizing registry is swept the same way and for a related reason: it mints per run, not per distinct value
([kitty-text-sizing.md](../protocols/kitty-text-sizing.md) "Reclaiming registry entries"). The cluster and link tables
stay append-only, since a remappable table and an incrementally streamed one are mutually exclusive (below). Reflow
needs no table walk either way: it moves handles _with_ their cells.

_When_ to sweep is not the grid's judgement. Both mechanisms take a threshold, and the policy (sweep past it, then raise
it) lives in a `TableGc` the driver owns, so it stays out of the grid's `PartialEq` and its dump. There are two drivers,
not one: the daemon's parse core sweeps after a parse chunk, and the client's shadow sweeps after an applied message,
because the shadow fills the same two registries from decoded rows (a pen per attribute run, a sizing entry per sized
cell) and a window has no session restart to clear them. One policy type rather than two sets of constants is what keeps
the two from drifting apart.

How far to raise it is the whole of the policy, and the two registries answer differently. The sizing raise doubles,
bounded: a sizing handle that cannot be minted has no fallback (the run draws unsized), so the sweep must stay in reach
and the ceiling is a real constraint. The style raise is uncapped (`StyleId` is a `u32`, and an unswept table is only
residency) and it leaves **headroom proportional to the cells the sweep just scanned**, not to the live set it found.

That distinction is load-bearing: headroom scaled to the live set is what makes a truecolor flood the losing regime.
Headroom equal to the live set amortizes the O(cells) sweep to O(1) per interned pen only while the live pens are
themselves proportional to the scan, which is true on an empty grid and false the moment scrollback fills with rows the
flood never wrote. A screen of unique pens over a full 10 000-row ring is ~11 k live pens against ~1.9 M scanned cells:
with the trigger at twice the live set, one sweep fires per ~11 k pens and each walks all 1.9 M cells. Measured on a 32
MiB per-cell-truecolor flood over a full ring, the sweeps are 1.41 s of a 1.57 s parse (90 % of the parse thread) while
the parse itself holds 208 MiB/s either way. Scaling the headroom to the scan instead (a sixteenth of it, so a sweep
costs at most sixteen cell visits per entry it admits) puts termbench's `sgr_fg_lines` at 229 MiB/s rather than 67, last
in the field to first. The headroom still clears the live set as well, because the opposite shape (a full screen of
distinct pens, where a sweep reclaims only what churned) would otherwise sweep several times per frame.

The mark pass that finds the roots is a dense `bool` per id rather than a `HashSet`: it visits every cell of the ring
while the ids it writes are `0..len` by construction, so hashing each one buys nothing. A sweep that reclaims nothing
also costs nothing beyond that pass: it reports that no id moved and the caller skips both the rewrite over every cell
and the dedup index rebuild.

#### The wire stays attribute-based

`StyleId`s are grid-local: the daemon and each client intern independently, so an id means nothing across the hop. The
row codec (`wire.rs`) therefore ships the **resolved `Attributes`** per RLE run: the daemon resolves
`StyleId → Attributes` on encode, and the shadow screen re-interns `Attributes → StyleId` into its own table on decode.
Clients, the protobuf wire, and every out-of-repo consumer (felis-web-component, felis-fcast, felis.el) never see a
`StyleId`; the id space exists only in each process's memory. Keeping ids off the wire is not merely convenience. An id
space that a GC remaps cannot also be streamed incrementally the way the cluster and link tables are: a client holding a
streamed id would see it invalidated under it. A remappable table and an incrementally streamed table are mutually
exclusive, and only the monotonic tables can stream.

Rejected alternatives:

- **Struct-of-arrays** (separate grapheme and attrs column arrays): fixes the ascii write but leaves attributes 16 bytes
  per cell resident (no residency win) and turns every cell access into a two-array gather.
- **Per-cell refcount** (inc on write, dec on overwrite): the reason above, it taxes `print_str`.
- **Interned ids on the wire with a streamed style table** (the cluster table's shape): saves nothing in memory over
  shipping the resolved attributes the RLE already dedups, and forecloses the GC (see the streaming/remap conflict
  above).

Revisit if: the sweep shows up on a real truecolor workload, in which case the next step is incremental compaction
rather than more headroom (the headroom divisor trades residency for parse time and sixteen is already where the two
meet); or the truecolor parse floor becomes a ceiling, in which case the next lever is the `intern` probe itself, not
its hash: a direct-mapped pen cache in front of the dedup map. foldhash already covers the hash cost, and a last-hit
cache measures useless because consecutive truecolor pens differ. Or a collision flood against the fixed seed is
demonstrated to cost more than the hash saves, in which case the lever is the standard library's random-seeded
SipHash-1-3: no seeding makes foldhash's fast variant collision-resistant, so the choice is the measured ~25 % floor
against a flood that a fixed seed lets an attacker precompute. Or residency is still the ceiling after interning, in
which case pack `Grapheme` to a `u32` (cell 16 → 12). That one is deferred: it needs a wire-repr conversion layer
(`Vec<Grapheme>` is serialized directly) and shrinks the cluster id space, so it should A/B on its own.

### Sizing handle

A cell that participates in an OSC 66 sized run carries a `NonZeroU16` handle into the grid's `sizing_table`; every cell
of the run carries the same one, and its position within the run is derived from that rather than stored. The handle
therefore does double duty (it resolves the sizing _and_ tells one run from the next), which is why this registry is
swept rather than deduped like its siblings. See
[kitty-text-sizing.md](../../explanation/protocols/kitty-text-sizing.md) "Reclaiming registry entries" for that argument
and [the reference twin](../../reference/protocols/kitty-text-sizing.md) for the wire behavior. Sizing is surfaced per
cell only for rows still on the grid; carrying it out to scrolled-out rows is deferred.

### Roles in a multi-cell run

A glyph wider than one cell is expressed in the grapheme enum: one primary cell holding the glyph, and continuation
cells marking the rest. One mechanism covers both East Asian wide glyphs (primary plus one continuation) and Kitty text
sizing runs of arbitrary M×N, which is why there is no separate role field.

Every spanned cell of a sized run carries the run's sizing handle, not just the primary. The primary can be several
columns and rows away, so a renderer that had to walk back to it would have to search; carrying the handle everywhere
makes the run's block extent readable from any cell it covers. Continuation cells are otherwise ordinary: an erase
blanks one like any other cell, and it forwards nothing to its primary.

### Image references

Cells carry no image data: direct placements (`a=T` / `a=p`) live in the image store's `Placements` side table
([image-store.md](image-store.md)), so placing or deleting an image touches no cell and damage is unaffected. The
renderer draws placements on top of the glyph layer, honoring each placement's z-order
([kitty-graphics.md](../../reference/protocols/kitty-graphics.md)). A placement is anchored to a row rather than to grid
content, and the placement table lives daemon-side, outside `Grid`, so `Grid::reflow` cannot move the anchors itself.
Instead it returns a `ReflowRemap` built from the same logical-line mapping that remaps prompt marks, and the daemon
replays every anchor through it (`Placements::remap_rows`): each anchor lands on the post-reflow row of the same cell,
survivors are re-stated to subscribers in full (a re-wrap moves each anchor by its own delta, so the uniform
`PlacementsShifted` directive cannot carry it), and an anchor whose line left the retained scrollback evicts exactly as
scroll-driven eviction does.

The one exception is Unicode-placeholder placements: those _are_ grid content (`U+10EEEE` plus diacritics in ordinary
cells), so they move with the text by construction.

## Cursor

The cursor is not a cell attribute; it lives beside the grid, together with the deferred-wrap flag that makes a print in
the rightmost column wrap on the _next_ glyph rather than immediately. Keeping it out of the cells is what lets a
cursor-only move leave damage empty: nothing on any row changed, so nothing ships
([damage-tracking.md](../rendering/damage-tracking.md)).

The daemon owns the cursor and the client renders it at draw time. Blink is the exception in the other direction: the
daemon ships the style and the blink flag, never blink frames, because the phase belongs to the clock that paints
([pipeline.md](../rendering/pipeline.md) "Frame pacing").

## Occupancy watermark

Each _physical_ row of the live grid carries a `u16` watermark (`Grid::occupancy`, indexed like `soft_wrap`) that is the
authoritative live extent of the row: columns `[0..occupancy)` are live content and columns `[occupancy..cols)` are
undefined, holding whatever bytes a previous tenant of the physical slot left behind. Any cell write raises the
watermark (raising is always safe); a whole-row default blank (the `rotate_region` recycle, or an erase-to-row-end under
the default pen) drops it. A pen carrying any non-default attribute (BCE) is live content, so it pins the watermark at
`cols` and materializes the cells; colored blanks are never mistaken for absent ones.

The model divides responsibility between readers and writers, and both halves are needed for the elision below to be
safe. Readers clip at the watermark: a column at or past it reads as a shared blank, row content and the push into
scrollback carry only the `[0..occupancy)` prefix ([scrollback.md](scrollback.md) "Structure"). Writers that land past
it blank the gap `[occupancy..col)` first, so raising the watermark over a recycled slot never exposes the previous
tenant's tail; the cell-shifting editors, which reason over the whole row rather than a prefix, materialize the tail
before shifting so a `copy_within` cannot drag stale bytes into view.

Making occupancy authoritative lets the default-pen recycle skip work rather than do it cheaply. `rotate_region` drops
the watermark to 0 and leaves the existing bytes in place, eliding the `cols`-cell memset that otherwise runs per line
feed. That memset matters whenever the parser is the bottleneck, which holds for both builds: the debug build is
parse-bound outright (halving the per-line scroll work roughly halves real `cat`-of-a-large-file wall-clock), and the
release build's `cat` is parse-floor-bound because the parse runs on the PTY parse thread
([pipeline.md](../rendering/pipeline.md) "Demand-driven emission" owns that topology), so the per-line scroll cost is a
visible slice of that floor.

Rejected alternative: blank the leading gap inline in `print_str` rather than forking to a cold copy. The bulk write
loop's throughput depends on the compiler hoisting `self.cells`'s data pointer and eliding bounds checks across the
whole run, and any `self.cells` write or call preceding the loop in the same function body defeats that at compile time
even when the gap never fires at runtime (measured ~30% on plaintext). A read-only watermark probe is free, so the fast
path keeps it and only the rare gap case pays a call. `cat` through a tty gets `\n`→`\r\n` from `ONLCR`, so its runs
restart at column 0 and never gap; only a raw producer emitting bare `\n`, or explicit cursor addressing, reaches the
cold path.

The unified live-grid + history ring ([scrollback.md](scrollback.md) "Unified viewport-into-history ring") composes with
the watermark rather than subsuming it: the ring makes eviction a base bump (no row copy), while the watermark is what
lets the recycled row skip the memset. Without the watermark every recycle pays the blank; without the ring every
eviction pays the copy.

## Wire shape and packing

How a dirty row ships to the client is part of the wire contract. The row-delta message that carries it is in
[ipc.md](../../reference/ipc.md) "Row codec"; the bytes inside it (the cell records, the attribute RLE, and the OSC 66
sizing side-band) are specified language-neutrally, with golden vectors, in
[row-codec.md](../../reference/row-codec.md).

## Reflow on resize

A column-count change re-wraps the primary screen (REQ-604); the algorithm, the stitch it runs over, and the cursor,
mark and round-trip decisions are in [scrollback.md](scrollback.md) "Prior art and alternatives considered". What it
costs a cell is the sized run: one the re-wrap cannot fit is discarded and re-emitted by the producer rather than
re-fitted in place, per [kitty-text-sizing.md](../../reference/protocols/kitty-text-sizing.md).

## Why the daemon owns this

- The grid is read far more often than written (every render pass on the client), so it must live close to the parser.
- The grid is the natural unit of _rehydration_: handing a snapshot to a freshly-attaching client is "send the grid plus
  the image store."
- The grid must persist across client disconnects, by definition.

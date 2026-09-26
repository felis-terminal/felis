---
title: Scrollback
sidebar:
  order: 2
---

Scrollback is the off-screen history of the primary grid. It exists only on the daemon. The alternate screen has none.

## Structure

History and the live viewport share one flat cell ring. `Grid`'s `cells` holds up to `rows + cap` physical rows; a
`base` offset marks the top of the viewport, and the rows just below it in ring order are the retained history. The row
that scrolls off the viewport top _is_ the youngest history row, in the same storage, so eviction is a `base` bump, not
a copy (`crates/felis-grid/src/screen.rs`).

Logical viewport row `r` resolves to physical ring row `(base + r) mod phys_cap`; history row `-k` (1 = youngest) to
`(base - k) mod phys_cap`. History and viewport occupy one contiguous ring window `[base - history_len, base + rows)`,
so their combined length never exceeds `phys_cap`. A full-screen scroll off the top advances `base` and drops the
occupancy watermark of the row now recycled at the viewport bottom: no cells move, and once history is at `cap` that
watermark drop _is_ the eviction of the oldest row (the window has wrapped fully around). Why one ring rather than a
separate history store is recorded in ["Unified viewport-into-history ring"](#unified-viewport-into-history-ring) below.

Each row stays contiguous, so the print fast path keeps its hoisted per-row-chunk loop and pays only the ring resolution
on the row base. The live window is contiguous in ring space (a monotone `base`, no permutation); a sub-region scroll
(DECSTBM, IL/DL), which cannot be a whole-window shift, rotates a _scroll band_ over the region instead of moving rows:
an O(1) counter bump, recorded in ["Interior scroll band"](#interior-scroll-band) below. Resize / reflow re-lay the ring
out canonically (`install_ring`). One eviction copy remains: a scroll region anchored at the top but not covering the
full screen (`DECSTBM` with `top = 0`, `bottom < rows - 1`) pushes to scrollback but cannot be a pure base bump, because
rows below the region must stay; that cold path (`scroll_partial_top_into_history`) evicts its top rows through the
full-screen path and then shifts the rows below the band back down, the same occupancy-clipped move. Because history and
the live screen are one surface, the scrollback / live seam that `search`, `reflow`, the pipe sources, and the viewport
composition read is a plain window read, not a join of two stores.

The physical buffer grows lazily: it starts at the live `rows` and doubles toward `rows + cap` only as history fills,
re-laying the ring out canonically on each growth (amortized O(1) per push). This is a requirement, not an optimization.
Initializing all `rows + cap` rows up front writes every one of them, so an idle session would be resident at the full
cap: at 16 bytes per `Cell` and a 137-column grid, ~21 MiB against the ~75 KiB a bare 137×35 viewport actually needs.

The _capacity_ is nonetheless reserved up front, and only the length grows. Reserved address space and resident rows are
not the same cost: `Vec::with_capacity` on a buffer this size is one `mmap`, whose pages stay unfaulted until written,
so the full 21 MiB is address space while residency tracks the rows history has actually reached, and the ring never has
to move. Growing by allocating the next size up and copying into it (the rejected alternative) leaves the allocator
holding every intermediate buffer: after a 200 k-line flood the daemon holds 11.2 MB of freed ring buffers it has not
returned to the OS against 12.2 MB of ring actually live, which is _half the flooded footprint being garbage_ (measured
at 24×80, `vmmap`'s `MALLOC_LARGE (empty)`; the same shape at 82×137 with a client attached reads 51.7 → 28.8 MB
flooded, idle unchanged). Returning the pages instead (`malloc_zone_pressure_relief` and its Linux twin) is rejected: it
is per-platform `unsafe` FFI to undo an allocation the ring never needed to make.

With the capacity reserved, a growth is a rotation rather than a copy. The one caller grows only a ring that is exactly
full (`history_len + rows == phys_cap`), so every physical row is live and canonical order is one `rotate_left` away,
needing no scratch buffer. The `soft_wrap` and `occupancy` side-arrays are rebuilt on each growth; at one bool and one
`u16` per row against `cols` cells, their churn is three orders below the cells' and not worth a second reservation.

Readers go through a borrowing `ScrollbackView` (`Grid::scrollback()`), which clips each history row at its occupancy
watermark ([grid-and-cells.md "Occupancy watermark"](grid-and-cells.md#occupancy-watermark)): a history row is a former
viewport row left in place, so its tail past the watermark holds undefined bytes from the zero-copy recycle. Every
consumer already trims trailing blanks: the daemon copy / pipe sources (`row_text_trim` / `encode_row`), seam search,
and reflow, which re-pads history rows to full width locally so a bare-LF column carry still reconstructs. On the
alternate screen `cap = 0` and the live ring is a bare viewport; a scrollback read routes into the saved primary buffer
rather than the live (alt) fields, and the `SavedScreen` snapshot carries `base`, `phys_cap`, `history_len`, and `cap`
across the toggle, plus the `rows` and `cols` they are relative to, which a resize under the alt screen moves the live
grid past ("Reflow on resize", D4).

## Soft wrap

A logical line of shell output may span N rows because it wrapped at the column edge. Each row carries a
`soft_wrap_continued` bit: true when the row is an autowrap continuation of the row above. On the live grid the bit is
indexed by _physical_ row (`Grid::soft_wrap` in `crates/felis-grid/src/lib.rs`), a parallel ring to `cells`, so a scroll
that only bumps `base` carries every row's bit with its cells for free; a row that scrolls into history keeps the same
physical slot and so keeps its bit with no move at all.

The bit records how content **arrived**: it is set when a deferred or wide-glyph autowrap completes, cleared when the
row is wholesale blanked, and deliberately untouched by partial overwrites; it is not a re-derivation of what the row
currently holds. Four consumers read it: search stitches logical lines back together (below), `?45` reverse-wraparound
retraces wrapped lines (`Grid::step_cursor_back`), triple-click logical-line selection on the client, and reflow
stitches each soft-wrapped line before re-wrapping it at a new width (`Grid::reflow`, "Reflow on resize" below). The bit
rides the `RowDelta` row codec (a header flag bit, normatively [row-codec.md](../../reference/row-codec.md), implemented
in `crates/felis-grid/src/wire.rs`; every row-carrying path: delta, batch, rehydrate, composed browse viewport), and the
shadow screen mirrors it on apply.

Stitching itself is one rule in one function. `felis_grid::logical_line_spans` joins rows tied by the bit into the
logical line the program printed, with no newline across the wrap edge, and walks scrollback and the live rows as one
sequence, so the stitch crosses the scrollback / live seam. Every text-reconstructing consumer routes through it
(`search`, `capture` in text mode, the `pipe` encoders, and the client's `Selection::extract_text`), so a wrapped URL or
path reconstructs identically everywhere; the per-consumer sections below state only what each does with the stitched
line.

## Search

Search runs daemon-side because the data is daemon-side (REQ-607), and it matches per **logical line** over the shared
stitch ("Soft wrap" above). That is the decision the rest follows from: a needle spanning an auto-wrap edge is found, a
regex `^` anchors at the true line start rather than at every visual row, and a logical line whose head scrolled into
history while its tail is still on screen matches whole. The query modes, the reply fields, and the negative-from-top
`line_index` convention (REQ-608) are in [the IPC wire reference](../../reference/ipc.md#search-kind--9); the surfaces
that drive it are in [cli.md](../../reference/cli.md) and [keybindings.md](../../reference/keybindings.md). The grid
itself is never modified by a search.

The walk runs in **bounded slices**, which is what makes the stream cancelable: the daemon takes the session lock for
one slice at a time rather than for the whole scan, so a cancel lands between slices instead of after the last hit, and
a search over ten thousand rows never parks the session behind itself. Slices resume against a cursor carrying two
numbers, how many logical lines were visited and how many the surface held when the last slice ran. The second is what
makes mid-search growth measurable: the walk is newest-first, so a line printed since the last slice shifts every older
line's ordinal by one, and a cursor without it would re-report the lines that shift pushed past it. Materializing every
hit and then emitting the pile is the rejected alternative: it holds the lock for the whole scan, which leaves a cancel
nothing to land between.

**Limitations (intentional):**

- **The alternate screen is not searched**: alt content (vim, less) is a transient surface the program repaints at will;
  its rows are no continuation of scrollback and its `soft_wrap` bits describe the alt screen, not the saved primary.
  The scrollback half still matches while a program holds the alt screen.
- **Trailing-blank trim** on the logical line's tail: a needle of `"hi"` does not surprise the user by matching `"hi"`
  followed by 78 cells of `Empty` (which would, post-decode, look like `"hi" + 78 spaces` and be invisible). Leading and
  interior blanks are preserved; rows that wrap into the next drop only their trailing `Empty` cells (the unfillable
  slot a wide-glyph wrap leaves). This is an exact rule, not a heuristic, since printed spaces decode as `Ascii(' ')`
  and survive.

## Capture

Capture is the **pull** half of the region surface: it reads a region to the caller's stdout, the headless complement of
the keymap `pipe` action's **push** to an in-window sink ([input.md](../input.md) "Why pipe, not a copy mode"). The
verb, its flags, and its output framing are in [cli.md](../../reference/cli.md); the wire conversation is in
[the IPC wire reference](../../reference/ipc.md#region-kind--6). Four decisions shape it.

**It is daemon-side, mirroring search.** The data is daemon-side and search already proves the shape: attach, send one
request, read a framed reply stream, detach. The daemon encodes each row to text itself, so the CLI reconstructs no grid
and carries no cell codec. Like search the reply is a cancelable stream, and the row window is pushed into each source's
iterator rather than applied to a materialized region, so a capture of a huge scrollback pays for the rows it ships.

**Its sources are the pipe action's sources, token for token.** One wire enum serves both, so a user who learns which
region a chord sends learns which region the CLI reads. The two readers differ only in reply shape: `pipe` asks for one
stitched blob, which is what a pager or an argv sink consumes, while capture asks for the same region row by row,
because a structural consumer wants each row's index and soft-wrap bit and a stitched blob flattens both away. Why the
richer source lands on capture rather than a separate `sessions pipe` verb is recorded in
[control-surfaces.md](../architecture/control-surfaces.md) "`pipe` is keymap-only, and stays a separate verb from
`capture`".

**Row indices live in the region's own coordinate space.** Scrollback rows are negative (REQ-608), live rows keep their
grid coordinate, and a mark range counts from zero at its first row, because a mark range is anchored to a command
rather than to the grid and a grid-absolute index there would name a row the range does not have. _Revisit if_ a
consumer needs a mark range's rows tied back to absolute grid lines, which would force the wire to carry them.

**A caller never has to choose between color and structure.** Asking for both runs both encoders and each row carries
its plain and its SGR form side by side, because raw escapes would corrupt the structural field and dropping one form
would make the picker that lists rows structurally and previews them in color read the session twice.

Tailing a source to its last N rows is a **filter, not a renumbering**: surviving rows keep the indices the untrimmed
capture would show, since an index that changed meaning under a flag would make the stable-machine-output guarantee a
lie. The cut happens daemon-side for every source, because the point of a tail is to not ship rows the client would
drop. A logical line the cut bisects prints only its surviving rows, which is honest for a tail view.

**Rejected: a viewport-walking client** that drives `InputMsg::Viewport` upward in grid-height steps and stitches each
recomposed window. The viewport path is built for an interactive GUI under pull pacing: the daemon _withholds_ row diffs
while the viewport is non-zero and only flushes on snap-back (a frozen-view policy matching kitty/wezterm), so a
headless CLI client would have to fake the pull cadence and de-overlap window seams. A single synchronous daemon walk is
simpler and round-trip-free. **Revisit if** an interactive in-window "export visible + scrollback" action ever needs the
same dump without a CLI round-trip: it would reuse `region_rows_window` directly.

## Piping to an external command

Scrollback is the primary source for the **pipe action**: a binding ships the retained scrollback plus the live screen
to a pager, the escape hatch kitty (`show_scrollback`, <https://sw.kovidgoyal.net/kitty/overview/>) and foot (the
`pipe-*` family, <https://man.archlinux.org/man/foot.ini.5.en>) ship for heavy scrollback work. The action surface, the
temp-file carrier, the transient session a `Command` target runs in, and the environment that transient carries are in
[input.md](../input.md) "Action mapping"; the bindings are in [keybindings.md](../../reference/keybindings.md). What the
buffer contributes is the region and its encoding.

### Encoding: plain by default, `ansi` for color

The `ansi` flag on the request picks the per-row encoder. Default (`ansi = false`) is plain text via
`felis_grid::row_text_trim`; `ansi = true` reconstructs color via `felis_grid::row_ansi`, which walks the row's cells
emitting CSI SGR sequences on attribute change and a trailing `SGR 0` per line, so color survives into `less -R` /
`bat`.

Plain is the default because the encoding is a property of the **consumer**, and most pipe consumers re-parse the text
as _data_: a hint picker (`thumbs`, `urlscan`), an editor, a text tool; embedded SGR escapes corrupt their parse. Only a
consumer that renders the text _back to a screen_ wants color: a color pager (`less -R`, `bat`) or `fzf --ansi`. So a
pager binding sets `ansi = true` (the `-R` and the flag go together), while any binding that omits it defaults plain:
the fail-safe direction, since plain into a color pager merely loses color, whereas SGR into a data tool produces
garbage. With no default `pipe` chord shipped, every default is uniformly plain; `ansi = true` appears only where a user
(or the manual's sample) opts a pager in. This mirrors `felis sessions capture --ansi` (default off), so the two
surfaces share one mental model. The live risk is drift: if the attribute set grows an SGR dimension the encoder does
not emit, pager output diverges from what the window renders. That risk is why the encoder is shared rather than copied:
the pipe action, `capture --ansi`, and the host-terminal clients all draw on `felis-grid::ansi`
([architecture/overview.md](../architecture/overview.md) "Shared wire knowledge across the satellite clients"), so one
addition serves every consumer instead of leaving the slowest copy silently monochrome.

### Logical lines: soft-wrap stitching

Both encoders join rows with `\n` **except across a soft-wrap edge**, via the shared stitch ("Soft wrap" above), so a
logical line the program wrapped at the screen edge (a URL, a path) reconstructs whole. Without this a downstream hint
picker sees a wrapped match split in half and cannot act on it.

One residual limit, in the `ansi = true` case only: each `row_ansi` row closes with `SGR 0`, so a stitched logical line
carries a reset mid-line. Harmless for a color pager. A picker that strips SGR before matching (kitty's hints kitten
does; `kovidgoyal/kitty` `kittens/hints/marks.go` `process_escape_codes`) rejoins the chars; a picker that does not
(`thumbs` skips SGR but does not bridge it, `urlscan` ignores it) may still split a _colored_ wrapped match. The common
default-plain case is clean for every picker, which is why plain is the default for the hints flow.

### Selection source: client-side serialization

Four of the five `pipe` sources (`scrollback`, `visible`, `command_output`, `last_command`) are daemon state, serialized
daemon-side by the encoders above. The fifth, `selection`, is **not** daemon state: the grid the daemon owns carries no
selection; the selection is a client-local gesture over presented pixels (principle 3). So the **client** serializes it,
reusing `felis_client_core::Selection::extract_text`, which routes through the shared stitch ("Soft wrap" above), then
feeds those bytes straight to the sink; there is no region to ask the daemon for, so the chord skips the round trip
entirely. Placing the read on the client is the boundary, not a convenience: asking the daemon to track a per-client
selection would put presentation state on the wrong side of principle 3, and would not round-trip cross-host anyway
(each attached client has its own selection).

### A region too large to carry

A 10 000-row scrollback of wide, heavily colored cells (REQ-605, REQ-605a) with `ansi = true` serializes past the 64 MiB
framing backstop, and a body that big would be refused by the daemon's own writer, costing the requesting window its
connection in the middle of a chord (REQ-105). So the reply carries a ceiling of its own at half the backstop
(`MAX_REGION_REPLY_BYTES`, [ipc.md](../../reference/ipc.md) "Semantic limits"), and a region past it is trimmed to its
youngest lines rather than sent whole.

Trimming from the head is what the sinks make right: the youngest lines are the ones the chord was pressed for, so a
pager or editor opened on them shows the reader what they asked to see. A typed refusal naming the size is the
alternative, and it answers a question nobody asked: a chord is not a size query, and a refusal leaves the reader with
nothing to read and no way to ask for less. What the trim does cost is the viewport anchor, which names a line in the
head it removed, so the position is dropped rather than adjusted and the child starts wherever it starts
([input.md](../input.md) "Opening the pager where the user was looking"). The cut lands at a resumable boundary, a line
start or a scalar start outside any escape sequence, so what survives is still a decodable region rather than a fragment
split mid-sequence. The row stream behind `felis sessions capture` reads the same region without the ceiling, so the
export the blob cannot carry still has a route.

The `paste` sink is bounded tighter than the other two, at `MAX_PASTE_BYTES` ([ipc.md](../../reference/ipc.md) "Semantic
limits"): its region goes back out as an `InputMsg::Paste`, and the budget above admits twice what one of those may
carry. It is refused with the same notice a clipboard paste of that size draws, since the sink is what makes the cap
reachable without a human holding a 16 MiB clipboard.

The OSC 133-keyed pipe sources (`CommandOutput` = last `C→D` range, `LastCommand` = last `B→D` range) extract their rows
the same way, but across the scrollback/screen boundary: prompt marks carry **absolute line coordinates**
(`Grid::scrollback_total_pushed + row` at record time) so a command whose output has scrolled into history is still
recoverable. `Grid::locate_line` maps an absolute line back to a current scrollback index or screen row. See the
prompt-mark coordinate model below.

## Prompt marks (OSC 133)

Each `OSC 133` boundary records a `PromptMark { line, kind, exit_code }` on the grid. `line` is the **absolute**
coordinate `scrollback_total_pushed + cursor.row` at record time. `scrollback_total_pushed` is the cumulative count of
rows ever pushed into scrollback; it only ever increases (it is never reset), so a mark's `line` stays stable across
later scrolls and resize.

`Grid::locate_line(line)` inverts the coordinate to a current `MarkLocation`:

- `Screen(row)`: the absolute line is on the live grid.
- `Scrollback(idx)`: it is a retained scrollback row (`idx` indexes `ScrollbackView::row`, 0 = oldest retained).
- `Evicted`: it has scrolled past the retained ring and is unrecoverable.

`prompt_marks` is **front-pruned**: marks are pushed as boundaries fire, resize never touches them, and a mark is
dropped once its line evicts past the retained scrollback ring or is cleared by ED 3. `prune_evicted_marks` runs on each
scrollback eviction and on ED 3; it drains the leading marks whose `line` has fallen below the oldest retained row and
adds their count to `prompt_marks_pruned`. The vector therefore stays bounded by the number of _retained_ marks, not by
session lifetime.

Front-draining breaks the naive "mark index = position in the vector" assumption, so the daemon's streaming cursor
counts in a **never-pruned ordinal**: mark `i` in the live vector carries absolute ordinal `prompt_marks_pruned + i`. A
per-connection cursor stored as that ordinal survives a prune untouched, and the daemon subtracts `prompt_marks_pruned`
to index the retained slice (`Grid::prompt_marks_pruned`, consumed in `crates/felis-daemon/src/serve/streaming.rs`). A
mark pruned before a subscriber ever streamed it is skipped, never re-sent as a different mark.

### Prompt-jump navigation

`Grid::prompt_jump_target(viewport, direction)` resolves a previous/next prompt jump to a scrollback viewport entirely
on the daemon. The matched `PromptStart` mark is top-aligned in the composed view; the result is hard-clamped at the
history ends with **no wrap** (returns `None` when it would not move). Jumps are disabled on the alternate screen and
only consider still-retained marks (evicted marks are skipped). Deferred refinements (e.g. per-output-block jumps) are a
revisit.

## Selection vs scrollback

The selection model is client-side pure data (`crates/felis-client-core/src/selection.rs`): an anchor/extent pair in
0-based shadow-grid coordinates, owned by the App. The daemon's grid and scrollback carry no selection state and nothing
crosses the wire ([input.md](../input.md)); when a keybinding pipes the selection, the client serializes it itself (see
"Selection source: client-side serialization" above).

## Capacity and eviction

- Capacity is in lines. A session takes `DEFAULT_SCROLLBACK_ROWS = 10_000` (`crates/felis-grid/src/lib.rs`); per-session
  sizing is not user-configurable. Capacity is a construction parameter rather than a constant because a screen that is
  a mirror retains nothing: the client's shadow builds at capacity 0, since the daemon composes the browse viewport and
  ships it as ordinary rows. At capacity 0 a row leaving the viewport top is dropped instead of evicted into history,
  and the ring reserves only the live viewport. The alternate screen runs at capacity 0 for the same reason: it has no
  history of its own.
- Memory is the dense cell array: a row is a `[Cell]` at 16 bytes per cell (`size_of::<Cell>()`), with the pen interned
  to a four-byte handle rather than carried inline ([grid-and-cells.md](grid-and-cells.md#style-interning)), so the
  default 10k rows at 96 columns costs ≈ 15 MiB per session, the figure the 100 000-row rejection below scales by ten.
- Eviction is FIFO. There is no "important lines" policy; users wanting permanence should pipe to a file.

## Prior art and alternatives considered

felis's storage choices are the mainstream terminal consensus; the few places it diverges are called out. This records
why each was chosen over the alternatives, so the rationale lives with the design rather than in a separate survey.

**Storage: row ring, not slab or paged log.** A fixed-size ring of dense rows is what Kitty, Alacritty, WezTerm, foot,
and Ghostty all use: O(1) append and random access, contiguous and cache-friendly, predictable eviction. The
alternatives buy memory felis does not need: a pointer-ring-plus-slab (tmux) compacts short lines but adds indirection
and resists mmap; a piece table gives up the O(1) cell lookup the renderer depends on (no major terminal uses one); a
paged or disk-backed log (iTerm2 optional, tmux logging) bounds memory regardless of capacity but costs a syscall per
read and a versioned on-disk format. At felis's scale none of it pays for itself.

**Default capacity: 10 000 rows.** Matches Alacritty and Ghostty; Kitty and tmux default to 2k, foot and iTerm2 to 1k. A
larger default is rejected because the daemon keeps _every_ session's scrollback resident for the session's whole
lifetime (sessions outlive windows), so the worst case multiplies by session count, not pane count: at 100 000 rows ≈
150 MiB per session, a handful of long-lived sessions would dominate RSS for history almost nobody pages through.

**Row representation: dense, not sparse.** A row is a flat `[Cell]`, not a sparse `(col, Cell)` list. Sparse storage
saves memory on blank-heavy rows but costs more allocations and a slower lookup; the modern consensus is dense, and the
predictability is worth more than the savings.

**Search: scan per query, no index.** Substring and regex run over the rows on each query, as in Kitty, WezTerm, iTerm2,
and tmux `copy-mode`. A persistent trigram or suffix-array index would make queries cheap but costs an expensive build
and a per-append update; at 10k lines (a few MiB) even a naive scan is sub-100 ms, so an index is not worth its
maintenance. Build one only if profiling shows search dominating a real workload.

**Reflow on resize: re-wrap every soft-wrapped line.** The landscape splits: Kitty, WezTerm, and foot re-wrap every
soft-wrapped logical line at the new width; Alacritty reflows only on grow; tmux defaults to none. Not reflowing looks
broken, so `Grid::reflow` re-wraps: it walks scrollback then the live rows as one combined surface (the same walk
`Grid::search` uses), stitches each soft-wrapped logical line back to its pre-wrap length, and lays it out again at the
new column count; a hard-wrapped line (one that ended in an explicit LF) stays a separate logical line. This is
`O(rows × cols)` at resize, an acceptable cost. `reflow` is the daemon's primary-screen resize entry point;
`ScreenBuffer::resize` (trim and pad) is the operator for the shadow and the alternate screen.

Three decisions fix the reflow's edges:

- **Cursor and marks ride their logical line (D1).** Reflow preserves the cursor's _logical_ position: column K of
  logical line N lands wherever that column falls at the new width, not at a fixed physical row. Each prompt mark is
  remapped onto the new first physical row of the logical line it sat on, and a mark whose line evicts is pruned.
  Anchoring to the physical row instead would scatter the cursor and every mark on any width change. Kitty
  direct-placement anchors ride the same mapping: `reflow` returns a `ReflowRemap` and the daemon replays each anchor
  through it (REQ-604; see [grid-and-cells.md](grid-and-cells.md) "Image references"); a resize deferred under the alt
  screen (D4) delivers the remap at stream position via `PtyEffect::PrimaryReflowed`, after the screen switch restores
  the saved primary placements.
- **The round-trip is exact only without eviction (D2).** `W → W' → W` restores the original layout only while no
  scrollback eviction occurs. Narrowing produces more physical rows, and once the ring is at `cap` the oldest rows evict
  first (as in Kitty), so a narrow-then-widen that overflowed the ring cannot resurrect the evicted head. This is the
  scoped limit of the round-trip guarantee (REQ-604, REQ-407); the proptest that pins it
  (`reflow_width_round_trip_restores_plain_text`) holds the input under the cap so no eviction fires.
- **The alternate screen keeps trim/pad (D3).** On the alt screen `reflow` delegates to `resize`. Alt rows are a
  distinct surface with no shared scrollback to stitch against, so re-wrapping them against the primary's history would
  join unrelated content; trim and pad is the right operator there.
- **A resize under the alt screen defers the primary's re-wrap (D4).** While a TUI holds the screen the primary is not
  the live surface, and D3's reasoning cuts both ways: the alt rows occupying the live fields are not the primary's
  history to re-wrap. The saved primary therefore waits at its snapshot geometry and re-wraps in one pass when `?1049l`
  restores it. The deferral is also what keeps its history: a trim reads viewport rows only, so trimming the snapshot in
  lockstep would drop every retained row. Because the snapshot's width outlives the live grid's, reads that route into
  it (the scrollback a search or a capture sees while the TUI is up) resolve rows through the snapshot's own width. A
  burst of resizes under the alt screen costs one re-wrap, not one per resize.

A multi-cell sized run (OSC 66) that the re-wrap cannot fit is discarded, and the producer re-emits it on the new
geometry: the same keep-or-discard rule `resize` already applies (REQ-406/REQ-407), not a re-fit in place. The rewrap
lays cells out one at a time and does not retain the run's source text, so a run split across a new wrap boundary cannot
be re-fitted from the grid alone. _Revisit if_ producers need a sized run preserved across a wrap boundary, which would
mean retaining run source text on the grid.

**Coordinate space: negative-from-top.** Row 0 is the top of the visible grid, −1 the line just above, −N further back
(REQ-608). The alternative, absolute line numbers from session start, is used by some logging-style terminals where
numbers persist across scrolling; felis does not need that, and negative-from-top maps straight to a scroll offset.

**Persistence: across client disconnect, not daemon restart.** Most terminals keep scrollback in RAM and lose it on
exit; iTerm2's optional disk overflow is the exception. felis is RAM-only, and because the daemon owns the PTY a restart
loses the session itself, not just its history. _Revisit if_ scrollback capacity becomes configurable and sessions run
with more than one million lines, where on-disk overflow starts to pay for its format.

## Unified viewport-into-history ring

The obvious storage shape is a split: a flat live buffer plus a boxed history ring, with each row that scrolls off the
top eagerly copied across (a `push_prefix`). felis instead unifies them into the single flat ring "Structure" above
describes. This section records why the unified ring, the most invasive option, wins over the cheaper fixes.

Under the split, plaintext scroll on the primary screen trails ghostty about 1.6× (vtebench `scrolling`, matched 137×35
grid), and a bisection of `scroll_region_up` localizes the whole primary-vs-alt gap to that one eager copy: ~9 ns per
line even when the occupied prefix is a single cell, about half cold-cache miss on the destination slot, half the copy
call itself. ghostty and alacritty pay nothing here because their viewport is a window into the history ring: the row
that leaves the viewport top is already the newest history row, in the same storage, so eviction is pointer motion.
felis's grid floor is not the problem: on the alt screen, the same rotate with no push already beats ghostty's
end-to-end throughput. What the split lacks is the structural property that the evicted row is never copied, and only
the unified ring supplies it.

Two cheaper fixes are measured and rejected:

- Shrinking `push_prefix`'s per-call cost (replacing its modulo with a compare-and-reset branch) regresses the primary
  floor 16%; the path is as inlining-sensitive as the print fast path, and the division is not the cost.
- A base-offset ring for the live window alone (an O(1) full-screen scroll that leaves history untouched) costs 1.1% on
  the ascii print fast path and buys only the ~3.5% rotate memmove, on the alt path that already wins. The ring index is
  worth paying for only if it also eliminates the copy, so the offset ring is not a standalone stage: it is one facet of
  the unified ring.

The unification is invisible on the wire: `Grid`'s storage is private and the wire ships logical `RowDelta` / `Scrolled`
messages, so the shadow screen, the renderer, and every out-of-repo consumer see the same row stream either way.

## Interior scroll band

A DECSTBM / IL / DL scroll cannot be a `base` bump (it shifts only a sub-range of the viewport), so the ring resolves
those through a second rotation layered on the same seam: `(band_top, band_len, band_rot)` on `Grid`, applied inside
`phys_row_at` to rows within the band before the `base` offset. `rotate_region` arms the band on the scrolled region and
bumps `band_rot` instead of moving rows; only the recycled rows are blanked (the same occupancy-watermark recycle as the
full-screen path). The band also serves the rare full-screen rotate that cannot bump `base` (scroll-down / RI with
retained history below the viewport, where the bump would resurrect history rows).

The band is _materialized_ (physically reordered back to identity, `O(band)` occupied prefixes, then deactivated)
exactly where raw ring order is assumed: a `base` shift (`scroll_full_screen_into_history`, the full-screen arm of
`rotate_region`), the alt-screen snapshot / restore pair (`SavedScreen` records `base` but no band, and its compaction
helpers rotate by `base` alone), and on a _change_ of scrolled region. The relayout paths (`install_ring`,
`drop_history`, resize / reflow) copy the viewport out through the band-aware `phys_row_at`, so their canonical rebuild
materializes as a side effect and they only reset the band fields. `grow_ring` rotates in place instead of copying out,
so it has no such side effect and materializes explicitly first. A sustained same-region flood never materializes;
alternating regions (IL at a moving cursor) re-materialize per switch, which costs one per-row region move, so the worst
case matches what a move-per-line scheme pays on every scrolled line.

The rejected alternative moves the region's rows per scrolled line, occupancy-clipped. Clipping alone leaves the
interior scroll ~8× behind ghostty on vtebench `scrolling_top_region`, because the residual is per-row move bookkeeping
that no clipping removes: parity needs the scroll to stop touching rows at all. Measured (parse-into-grid floor, 137×82,
`\x1b[2;82r` + 2-byte lines): the band reaches ~153 MiB/s against the per-line move's 12.3 (~12×), bringing the interior
region to parity with the full-screen sparse-scroll floor (~140 MiB/s at 137×35); end-to-end in a live window a 30 MB
flood takes 0.32 s against 1.92 s (~6×), with rendering adding nothing on top (raw ≈ DECSET 2026). The ascii print floor
and the full-screen scroll floor are unregressed: the band check in `phys_row_at` is gated on an interleaved floor A/B
(n=24 pairs, distributions congruent).

Two codegen traps, both measured on the sparse full-screen scroll floor (the path that pays the band checks without ever
arming a band):

- The in-band wrap stays a `%` even though `band_rot < band_len` would let a conditional subtract avoid the `idiv`: the
  subtract form's extra inline code bloated `phys_row_at`'s many inlined copies and read −8%; the `idiv` sits only on
  in-band resolutions, paid once per printed row of a region flood.
- `materialize_band` is a `#[inline]` guard over a `#[cold]` reorder body: folded into one function, the per-LF guard
  call in `scroll_full_screen_into_history` read −9%.

Alternatives considered:

- **Keep the occupancy-clipped row moves**: simplest invariants (no second mapping layer, no materialization contract)
  but structurally short of parity, as above.
- **A per-region `row_lookup` permutation** (the split design's general form): subsumes the band but adds the per-read
  indirection load the unified ring avoids (measured −1.1% on ascii; avoiding it is part of the ring's +8–11%). The band
  is the special case that costs a branch instead of a load: real workloads scroll one region at a time.
- **Rotating only at read time without materialization hooks** (lazy everywhere): every raw-ring-order consumer (`base`
  bumps, snapshots, the saved-screen compaction) would need band awareness threaded through; the hook points are few and
  cold, so eager materialization at those seams keeps the band invisible everywhere else.

Revisit if: a workload alternates scroll regions faster than it scrolls within them (materialize thrash, which would
show as region scrolls regressing toward the per-row-move cost), or a second simultaneously-hot region appears (the band
is deliberately singular).

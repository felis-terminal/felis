---
title: Scrollback
sidebar:
  order: 2
---

Scrollback is the off-screen history of the primary grid. It exists only on the daemon. The alternate screen has none.

## Structure

History and the live viewport share one flat cell ring, the **hot tier**; once a session's output stops, history older
than one screen moves to a **cold tier** (["Idle compaction"](#idle-compaction) below). `Grid`'s `cells` holds up to
`rows + cap` physical rows; a `base` offset marks the top of the viewport, and the rows just below it in ring order are
the retained history. The row that scrolls off the viewport top _is_ the youngest history row, in the same storage, so
eviction is a `base` bump, not a copy.

Logical viewport row `r` resolves to physical ring row `(base + r) mod phys_cap`; history row `-k` (1 = youngest) to
`(base - k) mod phys_cap`. History and viewport occupy one contiguous ring window `[base - history_len, base + rows)`,
so their combined length never exceeds `phys_cap`. A full-screen scroll off the top advances `base` and drops the
occupancy watermark of the row now recycled at the viewport bottom: no cells move, and once history is at `cap` that
watermark drop _is_ the eviction of the oldest row (the window has wrapped fully around). Why one ring rather than a
separate history store is recorded in ["Unified viewport-into-history ring"](#unified-viewport-into-history-ring) below.

Each row stays contiguous, so the print fast path pays only the ring resolution on the row base. A sub-region scroll
(DECSTBM, IL/DL), which cannot be a whole-window shift, rotates a _scroll band_ over the region instead of moving rows
(["Interior scroll band"](#interior-scroll-band) below). One eviction copy remains: a scroll region anchored at the top
but short of the bottom pushes to scrollback yet cannot be a pure base bump, because the rows below it must stay, so
that cold path shifts them back down after the push. Because history and the live screen are one surface, the scrollback
/ live seam that search, reflow, the pipe sources, and the viewport composition read is a plain window read, not a join
of two stores.

The physical buffer grows lazily: it starts at the live `rows` and doubles toward `rows + cap` only as history fills,
re-laying the ring out canonically on each growth (amortized O(1) per push). This is a requirement, not an optimization.
Initializing all `rows + cap` rows up front writes every one of them, so an idle session would be resident at the full
cap: at 16 bytes per `Cell` and a 137-column grid, ~21 MiB against the ~75 KiB a bare 137×35 viewport actually needs.

The _capacity_ is nonetheless reserved up front, and only the length grows. Reserved address space and resident rows are
not the same cost: the cell buffer is an anonymous mapping of its own, whose pages stay unfaulted until written, so the
full 21 MiB is address space while residency tracks the rows history has actually reached, and the ring never has to
move. Growing by allocating the next size up and copying into it (the rejected alternative) leaves the allocator holding
every intermediate buffer: after a 200 k-line flood about half the daemon's footprint is freed ring buffers it has not
returned to the OS. Keeping the ring on `malloc` and returning those pages afterwards (`malloc_zone_pressure_relief` and
its Linux twin) is rejected: it undoes, per platform, an allocation the ring never needed to make, and under glibc a
freed ring-sized block raises the mmap threshold so that later rings are never returned at all. With the capacity
reserved, a growth is an in-place rotation rather than a copy.

Readers go through a borrowing scrollback view, which clips each history row at its occupancy watermark
([grid-and-cells.md "Occupancy watermark"](grid-and-cells.md#occupancy-watermark)): a history row is a former viewport
row left in place, so its tail past the watermark holds undefined bytes from the zero-copy recycle. Reflow re-pads
history rows to full width locally so a bare-LF column carry still reconstructs. On the alternate screen `cap = 0` and
the live ring is a bare viewport; a scrollback read routes into the saved primary buffer, whose snapshot keeps its own
ring geometry and the `rows` and `cols` it is relative to, since a resize under the alt screen moves the live grid past
them (D4 below).

## Idle compaction

A full ring is resident at full width, while rows average a fraction of it: at 54×192, shell output occupies 33 of 192
columns on average, so most of a full ring is blank tails, and the daemon keeps that for every session it holds. Once a
session's output stops, the daemon moves its history older than one screen out of the ring into the cold tier, which
keeps each row's occupied cells only, packed in FIFO order with the row's wrap bit, and hands the ring pages those rows
vacated back to the OS.

Readers cannot tell the tiers apart. The scrollback view lends a cold row as the same occupancy-clipped `&[Cell]` a ring
row yields, and the scroll path evicts the oldest cold row before any ring row, in constant time. History is never
written once it leaves the viewport; a relayout (resize, `CSI 3 J`) rebuilds the ring densely through the view, so the
tiers never need reconciling. A walk over the physical ring visits live rows and the cold tier only, because touching a
vacated page faults it back in.

The trigger is time, never content: a run starts once a quiet interval passes with no output applied. Two cheaper
signals cannot prove a quiet interval: a sink chunk's length says nothing about what follows, because the PTY reader
batches reads, and the task's dirty flag folds many chunks into one wake. A run takes slices back to back, releasing the
session lock between them; a slice that finds new output ends the run, and the next quiet interval resumes where it
stopped. Waiting a quiet interval per slice instead is rejected: a full history would take dozens of intervals, so a
session whose output comes in bursts would rarely finish. A slice moves a fixed budget of cells, so one lock hold stays
bounded at any grid width.

Vacated pages are discarded, never decommitted. A discard leaves the pages mapped and readable as their old bytes or as
zeros, and the all-zero bit pattern is `Cell::default()`, so a refused discard changes nothing. A decommit and recommit
pair is rejected: a recommit can fail under commit pressure and leave live rows on inaccessible pages.

Four alternatives are measured on scroll floods and rejected:

- Copying each row into a variable-length store as it leaves the viewport costs 8 to 15% more instructions, past the 2%
  gate; it is the eager copy that the ["Unified viewport-into-history ring"](#unified-viewport-into-history-ring)
  removes. Batching the copy 128 rows at a time costs more, because the source rows have gone cold by then.
- Rings bucketed by width class pay the same copy plus about 25% rounding waste.
- Ghostty's pages with an 8-byte cell and idle-time LZ4 would replace the grid wholesale; the policy, compacting cold
  history while idle, is the part taken.
- Leaving row tails unwritten saves nothing, since a row is shorter than a 4 KiB page.

## Soft wrap

A logical line of shell output may span N rows because it wrapped at the column edge. Each row carries a
`soft_wrap_continued` bit: true when the row is an autowrap continuation of the row above. On the live grid the bit is
indexed by _physical_ row, a parallel ring to `cells`, so a scroll that only bumps `base` carries every row's bit with
its cells for free; a row that scrolls into history keeps the same physical slot and so keeps its bit with no move at
all.

The bit records how content **arrived**: it is set when a deferred or wide-glyph autowrap completes, cleared when the
row is wholesale blanked, and deliberately untouched by partial overwrites; it is not a re-derivation of what the row
currently holds. Four consumers read it: search stitches logical lines back together (below), `?45` reverse-wraparound
retraces wrapped lines, triple-click logical-line selection on the client, and reflow stitches each soft-wrapped line
before re-wrapping it at a new width ("Reflow on resize" below). The bit rides the `RowDelta` row codec (a header flag
bit, normatively [row-codec.md](../../reference/row-codec.md)) on every row-carrying path, and the shadow screen mirrors
it on apply.

Stitching itself is one rule in one function (`felis_grid::logical_line_spans`): it joins rows tied by the bit into the
logical line the program printed, with no newline across the wrap edge, across the scrollback / live seam. Every
text-reconstructing consumer routes through it (search, text-mode capture, the `pipe` encoders, and client-side
selection), so a wrapped URL or path reconstructs identically everywhere.

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

The `visible` source resumes by row position instead, as capture's row stream does: the growth rule above holds only for
history that grows at its newest end, and a program repaints live rows at will.

**Limitations (intentional):**

- **The default walk leaves out the alternate screen**: alt content (vim, less) is a transient surface the program
  repaints at will; its rows are no continuation of scrollback and its `soft_wrap` bits describe the alt screen, not the
  saved primary. The scrollback half still matches while a program holds the alt screen. The `visible` source is a
  separate walk over the live rows alone: it reads the alt screen because it stitches nothing into history, and its rows
  stitch among themselves by bits that do describe them.
- **Trailing-blank trim** on the logical line's tail, so a needle ending in spaces does not match the invisible blank
  cells after the text. Leading and interior blanks are preserved; rows that wrap into the next drop only their trailing
  `Empty` cells (the unfillable slot a wide-glyph wrap leaves). This is an exact rule, not a heuristic, since printed
  spaces are cells of their own and survive.

## Capture

Capture is the **pull** half of the region surface: it reads a region to the caller's stdout, the headless complement of
the keymap `pipe` action's **push** to an in-window sink ([input.md](../input.md) "Why pipe, not a copy mode"). The
verb, its flags, and its output framing are in [cli.md](../../reference/cli.md); the wire conversation is in
[the IPC wire reference](../../reference/ipc.md#region-kind--6). Four decisions shape it.

**It is daemon-side, mirroring search.** The data is daemon-side and search already proves the shape: attach, send one
request, read a framed reply stream, detach. The daemon encodes each row to text itself, so the CLI reconstructs no grid
and carries no cell codec. Like search the reply is a cancelable stream, and the row window is pushed into each source's
iterator rather than applied to a materialized region, so a capture of a huge scrollback pays for the rows it ships.

**Its sources are the pipe action's daemon-side sources, token for token.** One wire enum serves both, so a user who
learns which region a chord sends learns which region the CLI reads. The two readers differ only in reply shape: `pipe`
asks for one stitched blob, which is what a pager or an argv sink consumes, while capture asks for the same region row
by row, because a structural consumer wants each row's index and soft-wrap bit and a stitched blob flattens both away.
Why the richer source lands on capture rather than a separate `sessions pipe` verb is recorded in
[control-surfaces.md](../architecture/control-surfaces.md) "`pipe` is keymap-only, and stays a separate verb from
`capture`".

**Row indices live in the region's own coordinate space.** Scrollback rows are negative (REQ-608), live rows keep their
grid coordinate, and a mark range counts from zero at its first row, because a mark range is anchored to a command
rather than to the grid and a grid-absolute index there would name a row the range does not have.

**A caller never has to choose between color and structure.** Asking for both runs both encoders and each row carries
its plain and its SGR form side by side, because raw escapes would corrupt the structural field and dropping one form
would make the picker that lists rows structurally and previews them in color read the session twice.

Tailing a source to its last N rows is a **filter, not a renumbering**: surviving rows keep the indices the untrimmed
capture would show, since an index that changed meaning under a flag would make the stable-machine-output guarantee a
lie. The cut happens daemon-side for every source, because the point of a tail is to not ship rows the client would
drop. A logical line the cut bisects prints only its surviving rows, which is honest for a tail view.

**Rejected: a viewport-walking client** that scrolls the viewport upward in grid-height steps and stitches each
recomposed window. The viewport path is built for an interactive GUI under pull pacing, with row diffs withheld while
the view is scrolled back, so a headless client would have to fake the pull cadence and de-overlap window seams. A
single daemon walk is simpler and round-trip-free.

## Piping to an external command

Scrollback is the primary source for the **pipe action**: a binding ships the retained scrollback plus the live screen
to a pager, the escape hatch kitty (`show_scrollback`, <https://sw.kovidgoyal.net/kitty/overview/>) and foot (the
`pipe-*` family, <https://man.archlinux.org/man/foot.ini.5.en>) ship for heavy scrollback work. The action surface, the
temp-file carrier, the transient session a `Command` target runs in, and the environment that transient carries are in
[input.md](../input.md) "Action mapping"; the bindings are in [keybindings.md](../../reference/keybindings.md). What the
buffer contributes is the region and its encoding.

### Encoding: plain by default, `ansi` for color

The `ansi` flag on the request picks the per-row encoder: plain text by default, or SGR sequences on each attribute
change with a trailing `SGR 0` per line, so color survives into `less -R` / `bat`.

Plain is the default because the encoding is a property of the **consumer**, and most pipe consumers re-parse the text
as _data_: a hint picker (`thumbs`, `urlscan`), an editor, a text tool; embedded SGR escapes corrupt their parse. Only a
consumer that renders the text _back to a screen_ wants color: a color pager or `fzf --ansi`. So a pager binding sets
`ansi = true`, while any binding that omits it defaults plain: the fail-safe direction, since plain into a color pager
merely loses color, whereas SGR into a data tool produces garbage. `felis sessions capture --ansi` defaults off for the
same reason. The live risk is drift: if the attribute set grows an SGR dimension the encoder does not emit, pager output
diverges from what the window renders. That is why the encoder is shared rather than copied: the pipe action,
`capture --ansi`, and the host-terminal clients all draw on `felis-grid::ansi`
([architecture/overview.md](../architecture/overview.md) "Shared wire knowledge across the satellite clients"), so one
addition serves every consumer.

### Logical lines: soft-wrap stitching

Both encoders join rows with `\n` **except across a soft-wrap edge**, via the shared stitch ("Soft wrap" above), so a
logical line the program wrapped at the screen edge (a URL, a path) reconstructs whole. Without this a downstream hint
picker sees a wrapped match split in half and cannot act on it.

One residual limit, in the `ansi = true` case only: each row closes with `SGR 0`, so a stitched logical line carries a
reset mid-line. Harmless for a color pager. A picker that strips SGR before matching (kitty's hints kitten does) rejoins
the chars; one that does not (`thumbs`, `urlscan`) may still split a _colored_ wrapped match. The default-plain case is
clean for every picker.

### Selection source: client-side serialization

Four of the six `pipe` sources (`scrollback`, `visible`, `command_output`, `last_command`) are daemon state, serialized
daemon-side by the encoders above. The fifth, `osc_52`, is a program's payload, not cells, and stays with the window for
the reason [input.md](../input.md) "`pipe` ships a region to an external sink" gives. The sixth, `selection`, is **not**
daemon state: the grid the daemon owns carries no selection; the selection is a client-local gesture over presented
pixels (principle 3). So the **client** serializes it through the shared stitch ("Soft wrap" above) and feeds those
bytes straight to the sink, with no daemon round trip. Asking the daemon to track a per-client selection instead would
put presentation state on the wrong side of principle 3, and each attached client has its own selection anyway.

### A region too large to carry

A 10 000-row scrollback of wide, heavily colored cells (REQ-605, REQ-605a) with `ansi = true` serializes past the
framing backstop, and a body that big would be refused by the daemon's own writer, costing the requesting window its
connection in the middle of a chord (REQ-105). So the reply carries a ceiling of its own below the backstop
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
reachable without a human holding a clipboard that large.

The OSC 133-keyed pipe sources (`command_output` = last `C→D` range, `last_command` = last `B→D` range) read across the
scrollback / screen boundary, so a command whose output has scrolled into history is still recoverable through the
prompt-mark coordinates below.

## Prompt marks (OSC 133)

Each `OSC 133` boundary records a mark (line, kind, exit code) on the grid. The line is an **absolute** coordinate: the
cumulative count of rows ever pushed into scrollback, which never decreases, plus the cursor row at record time. A
mark's line therefore stays stable across later scrolls and resizes, and resolves at read time to a live row, a retained
scrollback row, or evicted (unrecoverable).

The mark list is **front-pruned**: on each scrollback eviction and on ED 3, the leading marks whose line has fallen
below the oldest retained row are drained and counted in `prompt_marks_pruned`, so the list stays bounded by the number
of _retained_ marks, not by session lifetime.

Front-draining breaks the naive "mark index = position in the vector" assumption, so the daemon's streaming cursor
counts in a **never-pruned ordinal**: mark `i` in the live vector carries absolute ordinal `prompt_marks_pruned + i`. A
per-connection cursor stored as that ordinal survives a prune untouched, and the daemon subtracts `prompt_marks_pruned`
to index the retained slice. A mark pruned before a subscriber ever streamed it is skipped, never re-sent as a different
mark.

### Prompt-jump navigation

A previous/next prompt jump resolves to a scrollback viewport entirely on the daemon. The matched prompt-start mark is
top-aligned in the composed view, clamped at the history ends with **no wrap**. Jumps are disabled on the alternate
screen and skip evicted marks.

### Browsing while output arrives

A browsing view stays on the lines it shows while the program keeps printing: the offset grows by the rows pushed into
scrollback, as kitty grows `scrolled_by` and alacritty its display offset, and reaching the bottom joins the live view
again. A resize, `CSI 3 J` or a screen switch also returns the view to live: each stops a push count naming the same
lines, and the anchor's generation records it even when one burst erases history and refills it before the next compose,
where a clamp alone would land on unrelated rows. The anchoring lives in felis-grid so an embedder holding its own
offset calls the same function. The client's next step must start from the grown offset, and with pull pacing a frame
carrying it is usually in flight while the user scrolls, so a step sent then would name an offset the daemon has since
grown. Every browsing `ViewportState` therefore carries the `ViewportAnchor` it is expressed against and a request
echoes it back (wire in [ipc.md](../../reference/ipc.md)). **Rejected: daemon-side anchoring alone**, reading each
request against the current bottom: correct between frames, but a scroll during streaming output lands one frame's
output away, and on a remote attachment, where the round trip is longer than a frame, that becomes the common case.

A relative scroll (a line, a half page, a wheel tick, Home) is not an offset at all: the window sends
`InputMsg::ViewportStep` and the daemon applies it to the view it holds for that subscriber, as `JumpPrompt` already
resolves a prompt jump, so a step sent before the previous step's reply arrives still counts from it. An offset the
window computed from its mirror would name the mirror's offset plus one, losing the step in flight or undoing a return
to live still on its way. **Rejected: a request sequence echoed in `ViewportState`**, with the window stepping from its
latest unacknowledged request: every request would have to earn a reply even when the view does not move, and after a
prompt jump, whose target only the daemon knows, the window would still have nothing to step from until the reply.
Absolute requests (a scrollbar position, a search hit, a returning visit) stay offsets with an anchor, since what they
name is a place, not a distance.

## Selection vs scrollback

The selection model is client-side pure data: an anchor/extent pair in 0-based shadow-grid coordinates, owned by the
App. The daemon's grid and scrollback carry no selection state and nothing crosses the wire ([input.md](../input.md));
when a keybinding pipes the selection, the client serializes it itself (see "Selection source: client-side
serialization" above).

## Capacity and eviction

- Capacity is in lines. A session takes `DEFAULT_SCROLLBACK_ROWS = 10_000`; per-session sizing is not user-configurable.
  Capacity is a construction parameter rather than a constant because a screen that is a mirror retains nothing: the
  client's shadow builds at capacity 0, since the daemon composes the browse viewport and ships it as ordinary rows. At
  capacity 0 a row leaving the viewport top is dropped instead of evicted into history, and the ring reserves only the
  live viewport. The alternate screen runs at capacity 0 for the same reason: it has no history of its own.
- Memory is the dense cell array: a row is a `[Cell]` at 16 bytes per cell, with the pen interned to a four-byte handle
  rather than carried inline ([grid-and-cells.md](grid-and-cells.md#style-interning)), so the default 10k rows at 96
  columns costs ≈ 15 MiB per session before [idle compaction](#idle-compaction), the figure the 100 000-row rejection
  below scales by ten.
- Eviction is FIFO across both tiers. There is no "important lines" policy; users wanting permanence should pipe to a
  file.

## Prior art and alternatives considered

felis's storage choices are the mainstream terminal consensus; the few places it diverges are called out. This records
why each was chosen over the alternatives, so the rationale lives with the design rather than in a separate survey.

**Storage: row ring, with a slab only for quiet history.** A fixed-size ring of dense rows is what Kitty, Alacritty,
WezTerm, foot, and Ghostty all use: O(1) append and random access, contiguous and cache-friendly, predictable eviction.
felis keeps that ring, plus the cold tier of ["Idle compaction"](#idle-compaction) for quiet history. A
pointer-ring-plus-slab for all history (tmux) compacts short lines too, but pays a copy and an indirection on every
scroll; a piece table gives up the O(1) cell lookup the renderer depends on (no major terminal uses one); a paged or
disk-backed log (iTerm2 optional, tmux logging) bounds memory regardless of capacity but costs a syscall per read and a
versioned on-disk format. At felis's scale none of those pays for itself.

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
broken, so felis re-wraps: it walks scrollback then the live rows as one surface, stitches each soft-wrapped logical
line back to its pre-wrap length, and lays it out again at the new column count; a hard-wrapped line (one that ended in
an explicit LF) stays a separate logical line. This is `O(rows × cols)` at resize, an acceptable cost. Reflow is the
primary screen's resize; the shadow and the alternate screen trim and pad.

Five decisions fix the reflow's edges:

- **Cursor and marks ride their logical line (D1).** Reflow preserves the cursor's _logical_ position: column K of
  logical line N lands wherever that column falls at the new width, not at a fixed physical row. Each prompt mark is
  remapped onto the new first physical row of the logical line it sat on, and a mark whose line evicts is pruned.
  Anchoring to the physical row instead would scatter the cursor and every mark on any width change. A prompt blanked
  for the shell to repaint (D5) is the exception: blanked whole, it leaves the cursor at its column and its row offset
  from the prompt's first row, which is where the shell's repaint starts from; under `redraw=last` only the cursor's row
  is blanked, and the cursor keeps its column on it. Kitty direct-placement anchors ride the same mapping (REQ-604; see
  [grid-and-cells.md](grid-and-cells.md) "Image references"); a resize deferred under the alt screen (D4) delivers the
  remap at stream position, after the screen switch restores the saved primary placements.
- **The round-trip is exact only without eviction (D2).** `W → W' → W` restores the original layout only while no
  scrollback eviction occurs. Narrowing produces more physical rows, and once the ring is at `cap` the oldest rows evict
  first (as in Kitty), so a narrow-then-widen that overflowed the ring cannot resurrect the evicted head. This is the
  scoped limit of the round-trip guarantee (REQ-604, REQ-407).
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
- **The prompt the shell repaints is blanked, not re-wrapped (D5).** On SIGWINCH zsh climbs back the rows it drew its
  prompt on and repaints from there. A re-wrap changes that row count, so a prompt line that filled the drawn width (the
  p10k and starship separator style) survives as a stale copy on every resize. Kitty
  (`prevent_current_prompt_from_rewrapping` in `kitty/screen.c`) and Ghostty (`clearPromptForRedraw` in
  `src/terminal/Screen.zig`) blank the prompt instead, and felis follows them, with the same `redraw` opt-out. Only an
  `OSC 133` prompt qualifies: finding an unmarked prompt means guessing at shell content
  ([non-goals.md](../non-goals.md) "Heuristic prompt or command detection"). A shell that marks prompts but never sends
  `C` does not qualify either: its running command's output still sits under the youngest `A`, and the blank would erase
  output nothing repaints. For the same reason a prompt that appears while a marked command runs (a nested shell, or one
  the marking shell `exec`ed) is left alone until that shell ends a marked command itself. What stays out of reach is
  output a background job writes while the shell waits at its prompt: no mark separates it from the echo of the line
  being typed, so the blank takes it too, as it does in kitty and Ghostty.

A multi-cell sized run (OSC 66) that the re-wrap cannot fit is discarded, and the producer re-emits it on the new
geometry: the same keep-or-discard rule `resize` already applies (REQ-406/REQ-407), not a re-fit in place. The rewrap
lays cells out one at a time and does not retain the run's source text, so a run split across a new wrap boundary cannot
be re-fitted from the grid alone.

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

Under the split, plaintext scroll on the primary screen trails ghostty about 1.6× (vtebench `scrolling`), and the whole
primary-vs-alt gap is that one eager copy, paid per line even when the row holds a single cell. ghostty and alacritty
pay nothing here because their viewport is a window into the history ring: the row that leaves the viewport top is
already the newest history row, so eviction is pointer motion. On the alt screen, the same rotate with no push already
beats ghostty's end-to-end throughput, so what the split lacks is only the property that the evicted row is never
copied, and only the unified ring supplies it.

Two cheaper fixes are measured and rejected:

- Shrinking `push_prefix`'s per-call cost (replacing its modulo with a compare-and-reset branch) regresses the primary
  floor 16%; the path is as inlining-sensitive as the print fast path, and the division is not the cost.
- A base-offset ring for the live window alone (an O(1) full-screen scroll that leaves history untouched) costs 1.1% on
  the ascii print fast path and buys only the ~3.5% rotate memmove, on the alt path that already wins. The ring index is
  worth paying for only if it also eliminates the copy, so the offset ring is not a standalone stage: it is one facet of
  the unified ring.

The unification is invisible on the wire: the grid's storage is private and the wire ships logical rows and scrolls, so
the shadow screen, the renderer, and every out-of-repo consumer see the same row stream either way. The cold tier keeps
the property, since rows never reach it on the scroll path.

## Interior scroll band

A DECSTBM / IL / DL scroll cannot be a `base` bump (it shifts only a sub-range of the viewport), so the ring resolves
those through a second rotation layered on the same row resolution: a band (top, length, rotation) applied to rows
within the scrolled region before the `base` offset. A region scroll arms the band and bumps its rotation instead of
moving rows; only the recycled rows are blanked. The band also serves the rare full-screen rotate that cannot bump
`base` (scroll-down / RI with retained history below the viewport, where the bump would resurrect history rows).

The invariant a contributor must hold: the band is _materialized_ (physically reordered back to identity, then
deactivated) wherever raw ring order is assumed: a `base` shift, the alt-screen snapshot / restore pair, a ring growth,
and a _change_ of scrolled region. Relayouts that copy the viewport out through the band-aware row resolution
materialize as a side effect. A sustained same-region flood never materializes; alternating regions (IL at a moving
cursor) re-materialize per switch, which costs one per-row region move, so the worst case matches what a move-per-line
scheme pays on every scrolled line.

The rejected alternative moves the region's rows per scrolled line, occupancy-clipped. Clipping alone leaves the
interior scroll ~8× behind ghostty on vtebench `scrolling_top_region`, because the residual is per-row move bookkeeping
that no clipping removes: parity needs the scroll to stop touching rows at all. The band is about 12× faster than the
per-line move on the parse-into-grid floor, bringing the interior region to parity with the full-screen scroll, and it
leaves the ascii print and full-screen scroll floors unregressed.

Alternatives considered:

- **Keep the occupancy-clipped row moves**: simplest invariants (no second mapping layer, no materialization contract)
  but structurally short of parity, as above.
- **A per-region `row_lookup` permutation** (the split design's general form): subsumes the band but adds the per-read
  indirection load the unified ring avoids (measured −1.1% on ascii; avoiding it is part of the ring's +8–11%). The band
  is the special case that costs a branch instead of a load: real workloads scroll one region at a time.
- **Rotating only at read time without materialization hooks** (lazy everywhere): every raw-ring-order consumer (`base`
  bumps, snapshots, the saved-screen compaction) would need band awareness threaded through; the hook points are few and
  cold, so eager materialization at those seams keeps the band invisible everywhere else.

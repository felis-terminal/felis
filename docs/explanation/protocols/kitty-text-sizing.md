---
title: Kitty text sizing design
sidebar:
  order: 4
---

This page records how felis divides the Kitty text sizing protocol between daemon and client, how sized runs are stored,
and how sizing interacts with the rest of the terminal model. The wire format, keys, and limits live in the
[reference twin](../../reference/protocols/kitty-text-sizing.md).

The text sizing protocol unlocks readable headings, large status displays, and per-cell fractional sizing without
breaking grid layout. felis implements the full protocol; the canonical reference is Kitty's own documentation at
<https://sw.kovidgoyal.net/kitty/text-sizing-protocol/>, and where this document and the upstream spec diverge, the
upstream spec wins.

## Why fractional scale only shrinks

The parser-enforced `d > n` rule keeps the fractional factor strictly below 1 because `n`/`d` exist to express
superscript-style sub-cell shrink (`n=1:d=2` → 0.5×); growth is the integer scale `s`'s job. Upstream frames it the same
way: "the fractional scale just adjusts the rendered font size within those cells [not the cell count]"
(<https://sw.kovidgoyal.net/kitty/text-sizing-protocol/>).

## Daemon vs client responsibilities

- **Daemon** records, per primary cell, the active sizing parameters for the run. Sizing is a cell attribute, like SGR.
- **Daemon** does not measure or render text: it has no font.
- **Client** receives the sizing parameters alongside the cell's content and applies them during shaping and atlas
  lookup.

A client switching fonts mid-attach does not require the daemon to do anything; the client re-shapes the affected runs
and rebuilds the atlas. (See [`text-shaping.md`](../rendering/text-shaping.md) for the per-(font, sizing handle, run)
cache.)

## Interaction with grid coordinates

A sized run occupies an N×M block of cells. Every cell of the run, the _primary_ (top-left) and every covered cell
alike, carries the same sizing handle; there is no separate "spanned-by" back-reference (REQ-603, REQ-408). The primary
is recovered by scanning: from any cell, walk up then left while the neighbor carries the identical handle, and the cell
where the walk stops is the primary. Cursor movement, erasure, and selection all operate on the primary cell: moving the
cursor "into" a spanned region jumps to the primary.

This mirrors how wide East Asian glyphs are handled in conventional terminals (one primary cell, one continuation cell).
Sized text generalizes that idea to arbitrary M×N spans.

A sized character taller than one row is only meaningful whole: its primary draws over the full footprint. A cell move
that carries some of its rows and not others would leave the primary drawing over cells that now hold other text, or
leave rows with no primary at all. felis therefore erases such a character before the move, as kitty does
([`screen.c`](https://github.com/kovidgoyal/kitty/blob/master/kitty/screen.c) `nuke_multiline_char_intersecting_with`,
called from `screen_insert_characters`, `screen_delete_characters`, `screen_insert_lines` and
`screen_delete_lines_impl`). Re-fitting the character elsewhere was rejected for the reason reflow rejects it (see
below): there is no position the application chose. The one exception is a scroll into scrollback: the rows that leave
keep their part of the character, so its text survives in history, and only the rows left on screen are cleared.

## How felis stores it

Per-cell sizing rides _inline_ in the `Cell`, not in a side-table. Each cell holds `sizing: Option<SizingHandle>`, a
`NonZeroU16` index (1-based; absence = default sizing) into the grid's `sizing_table` registry. Installing a non-default
OSC 66 run appends one `Sizing` entry to that registry and returns its handle; the whole run (primary and continuation
cells alike) is stamped with that one handle, so a long run costs one registry entry plus one machine word per cell.

Because the handle lives in the `Cell`, sizing is saved, restored, reflowed, and carried across an alternate-screen
switch with the cell buffer for free; no parallel structure has to be kept in sync. Distinguishing primary from
continuation cells is therefore not stored either: it is derived by the equal-handle neighbor scan described above.

## Reclaiming registry entries

An entry is minted per _run_, not per distinct sizing, so the registry is bounded by how much sized text a session ever
drew rather than by how many sizings it used. A producer that repaints (a slide deck, a TUI redrawing a sized header)
mints a fresh entry every repaint while the entries behind the overwritten cells go unreferenced, and the handle space
is 16-bit: past it `install_sizing` returns nothing and the run draws unsized. Nothing about that is visible to the
producer, which is what makes it worth reclaiming rather than reporting.

So the grid sweeps, exactly as it does for interned pens: `gc_sizings` marks the handles live and saved cells still
carry, compacts the registry, and remaps every cell in place. The daemon triggers it after a parse chunk, on a threshold
that doubles when a sweep cannot halve the table, but only up to a ceiling, because unlike a pen a handle that cannot be
minted has no fallback to degrade to, so a session must not be able to raise its own trigger point beyond the sweep's
reach.

Deduping equal sizings (the obvious alternative, and what the pen, cluster and link registries all do) is rejected here,
because this registry's handle carries a second meaning the others' do not: it is what tells one run from the next. Two
adjacent runs of equal sizing sharing an entry would merge into one block under the equal-handle scan above, and a write
into either would erase both. Compaction keeps handles distinct (the remap is a bijection), so it reclaims without
touching what the handle means.

## Interaction with reflow

When a resize changes the column count, a sized run either survives in place or is discarded whole:

1. If the run's cell block still fits at the new width, its cells ride the resize unchanged and it re-lays in place.
2. Otherwise the run is discarded, and the producer re-emits it on the new geometry, consistent with REQ-406's
   discard-rather-than-partial rule.

There is no partial re-fit: a run is never broken at a boundary with the trailing portion re-wrapped, because neither
`Grid::resize` nor `Grid::reflow` retains the run's source text on the grid, and re-fitting a sub-run would need it.
This is why the reflow path ([scrollback.md](../data-model/scrollback.md) "Prior art and alternatives considered")
discards a multi-cell run that a re-wrap splits across a new wrap boundary rather than laying its tail out one cell at a
time. The rules are deterministic so that resize-then-restore returns to the original layout while every run still fits
at the intermediate width (REQ-407); a run discarded along the way stays gone until the producer re-emits it.

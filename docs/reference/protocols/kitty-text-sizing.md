---
title: Kitty text sizing protocol
sidebar:
  order: 4
---

felis's implementation of the Kitty text sizing protocol (OSC 66): wire syntax, metadata keys, scaling parameters, and
limits.

Architectural design rationale (daemon vs client partitioning, cell storage, registry reclamation, and reflow) is
documented in the companion explanation document
[kitty-text-sizing.md](../../explanation/protocols/kitty-text-sizing.md). The upstream Kitty specification is published
at <https://sw.kovidgoyal.net/kitty/text-sizing-protocol/>.

## Wire format

```
OSC 66 ; metadata ; text ST
```

`metadata` is a colon-separated list of `key=value` pairs. All keys are optional; an empty metadata field
(`OSC 66 ; ; text ST`) renders `text` at default size with auto-derived width. The recognized keys are pinned in
[spec.md](../spec.md) (REQ-400 through REQ-408):

| Key | Range | Default | Purpose                                           |
| --- | ----- | ------- | ------------------------------------------------- |
| `s` | 1–7   | 1       | integer scale                                     |
| `w` | 0–7   | 0       | cell-width override (0 = auto)                    |
| `n` | 0–15  | 0       | fractional-scale numerator                        |
| `d` | 0–15  | 0       | fractional-scale denominator (`d > n`)            |
| `v` | 0–2   | 0       | vertical alignment: 0 top, 1 bottom, 2 centered   |
| `h` | 0–2   | 0       | horizontal alignment: 0 left, 1 right, 2 centered |

Each character of a run occupies `(w · s) × s` cells (`w = 0` derives from its grapheme width). Rendered glyph size is
`s × n/d` of base font size when fractional scaling is active (`d > 0`), otherwise `s`. Because the parser enforces
`d > n`, fractional factors are strictly less than 1 and only scale downward (e.g. `n=1:d=2` yields 0.5×; see
[kitty-text-sizing.md](../../explanation/protocols/kitty-text-sizing.md)). `text` is the run to render.

Each `OSC 66` defines a self-contained run; no persistent sizing mode is maintained for subsequent writes. The legacy
`CSI Pn:...:Pn t` sequence is not supported ([support-matrix.md](support-matrix.md#kitty-text-sizing-osc-66)). Size
transitions are instantaneous: a run is drawn at its size on the next frame, and felis interpolates nothing from the
size the cells carried before.

## Placing a sized character

Each grapheme cluster of `text` is one sized character with its own block: `w · s` columns wide (`w = 0` takes the
cluster's own width) and `s` rows tall. An explicit `w` sizes each cluster, not the run as a whole.

- A character wider than the screen or taller than the scroll region is discarded; the rest of the run is still placed
  (REQ-406).
- The right edge is the one a printed character uses: the right margin when left/right margins are set and the cursor is
  not past it, otherwise the last column. With autowrap on, a character that does not fit before the right edge wraps to
  the next line, as printed text does, and so does one that would land on the lower rows of a taller character. With
  autowrap off, or when the character is wider than the margins, it is placed against the right edge, and erases what it
  lands on.
- A character taller than the rows left in the scroll region scrolls the region up by the difference (into scrollback
  when the region starts at the top of the primary screen). Below the scroll region the cursor moves up instead.
- In insert mode the block's width is inserted on every row the block covers.
- A `w` narrower than the glyph squeezes the glyph into the block.
- A combining mark at the start of `text` is dropped, never joined to a character written before the run. A bidi
  override joins the character before it, or the next one printed when there is none, as in printed text.
- A mark printed after the run joins its last character without widening its block; with `w = 0`, a mark that would
  change the character's width (VS16 after `❤`) is dropped instead.

## Editing over a sized character

Each character of a run owns its own block of cells; the rules below apply per character, so editing one character of
`OSC 66 ; s=2 ; AB ST` leaves the other intact.

- Writing into any cell of a character's block erases the whole block, including rows the write does not reach.
- A cell move that would split a block taller than one row between its rows erases the block first: inserting or
  deleting characters on one of its rows (ICH, DCH, insert mode), shifting columns in a scroll region whose top or
  bottom margin crosses it (DECIC, DECDC, SL, SR, DECBI, DECFI), and scrolling or inserting or deleting lines at a seam
  inside it (SU, SD, LF, RI, IL, DL, with or without left/right margins).
- A scroll that pushes a block's upper rows into scrollback keeps them there and clears the rows left on screen.
- A block that a move carries in full moves intact.

## Limits

- Maximum integer scale `s`: 7 (REQ-401).
- Maximum fractional factor: 14/15 (`d > n` caps `n/d` strictly below 1; REQ-403). The maximum effective glyph scale is
  `s` itself.
- Effective glyph scale range: `[0, 7]`. `n=0` with `d > 0` yields an exactly zero-size glyph, which the parser accepts.
- Maximum cell-width override `w`: 7 (REQ-402).
- Grid bounds: a character whose block is wider than the screen or taller than the scroll region is discarded (REQ-406);
  a block is never clipped at the screen edge.
- Resize: a character whose block does not fit the new size loses its sizing; its text stays at its natural width when
  that fits, and is removed when it does not (REQ-407).

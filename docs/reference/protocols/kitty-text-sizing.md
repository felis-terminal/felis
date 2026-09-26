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

The cell footprint occupied by a run is `(w · s) × s` cells (`w = 0` derives from grapheme widths). Rendered glyph size
is `s × n/d` of base font size when fractional scaling is active (`d > 0`), otherwise `s`. Because the parser enforces
`d > n`, fractional factors are strictly less than 1 and only scale downward (e.g. `n=1:d=2` yields 0.5×; see
[kitty-text-sizing.md](../../explanation/protocols/kitty-text-sizing.md)). `text` is the run to render.

Each `OSC 66` defines a self-contained run; no persistent sizing mode is maintained for subsequent writes. The legacy
`CSI Pn:...:Pn t` sequence is not supported ([support-matrix.md](support-matrix.md#kitty-text-sizing-osc-66)). Size
transitions are instantaneous: a run is drawn at its size on the next frame, and felis interpolates nothing from the
size the cells carried before.

## Limits

- Maximum integer scale `s`: 7 (REQ-401).
- Maximum fractional factor: 14/15 (`d > n` caps `n/d` strictly below 1; REQ-403). The maximum effective glyph scale is
  `s` itself.
- Effective glyph scale range: `[0, 7]`. `n=0` with `d > 0` yields an exactly zero-size glyph, which the parser accepts.
- Maximum cell-width override `w`: 7 (REQ-402).
- Grid bounds: A run whose cell footprint exceeds terminal dimensions is discarded outright (REQ-406); partial clamping
  is not performed.

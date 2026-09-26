---
title: Row codec
sidebar:
  order: 14
---

The protobuf schema (`crates/felis-protocol/proto/felis.proto`) governs wire message framing and declares `packed_cells`
as opaque `bytes`; this specification defines its internal binary representation (REQ-112). The reference implementation
is in `crates/felis-grid/src/wire.rs`. Architectural rationale for a custom per-row codec is documented in
[ipc.md](../explanation/architecture/ipc.md).

This page governs the packed bytes alone. A consumer that reads a row as JSON reads `felis-json` v1, whose own DTOs
spell out the decoded row and are versioned with the rest of that format ([ipc.md](ipc.md) "Structural session JSON");
nothing outside the socket carries these bytes.

## Conventions

- **Endianness.** Every multi-byte integer is little-endian. This matches the frame header and differs from the version
  preface, which is big-endian because it is a magic-prefixed network preface.
- **Widths are fixed** except where this page says varint. A count or index that describes a grid dimension is `u16`,
  because grid dimensions are `u16`, and the cap sits under the field width at the admitted geometry ("Limits" below).
  Fixed widths also let an encoder reserve a count and back-patch it after one pass over the columns, which is why the
  attribute-run count is not a varint.
- **Varints** are unsigned LEB128: seven payload bits per byte, little-endian groups, high bit set on every byte but the
  last. Only the cluster handle uses one, where the values are small in practice and a fixed `u32` would cost three
  wasted bytes on exactly the rows that carry many of them. A varint is in **shortest form**: a value has one spelling,
  and a decoder rejects any other.
- **Text.** The payload carries no length-prefixed string. Cluster text and hyperlink URIs live in their own registry
  messages, bounded by `OSC_BUFFER_LIMIT` there; the only text-domain value here is a single Unicode scalar, encoded as
  UTF-8 and validated as such.
- **Reserved bits and undefined tags are errors**, never ignored: a decoder that masked them off would round-trip to a
  different row than the sender encoded.

## Layout

A payload is exactly the following, in order, with nothing before or after it.

| Field            | Encoding                  | Notes                                                                                         |
| ---------------- | ------------------------- | --------------------------------------------------------------------------------------------- |
| `version`        | `u8`                      | `1`. See [Versioning](#versioning).                                                           |
| `flags`          | `u8`                      | Bit 0: the row is an autowrap continuation of the row above. Bits 1–7 reserved, must be zero. |
| `cols`           | `u16`                     | Columns in the row, and the number of grapheme records that follow.                           |
| grapheme records | `cols` × variable         | [Grapheme records](#grapheme-records), one per column, in column order.                       |
| `run_count`      | `u16`                     | Number of attribute runs that follow.                                                         |
| attribute runs   | `run_count` × 10–19 bytes | [Attribute runs](#attribute-runs), in column order.                                           |
| `sized_count`    | `u16`                     | Number of OSC 66 side-band entries that follow.                                               |
| sized cells      | `sized_count` × 8 bytes   | [Sized cells](#sized-cells).                                                                  |

The soft-wrap bit rides the row codec rather than the `RowDelta` message so that every row-carrying path (delta, batch,
rehydrate, composed viewport) ships it without the protobuf schema learning about it.

### Grapheme records

One record per column: a `u8` tag, then the payload the tag names.

| Tag | Meaning                                            | Payload                                                                                                   |
| --- | -------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `0` | Empty cell                                         | none                                                                                                      |
| `1` | Printable ASCII                                    | one byte, `0x20`–`0x7E`; anything else (a C0 control, `DEL`, or a byte with the high bit set) is an error |
| `2` | Unicode scalar                                     | the scalar's UTF-8 encoding, 1–4 bytes                                                                    |
| `3` | Cluster handle                                     | varint `u32`, non-zero (handle 0 is the reserved "no entry")                                              |
| `4` | Spacer: the right half of a double-width glyph     | none                                                                                                      |
| `5` | Sized spacer: a continuation cell of an OSC 66 run | none                                                                                                      |

Tags `6` and above are errors.

The UTF-8 payload's length comes from its lead byte, and the sequence is then validated as UTF-8: an overlong form, a
surrogate, or a value past `U+10FFFF` is an error. Without that rule a naive reassembler would accept two byte strings
for one cell.

The codec does not police which variant a producer chose for a given character; it validates each variant's payload. A
decoder therefore reproduces the sender's variant exactly, which is what makes the encoding round-trip byte-for-byte.

Tag `1` is the one payload narrowed below its byte range, to the printable subset. It is not a general byte channel but
the compact spelling of the character an ASCII column holds, and a decoder that accepted `ESC` there would produce a
cell whose text (through a selection copy or an ANSI re-encode) is a live control sequence in whatever consumes it. Tag
`2` carries no such restriction: a Unicode scalar is any scalar, control scalars included, because a cell can
legitimately hold one.

### Attribute runs

Cells carry their pen as a grid-local interned handle, which does not survive the hop, so each run carries the
**resolved** attribute values, and the receiver interns them into its own registry. Runs are in column order and cover
the columns consecutively.

| Field             | Encoding | Notes                                                                               |
| ----------------- | -------- | ----------------------------------------------------------------------------------- |
| `len`             | `u16`    | Columns this run covers. Zero is an error.                                          |
| `fg`              | color    | Foreground.                                                                         |
| `bg`              | color    | Background.                                                                         |
| `underline_color` | color    | `Default` means "follow the foreground".                                            |
| `flags`           | `u16`    | Attribute bits, below. Any bit outside the defined set is an error.                 |
| `underline_style` | `u8`     | `0` single, `1` double, `2` curly, `3` dotted, `4` dashed. Other values are errors. |
| `link`            | `u16`    | Hyperlink slot shared by every cell of the run; `0` means no link.                  |

A **color** is a `u8` tag plus its payload: `0` = theme default, no payload; `1` = 256-color palette index, one byte;
`2` = direct sRGB, three bytes (red, green, blue). Other tags are errors.

The **attribute flags** word carries, from bit 0: bold, faint, italic, underline, blink, reverse, conceal,
strikethrough, overline, DECSCA protected, ISO protected. Bits 11–15 are undefined and rejected.

A run is therefore 10 bytes when all three colors are `Default`, and 19 when all three are sRGB.

### Sized cells

The sparse OSC 66 side-band: entries for the columns whose text sizing is not the default. Sizing rides here rather than
the cell stream because it is rare and per-run rather than per-cell.

| Field        | Encoding | Notes                                                      |
| ------------ | -------- | ---------------------------------------------------------- |
| `col`        | `u16`    | Column the entry applies to.                               |
| `scale`      | `u8`     | `s`, 1–7.                                                  |
| `cell_width` | `u8`     | `w`, 0–7; `0` auto-derives from the text's grapheme width. |
| `frac_num`   | `u8`     | `n`, 0–15.                                                 |
| `frac_den`   | `u8`     | `d`, 0–15.                                                 |
| `valign`     | `u8`     | `0` top, `1` bottom, `2` center.                           |
| `halign`     | `u8`     | `0` left, `1` right, `2` center.                           |

The parameters pass the same range check as an [kitty-text-sizing.md](protocols/kitty-text-sizing.md) parse, including
the fractional rule `d == 0 || n < d`; a combination outside them is an error, so nothing downstream re-validates a
sizing that came off the wire.

## Limits

| Limit              | Value | Applies to    |
| ------------------ | ----- | ------------- |
| Cells per row      | 2048  | `cols`        |
| Attribute runs     | 2048  | `run_count`   |
| Sized-cell entries | 2048  | `sized_count` |

Each cap is `MAX_GRID_COLS` ([ipc.md](ipc.md) "Semantic limits"), the widest row the admitted geometry holds: a run
covers at least one column, so there can be no more runs than columns, and the same bound covers the side-band. A count
above it is an error before the decoder reserves for it, since no grid could have produced the row.

The cap alone does not bound memory (2048 is still a claim a three-byte payload can make), so the allocation bound is a
second, tighter rule, checked **before** any reservation the count would drive:

- before reading the graphemes, the payload must hold at least `cols` more bytes (a grapheme record is at least its tag
  byte);
- before reading the runs, at least `run_count × 10` more bytes;
- before reading the side-band, at least `sized_count × 8` more bytes.

A payload that fails one of these is truncated, and the decoder says so without having reserved anything. The effect is
that a decoder's working memory is bounded by the length of the bytes it was handed, never by a number inside them.

## Decoding rules

A decode either yields exactly one row or fails; there is no partial success and no skipping.

- **Exact consumption.** Bytes left over after the side-band are an error. Slack would mean the payload and the decoder
  disagree about the layout, which is precisely what a silent tail hides.
- **Side-band columns.** Every sized-cell entry names a column of the row it rides, so a `col` at or past `cols` is an
  error rather than an entry the writer ignores.
- **Coverage.** The run lengths must sum to exactly `cols`. A run that would reach past the last column fails at that
  run; runs that end short of it fail after the last one. Neither is clamped: a desync must not shift every later
  column.
- **Order of checks.** Version, then flags, then each field in layout order, with the limit check for a count
  immediately before the reads it governs.
- **Errors name the field.** Every rejection carries what was expected and what was found: the version, the field that
  ran out of bytes and by how much, the count and the cap it broke, the tag that named nothing. A corrupt row closes the
  connection, so the error is the only account of why.

## Canonical form

The encoder emits **maximal runs**: adjacent columns sharing a pen and a link coalesce into one run. Together with
shortest-form varints, no zero-length runs, and exact consumption, that makes the encoder's output the canonical
spelling of a row: one row, one byte string. The golden vectors below are in that form, and re-encoding a decoded row
reproduces them byte for byte.

Maximality is a rule on the producer, not an invariant the decoder enforces. A payload that splits a run into two
identical adjacent ones is valid and decodes to the same row; it is merely larger.

## Versioning

The leading `version` byte identifies the codec version. A version a decoder does not implement is an error, not
something to skip past.

The tag identifies; it never authorizes. A new codec version is an addition like any other and is gated on the
connection's effective minor ([ipc.md](ipc.md) § "Versioning"): a peer may only send a version the effective minor
defines, which `felis-protocol`'s `ROW_CODEC_SINCE` states version by version for the send gate to read. Between honest
peers an unknown version therefore cannot occur, which is why receiving one is treated as corruption rather than as a
newer peer to accommodate.

## Golden vectors

Every implementation must produce these bytes for these rows and accept these bytes as these rows. All three are in
canonical form.

### The empty row

Zero columns: the header and three zero counts.

```
01                    version = 1
00                    flags: no soft wrap
00 00                 cols = 0
00 00                 run_count = 0
00 00                 sized_count = 0
```

### An ASCII row

`hi` in the default pen, no link, no sizing, the shape a line of `cat` output is made of. There is a single run: the pen
is written once for the row, not once per column.

```
01                    version = 1
00                    flags
02 00                 cols = 2
01 68                 Ascii 'h'
01 69                 Ascii 'i'
01 00                 run_count = 1
02 00                   len = 2
00                      fg = Default
00                      bg = Default
00                      underline_color = Default
00 00                   flags = 0
00                      underline_style = single
00 00                   link = none
00 00                 sized_count = 0
```

### A row exercising every shape

A multi-byte scalar, a varint cluster handle, both non-default color forms, live attribute flags, a non-default
underline shape, a link, two runs, the side-band, and the soft-wrap bit.

```
01                    version = 1
01                    flags: soft-wrap continuation
03 00                 cols = 3
02 E3 81 82           Char U+3042 'あ' as UTF-8
03 AC 02              Cluster handle 300 (varint)
04                    Spacer
02 00                 run_count = 2
01 00                   run 0: len = 1
02 AB CD EF             fg = sRGB(0xAB, 0xCD, 0xEF)
01 04                   bg = palette index 4
00                      underline_color = Default
09 00                   flags = bold | underline
02                      underline_style = curly
00 00                   link = none
02 00                   run 1: len = 2
02 AB CD EF             fg = sRGB(0xAB, 0xCD, 0xEF)
01 04                   bg = palette index 4
00                      underline_color = Default
09 00                   flags = bold | underline
02                      underline_style = curly
07 00                   link = slot 7
01 00                 sized_count = 1
01 00                   col = 1
02 00 00 00 00 00       scale 2, width auto, no fraction, top-left
```

The two runs do not coalesce because their link slots differ.

## Rejected vectors

Every decoder must reject each of these. They are derived from the vectors above by the stated edit. Every row is pinned
by a unit test in `felis-grid`'s `wire` module, and seeds for the boundary classes ship in `fuzz/seeds/row_codec/` to
start the fuzzer near them.

| Edit                                                    | Rejected as                          |
| ------------------------------------------------------- | ------------------------------------ |
| `version` = 2                                           | unknown version                      |
| `flags` = 2 (a reserved bit)                            | undefined flag bit                   |
| any prefix of a valid vector                            | truncated                            |
| a valid vector plus one byte                            | trailing bytes                       |
| `cols` = `FFFF` on the empty row                        | over the 2048 cap, before allocating |
| `run_count` = `FFFF` on the empty row                   | over the 2048 cap, before allocating |
| `sized_count` = `FFFF` on the empty row                 | over the 2048 cap, before allocating |
| a sized cell naming column 2 of a two-column row        | side-band column past the row        |
| a run's `len` = 0                                       | zero-length run                      |
| the ASCII row's run `len` = 4                           | runs cover 4 of 2 columns            |
| the ASCII row's run `len` = 1                           | runs cover 1 of 2 columns            |
| a grapheme tag of 6                                     | unknown grapheme tag                 |
| an ASCII payload of `80`                                | not 7-bit                            |
| an ASCII payload of `1B` (or any C0 byte, or `7F`)      | not printable ASCII                  |
| a `Char` payload of `C0 80`                             | overlong UTF-8                       |
| a `Char` payload of `ED A0 80`                          | UTF-16 surrogate                     |
| a cluster handle of `80 00`                             | non-minimal varint                   |
| a cluster handle of `00`                                | handle 0 is reserved                 |
| a color tag of 3                                        | unknown color tag                    |
| an attribute-flags word with bit 15 set (bytes `00 80`) | undefined flag bit                   |
| an `underline_style` of 5                               | unknown underline style              |
| a sizing `scale` of 0                                   | outside the OSC 66 ranges            |
| a sizing `valign` of 3                                  | unknown alignment                    |

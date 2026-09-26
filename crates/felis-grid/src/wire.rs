//! Row codec for `GridMsg::RowDelta` `packed_cells` (`docs/reference/row-codec.md`).
//! Hand-written rather than derived with serde/postcard so byte layout remains
//! strictly spec-driven and independent of Rust type layouts.
//! Rows split into graphemes, RLE style spans, and OSC 66 sizing side-bands.

use std::num::{NonZeroU16, NonZeroU32};

use thiserror::Error;

use crate::{AttrFlags, Attributes, Cell, Color, Grapheme, StyleId, StyleTable, UnderlineStyle};
use felis_protocol::kitty_text_sizing::{HAlign, Sizing, VAlign};
use felis_protocol::messages::MAX_GRID_COLS;

/// The codec version this build encodes, and the only one it decodes.
pub const ROW_CODEC_VERSION: u8 = 1;

/// The admitted geometry bounds a row, not the `u16` field width: a
/// wider row is impossible under REQ-605a, and the count is checked
/// against this before the decoder reserves for it.
pub const MAX_CELLS_PER_ROW: usize = MAX_GRID_COLS as usize;

/// A run covers at least one column, so the row width.
pub const MAX_ATTR_RUNS: usize = MAX_CELLS_PER_ROW;

/// One per column at worst, so the row width.
pub const MAX_SIZED_CELLS: usize = MAX_CELLS_PER_ROW;

/// `flags` bit 0: the row is an autowrap continuation of the row above
/// (`docs/explanation/data-model/scrollback.md` "Soft wrap"). It rides
/// the row codec so every row-carrying path ships it without touching
/// `felis-protocol`.
const FLAG_SOFT_WRAP: u8 = 0b0000_0001;

/// `len` (2) + three [`Color::Default`] tags (3) + flags (2) + underline
/// style (1) + link (2). The decoder multiplies it by the declared run
/// count to reject an over-long prefix before reserving for it.
const MIN_ATTR_RUN_BYTES: usize = 10;

/// Column (2) + the six sizing bytes.
const MIN_SIZED_CELL_BYTES: usize = 8;

/// Header (4), the run and sized-cell counts (2 each), and the one run
/// every non-empty row carries. Under-reserving costs the daemon a
/// realloc and a full memcpy on every text-bearing row.
const ROW_FIXED_BYTES: usize = 4 + 2 + MIN_ATTR_RUN_BYTES + 2;

#[derive(Debug, Error)]
pub enum RowCodecError {
    /// Never forward compatibility: the sender may only use a version
    /// the effective minor defines, so this is corruption.
    #[error("row codec: version {found}, expected {expected}")]
    UnknownVersion { expected: u8, found: u8 },
    #[error("row codec: {field} needs {need} more byte(s), {found} left")]
    Truncated {
        field: &'static str,
        need: usize,
        found: usize,
    },
    /// Reported before the allocation the prefix would drive.
    #[error("row codec: {field} declares {found}, over the {limit} cap")]
    OverLimit {
        field: &'static str,
        limit: usize,
        found: usize,
    },
    #[error("row codec: {field} holds {found}, which names no value")]
    Invalid { field: &'static str, found: u32 },
    #[error("row codec: the sized cell at column {col} is not a valid OSC 66 sizing")]
    InvalidSizing { col: u16 },
    #[error("row codec: a sized cell names column {col} of a {cols}-column row")]
    SizedCellColumn { col: u16, cols: usize },
    #[error("row codec: attr_runs cover {covered} cells, graphemes hold {graphemes}")]
    RunLengthMismatch {
        /// Sum of the run lengths (through the offending run).
        covered: usize,
        /// Number of decoded graphemes (the true column count).
        graphemes: usize,
    },
    /// Slack means the payload and this decoder disagree about the
    /// layout.
    #[error("row codec: {found} byte(s) left after the row")]
    TrailingBytes { found: usize },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DecodedRow {
    pub cells: Vec<Cell>,
    /// The caller installs each sizing into its grid and stamps the
    /// cell with the handle.
    pub sized_cells: Vec<(u16, Sizing)>,
    /// The shadow mirrors it via `Grid::set_soft_wrap` so client-side
    /// triple-click stitches the same way daemon-side search does.
    pub soft_wrap_continued: bool,
}

#[derive(Clone, Copy)]
pub struct RowEncode<'a> {
    pub cells: &'a [Cell],
    /// Columns in `[cells.len()..pad_to)` encode as [`Cell::BLANK`]; a
    /// `pad_to` below `cells.len()` is raised to it. The daemon passes
    /// the grid width so it can read [`crate::Grid::row_content`]
    /// (`[0..occ)`) yet ship a `cols`-wide row; the blank tail costs one
    /// RLE run.
    pub pad_to: usize,
    pub sized_cells: &'a [(u16, Sizing)],
    pub soft_wrap_continued: bool,
}

// ── Encode ──────────────────────────────────────────────────────────

/// `docs/reference/row-codec.md`. Writes straight from the borrowed
/// `&[Cell]`, no intermediate `Vec`.
pub fn encode_row(row: RowEncode<'_>, styles: &StyleTable) -> Result<Vec<u8>, RowCodecError> {
    let cols = row.pad_to.max(row.cells.len());
    if cols > MAX_CELLS_PER_ROW {
        return Err(RowCodecError::OverLimit {
            field: "cols",
            limit: MAX_CELLS_PER_ROW,
            found: cols,
        });
    }
    if row.sized_cells.len() > MAX_SIZED_CELLS {
        return Err(RowCodecError::OverLimit {
            field: "sized_cells",
            limit: MAX_SIZED_CELLS,
            found: row.sized_cells.len(),
        });
    }

    // Two bytes per column covers an ASCII grapheme record, so a common
    // `cat` line lands exactly on the reservation and never reallocates.
    let mut out = Vec::with_capacity(
        ROW_FIXED_BYTES + cols * 2 + row.sized_cells.len() * MIN_SIZED_CELL_BYTES,
    );
    out.push(ROW_CODEC_VERSION);
    out.push(if row.soft_wrap_continued {
        FLAG_SOFT_WRAP
    } else {
        0
    });
    out.extend_from_slice(&(cols as u16).to_le_bytes());
    for i in 0..cols {
        put_grapheme(
            &mut out,
            row.cells
                .get(i)
                .map_or(Cell::BLANK.grapheme, |c| c.grapheme),
        );
    }
    put_attr_runs(&mut out, row.cells, cols, styles);
    out.extend_from_slice(&(row.sized_cells.len() as u16).to_le_bytes());
    for (col, sizing) in row.sized_cells {
        out.extend_from_slice(&col.to_le_bytes());
        put_sizing(&mut out, *sizing);
    }
    Ok(out)
}

/// Defaults past the backing slice so a clipped row's blank tail
/// coalesces into one run. Keyed on the interned id: equal iff the pens
/// are equal.
fn key_at(cells: &[Cell], i: usize) -> (StyleId, Option<NonZeroU16>) {
    cells
        .get(i)
        .map_or((StyleId::DEFAULT, None), |c| (c.style, c.link))
}

/// The count is back-patched: counting the runs first walks every
/// column twice on the daemon's hottest path.
fn put_attr_runs(out: &mut Vec<u8>, cells: &[Cell], cols: usize, styles: &StyleTable) {
    let count_at = out.len();
    out.extend_from_slice(&[0, 0]);
    if cols == 0 {
        return;
    }
    let mut runs: u16 = 0;
    let (mut run_style, mut run_link) = key_at(cells, 0);
    // `cols <= u16::MAX` (checked by the caller), so neither counter
    // overflows.
    let mut run_len: u16 = 1;
    for i in 1..cols {
        let key = key_at(cells, i);
        if key == (run_style, run_link) {
            run_len += 1;
        } else {
            put_attr_run(out, run_len, styles.resolve(run_style), run_link);
            runs += 1;
            (run_style, run_link) = key;
            run_len = 1;
        }
    }
    put_attr_run(out, run_len, styles.resolve(run_style), run_link);
    runs += 1;
    out[count_at..count_at + 2].copy_from_slice(&runs.to_le_bytes());
}

fn put_attr_run(out: &mut Vec<u8>, len: u16, attrs: &Attributes, link: Option<NonZeroU16>) {
    out.extend_from_slice(&len.to_le_bytes());
    put_color(out, attrs.fg);
    put_color(out, attrs.bg);
    put_color(out, attrs.underline_color);
    out.extend_from_slice(&attrs.flags.bits().to_le_bytes());
    out.push(underline_style_tag(attrs.underline_style));
    out.extend_from_slice(&link.map_or(0, NonZeroU16::get).to_le_bytes());
}

fn put_grapheme(out: &mut Vec<u8>, grapheme: Grapheme) {
    match grapheme {
        Grapheme::Empty => out.push(0),
        Grapheme::Ascii(b) => out.extend_from_slice(&[1, b]),
        Grapheme::Char(c) => {
            out.push(2);
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        Grapheme::Cluster(handle) => {
            out.push(3);
            put_varint_u32(out, handle.get());
        }
        Grapheme::Spacer => out.push(4),
        Grapheme::SizedSpacer => out.push(5),
    }
}

fn put_color(out: &mut Vec<u8>, color: Color) {
    match color {
        Color::Default => out.push(0),
        Color::Indexed(n) => out.extend_from_slice(&[1, n]),
        Color::Rgb(r, g, b) => out.extend_from_slice(&[2, r, g, b]),
    }
}

fn put_sizing(out: &mut Vec<u8>, sizing: Sizing) {
    out.extend_from_slice(&[
        sizing.scale(),
        sizing.cell_width(),
        sizing.frac_num(),
        sizing.frac_den(),
        valign_tag(sizing.valign()),
        halign_tag(sizing.halign()),
    ]);
}

// The three data-free enums are matched both ways rather than cast
// through their discriminants: the wire values are the spec's, so
// reordering a Rust declaration must not be able to move them.
const fn underline_style_tag(style: UnderlineStyle) -> u8 {
    match style {
        UnderlineStyle::Single => 0,
        UnderlineStyle::Double => 1,
        UnderlineStyle::Curly => 2,
        UnderlineStyle::Dotted => 3,
        UnderlineStyle::Dashed => 4,
    }
}

const fn valign_tag(valign: VAlign) -> u8 {
    match valign {
        VAlign::Top => 0,
        VAlign::Bottom => 1,
        VAlign::Center => 2,
    }
}

const fn halign_tag(halign: HAlign) -> u8 {
    match halign {
        HAlign::Left => 0,
        HAlign::Right => 1,
        HAlign::Center => 2,
    }
}

/// LEB128, shortest form. Only the cluster handle uses it: handles are
/// small in practice, so a varint saves three bytes per cluster cell
/// over a fixed `u32`.
fn put_varint_u32(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

// ── Decode ──────────────────────────────────────────────────────────

/// Every read names its field so a truncation error can say what ran
/// out.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    const fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// Check without consuming: the limits-before-allocation gate,
    /// applied to a length prefix times the smallest element encoding.
    const fn need(&self, n: usize, field: &'static str) -> Result<(), RowCodecError> {
        if self.remaining() < n {
            return Err(RowCodecError::Truncated {
                field,
                need: n,
                found: self.remaining(),
            });
        }
        Ok(())
    }

    fn take(&mut self, n: usize, field: &'static str) -> Result<&'a [u8], RowCodecError> {
        self.need(n, field)?;
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self, field: &'static str) -> Result<u8, RowCodecError> {
        Ok(self.take(1, field)?[0])
    }

    fn u16(&mut self, field: &'static str) -> Result<u16, RowCodecError> {
        let bytes = self.take(2, field)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// A non-minimal encoding is rejected: two spellings of one value
    /// make the encoding non-bijective, and the golden vectors assert
    /// exact bytes both ways.
    fn varint_u32(&mut self, field: &'static str) -> Result<u32, RowCodecError> {
        let mut value: u32 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.u8(field)?;
            let payload = u32::from(byte & 0x7F);
            // The fifth byte carries only the top four bits of a u32.
            if shift == 28 && payload > 0x0F {
                return Err(RowCodecError::Invalid {
                    field,
                    found: u32::from(byte),
                });
            }
            value |= payload << shift;
            if byte & 0x80 == 0 {
                // A zero final byte past the first means the previous
                // byte set its continuation bit for nothing.
                if byte == 0 && shift != 0 {
                    return Err(RowCodecError::Invalid { field, found: 0 });
                }
                return Ok(value);
            }
            shift += 7;
            if shift > 28 {
                return Err(RowCodecError::Invalid {
                    field,
                    found: u32::from(byte),
                });
            }
        }
    }

    fn grapheme(&mut self) -> Result<Grapheme, RowCodecError> {
        let tag = self.u8("grapheme tag")?;
        match tag {
            0 => Ok(Grapheme::Empty),
            1 => {
                let byte = self.u8("grapheme ascii")?;
                // `Grapheme::Ascii`'s domain is printable ASCII; a
                // control byte here would mint a cell no encoder
                // produces, which selection and the ANSI re-encoder hand
                // on verbatim.
                if !(0x20..=0x7E).contains(&byte) {
                    return Err(RowCodecError::Invalid {
                        field: "grapheme ascii",
                        found: u32::from(byte),
                    });
                }
                Ok(Grapheme::Ascii(byte))
            }
            2 => self.utf8_char(),
            3 => {
                let handle = self.varint_u32("grapheme cluster")?;
                NonZeroU32::new(handle)
                    .map(Grapheme::Cluster)
                    .ok_or(RowCodecError::Invalid {
                        field: "grapheme cluster",
                        found: 0,
                    })
            }
            4 => Ok(Grapheme::Spacer),
            5 => Ok(Grapheme::SizedSpacer),
            other => Err(RowCodecError::Invalid {
                field: "grapheme tag",
                found: u32::from(other),
            }),
        }
    }

    /// Validated through `str::from_utf8` so an overlong form, a
    /// surrogate, or a value past `U+10FFFF` is rejected rather than
    /// reassembled into a scalar the encoder never produced.
    fn utf8_char(&mut self) -> Result<Grapheme, RowCodecError> {
        const FIELD: &str = "grapheme char";
        self.need(1, FIELD)?;
        let lead = self.bytes[self.pos];
        let len = match lead {
            0x00..=0x7F => 1,
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            // 0x80..=0xC1 and 0xF5..=0xFF start no valid sequence.
            _ => {
                return Err(RowCodecError::Invalid {
                    field: FIELD,
                    found: u32::from(lead),
                });
            }
        };
        let bytes = self.take(len, FIELD)?;
        let text = core::str::from_utf8(bytes).map_err(|_| RowCodecError::Invalid {
            field: FIELD,
            found: u32::from(lead),
        })?;
        text.chars()
            .next()
            .map(Grapheme::Char)
            .ok_or_else(|| RowCodecError::Invalid {
                field: FIELD,
                found: u32::from(lead),
            })
    }

    fn color(&mut self, field: &'static str) -> Result<Color, RowCodecError> {
        match self.u8(field)? {
            0 => Ok(Color::Default),
            1 => Ok(Color::Indexed(self.u8(field)?)),
            2 => {
                let rgb = self.take(3, field)?;
                Ok(Color::Rgb(rgb[0], rgb[1], rgb[2]))
            }
            other => Err(RowCodecError::Invalid {
                field,
                found: u32::from(other),
            }),
        }
    }

    fn attributes(&mut self) -> Result<Attributes, RowCodecError> {
        let fg = self.color("attr_run fg")?;
        let bg = self.color("attr_run bg")?;
        let underline_color = self.color("attr_run underline_color")?;
        let bits = self.u16("attr_run flags")?;
        // Undefined bits are rejected rather than truncated away: they
        // would round-trip to a different row than the sender encoded.
        let flags = AttrFlags::from_bits(bits).ok_or_else(|| RowCodecError::Invalid {
            field: "attr_run flags",
            found: u32::from(bits),
        })?;
        let style_tag = self.u8("attr_run underline_style")?;
        let underline_style = match style_tag {
            0 => UnderlineStyle::Single,
            1 => UnderlineStyle::Double,
            2 => UnderlineStyle::Curly,
            3 => UnderlineStyle::Dotted,
            4 => UnderlineStyle::Dashed,
            other => {
                return Err(RowCodecError::Invalid {
                    field: "attr_run underline_style",
                    found: u32::from(other),
                });
            }
        };
        Ok(Attributes {
            fg,
            bg,
            underline_color,
            flags,
            underline_style,
        })
    }

    fn sizing(&mut self, col: u16) -> Result<Sizing, RowCodecError> {
        let bytes = self.take(6, "sized_cell sizing")?;
        let valign = match bytes[4] {
            0 => VAlign::Top,
            1 => VAlign::Bottom,
            2 => VAlign::Center,
            other => {
                return Err(RowCodecError::Invalid {
                    field: "sized_cell valign",
                    found: u32::from(other),
                });
            }
        };
        let halign = match bytes[5] {
            0 => HAlign::Left,
            1 => HAlign::Right,
            2 => HAlign::Center,
            other => {
                return Err(RowCodecError::Invalid {
                    field: "sized_cell halign",
                    found: u32::from(other),
                });
            }
        };
        // `Sizing::new` is the one gate: a `Sizing` off the wire passes
        // exactly the ranges an OSC 66 parse does, so no consumer
        // re-validates.
        Sizing::new(bytes[0], bytes[1], bytes[2], bytes[3], valign, halign)
            .ok_or(RowCodecError::InvalidSizing { col })
    }
}

/// Decodes arbitrary bytes into one well-formed row or returns an error.
/// Interns resolved [`Attributes`] into the decoder's `styles` table;
/// daemon style IDs never cross the wire. Counts use `u16` width caps.
pub fn decode_row(bytes: &[u8], styles: &mut StyleTable) -> Result<DecodedRow, RowCodecError> {
    let mut r = Reader::new(bytes);
    let version = r.u8("version")?;
    if version != ROW_CODEC_VERSION {
        return Err(RowCodecError::UnknownVersion {
            expected: ROW_CODEC_VERSION,
            found: version,
        });
    }
    let flags = r.u8("flags")?;
    if flags & !FLAG_SOFT_WRAP != 0 {
        return Err(RowCodecError::Invalid {
            field: "flags",
            found: u32::from(flags),
        });
    }
    let soft_wrap_continued = flags & FLAG_SOFT_WRAP != 0;

    let cols = usize::from(r.u16("cols")?);
    if cols > MAX_CELLS_PER_ROW {
        return Err(RowCodecError::OverLimit {
            field: "cols",
            limit: MAX_CELLS_PER_ROW,
            found: cols,
        });
    }
    // A grapheme is at least its tag byte, so the input size bounds the
    // reservation instead of a peer's claim.
    r.need(cols, "graphemes")?;
    let mut cells = Vec::with_capacity(cols);
    for _ in 0..cols {
        cells.push(Cell {
            grapheme: r.grapheme()?,
            style: StyleId::DEFAULT,
            link: None,
            // OSC 66 sizing rides the `sized_cells` band, not the cell
            // stream (docs/explanation/data-model/grid-and-cells.md).
            sizing: None,
        });
    }

    let run_count = usize::from(r.u16("attr_runs")?);
    if run_count > MAX_ATTR_RUNS {
        return Err(RowCodecError::OverLimit {
            field: "attr_runs",
            limit: MAX_ATTR_RUNS,
            found: run_count,
        });
    }
    r.need(run_count * MIN_ATTR_RUN_BYTES, "attr_runs")?;
    let mut covered = 0usize;
    for _ in 0..run_count {
        let len = usize::from(r.u16("attr_run len")?);
        if len == 0 {
            return Err(RowCodecError::Invalid {
                field: "attr_run len",
                found: 0,
            });
        }
        let attrs = r.attributes()?;
        let link = NonZeroU16::new(r.u16("attr_run link")?);
        let end = covered + len;
        // Report rather than clamp, which would desync every later
        // column.
        if end > cells.len() {
            return Err(RowCodecError::RunLengthMismatch {
                covered: end,
                graphemes: cells.len(),
            });
        }
        let style = styles.intern(attrs);
        for cell in &mut cells[covered..end] {
            cell.style = style;
            cell.link = link;
        }
        covered = end;
    }
    // Leftover graphemes with no covering run is the mirror failure.
    if covered != cells.len() {
        return Err(RowCodecError::RunLengthMismatch {
            covered,
            graphemes: cells.len(),
        });
    }

    let sized_count = usize::from(r.u16("sized_cells")?);
    if sized_count > MAX_SIZED_CELLS {
        return Err(RowCodecError::OverLimit {
            field: "sized_cells",
            limit: MAX_SIZED_CELLS,
            found: sized_count,
        });
    }
    r.need(sized_count * MIN_SIZED_CELL_BYTES, "sized_cells")?;
    let mut sized_cells = Vec::with_capacity(sized_count);
    for _ in 0..sized_count {
        let col = r.u16("sized_cell col")?;
        if usize::from(col) >= cells.len() {
            return Err(RowCodecError::SizedCellColumn {
                col,
                cols: cells.len(),
            });
        }
        sized_cells.push((col, r.sizing(col)?));
    }

    if r.remaining() != 0 {
        return Err(RowCodecError::TrailingBytes {
            found: r.remaining(),
        });
    }
    Ok(DecodedRow {
        cells,
        sized_cells,
        soft_wrap_continued,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttrFlags, Color};
    use felis_protocol::kitty_text_sizing::{HAlign, VAlign};

    fn encode_plain(cells: &[Cell], styles: &StyleTable) -> Vec<u8> {
        encode_row(
            RowEncode {
                cells,
                pad_to: cells.len(),
                sized_cells: &[],
                soft_wrap_continued: false,
            },
            styles,
        )
        .unwrap()
    }

    // ── Golden vectors ──────────────────────────────────────────────
    // `docs/reference/row-codec.md` publishes these same rows. Asserted
    // in both directions, so a change to either half that the round-trip
    // tests would absorb fails here.

    const GOLDEN_EMPTY: &[u8] = &[
        0x01, // version
        0x00, // flags: no soft wrap
        0x00, 0x00, // cols = 0
        0x00, 0x00, // attr_runs = 0
        0x00, 0x00, // sized_cells = 0
    ];

    const GOLDEN_ASCII: &[u8] = &[
        0x01, // version
        0x00, // flags
        0x02, 0x00, // cols = 2
        0x01, b'h', // Ascii 'h'
        0x01, b'i', // Ascii 'i'
        0x01, 0x00, // attr_runs = 1
        0x02, 0x00, // run len = 2
        0x00, // fg Default
        0x00, // bg Default
        0x00, // underline_color Default
        0x00, 0x00, // flags = 0
        0x00, // underline_style Single
        0x00, 0x00, // link = none
        0x00, 0x00, // sized_cells = 0
    ];

    /// Every field that has more than one shape.
    const GOLDEN_RICH: &[u8] = &[
        0x01, // version
        0x01, // flags: soft-wrap continuation
        0x03, 0x00, // cols = 3
        0x02, 0xE3, 0x81, 0x82, // Char 'あ' (U+3042) as UTF-8
        0x03, 0xAC, 0x02, // Cluster 300 (LEB128)
        0x04, // Spacer
        0x02, 0x00, // attr_runs = 2
        // run 0: one column, no link
        0x01, 0x00, // len = 1
        0x02, 0xAB, 0xCD, 0xEF, // fg Rgb
        0x01, 0x04, // bg Indexed(4)
        0x00, // underline_color Default
        0x09, 0x00, // flags = BOLD | UNDERLINE
        0x02, // underline_style Curly
        0x00, 0x00, // link = none
        // run 1: two columns, link slot 7
        0x02, 0x00, // len = 2
        0x02, 0xAB, 0xCD, 0xEF, // fg Rgb
        0x01, 0x04, // bg Indexed(4)
        0x00, // underline_color Default
        0x09, 0x00, // flags
        0x02, // underline_style Curly
        0x07, 0x00, // link = 7
        0x01, 0x00, // sized_cells = 1
        0x01, 0x00, // column 1
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, // scale 2, top-left, no fraction
    ];

    fn golden_rich_row() -> (Vec<Cell>, StyleTable, Vec<(u16, Sizing)>) {
        let mut styles = StyleTable::new();
        let pen = styles.intern(Attributes {
            fg: Color::Rgb(0xAB, 0xCD, 0xEF),
            bg: Color::Indexed(4),
            underline_color: Color::Default,
            flags: AttrFlags::BOLD | AttrFlags::UNDERLINE,
            underline_style: UnderlineStyle::Curly,
        });
        let link = NonZeroU16::new(7);
        let cells = vec![
            Cell {
                grapheme: Grapheme::Char('あ'),
                style: pen,
                link: None,
                sizing: None,
            },
            Cell {
                grapheme: Grapheme::Cluster(NonZeroU32::new(300).unwrap()),
                style: pen,
                link,
                sizing: None,
            },
            Cell {
                grapheme: Grapheme::Spacer,
                style: pen,
                link,
                sizing: None,
            },
        ];
        let sized = vec![(
            1,
            Sizing::new(2, 0, 0, 0, VAlign::Top, HAlign::Left).unwrap(),
        )];
        (cells, styles, sized)
    }

    #[test]
    fn the_empty_row_matches_its_golden_vector() {
        assert_eq!(encode_plain(&[], &StyleTable::new()), GOLDEN_EMPTY);
        let back = decode_row(GOLDEN_EMPTY, &mut StyleTable::new()).unwrap();
        assert_eq!(back, DecodedRow::default());
    }

    #[test]
    fn the_ascii_row_matches_its_golden_vector() {
        let cells = vec![
            Cell {
                grapheme: Grapheme::Ascii(b'h'),
                ..Cell::BLANK
            },
            Cell {
                grapheme: Grapheme::Ascii(b'i'),
                ..Cell::BLANK
            },
        ];
        assert_eq!(encode_plain(&cells, &StyleTable::new()), GOLDEN_ASCII);
        let back = decode_row(GOLDEN_ASCII, &mut StyleTable::new()).unwrap();
        assert_eq!(back.cells, cells);
        assert_eq!(back.sized_cells, Vec::<(u16, Sizing)>::new());
        assert!(!back.soft_wrap_continued);
    }

    #[test]
    fn the_rich_row_matches_its_golden_vector() {
        let (cells, styles, sized) = golden_rich_row();
        let bytes = encode_row(
            RowEncode {
                cells: &cells,
                pad_to: cells.len(),
                sized_cells: &sized,
                soft_wrap_continued: true,
            },
            &styles,
        )
        .unwrap();
        assert_eq!(bytes, GOLDEN_RICH);

        let back = decode_row(GOLDEN_RICH, &mut StyleTable::new()).unwrap();
        assert_eq!(back.cells, cells);
        assert_eq!(back.sized_cells, sized);
        assert!(back.soft_wrap_continued);
    }

    /// Derived from real encodings rather than trusted: a field added to
    /// a run or a sized cell must move them, or the guard under-counts.
    #[test]
    fn the_minimum_element_sizes_match_what_the_encoder_writes() {
        // `GOLDEN_ASCII` is header + two 2-byte graphemes + count + one
        // minimal run + the empty band.
        assert_eq!(GOLDEN_ASCII.len(), 4 + 2 * 2 + 2 + MIN_ATTR_RUN_BYTES + 2);
        let sized = vec![(0, Sizing::default())];
        let with_band = encode_row(
            RowEncode {
                cells: &[],
                pad_to: 0,
                sized_cells: &sized,
                soft_wrap_continued: false,
            },
            &StyleTable::new(),
        )
        .unwrap();
        assert_eq!(with_band.len(), GOLDEN_EMPTY.len() + MIN_SIZED_CELL_BYTES);
    }

    /// The daemon encodes one per dirty row on its hottest path, so a
    /// shortfall buys a realloc plus a full memcpy.
    #[test]
    fn a_plain_ascii_row_fits_its_reservation() {
        let cells: Vec<Cell> = (0..80u8)
            .map(|i| Cell {
                grapheme: Grapheme::Ascii(b'a' + i % 26),
                ..Cell::BLANK
            })
            .collect();
        let bytes = encode_plain(&cells, &StyleTable::new());
        assert_eq!(bytes.len(), 4 + 80 * 2 + 2 + MIN_ATTR_RUN_BYTES + 2);
        assert_eq!(
            bytes.len(),
            bytes.capacity(),
            "an 80-column ASCII row outgrew its reservation and reallocated"
        );
    }

    // ── Malformed vectors ───────────────────────────────────────────
    // Raw bytes, never a forged Rust value: a decoder written from the
    // spec in any language must reject these.

    fn patched(at: usize, to: u8) -> Vec<u8> {
        let mut bytes = GOLDEN_RICH.to_vec();
        bytes[at] = to;
        bytes
    }

    fn decode_err(bytes: &[u8]) -> RowCodecError {
        decode_row(bytes, &mut StyleTable::new()).expect_err("must be rejected")
    }

    #[test]
    fn an_unknown_version_tag_is_rejected() {
        assert!(matches!(
            decode_err(&patched(0, 2)),
            RowCodecError::UnknownVersion {
                expected: ROW_CODEC_VERSION,
                found: 2
            }
        ));
    }

    #[test]
    fn a_reserved_flag_bit_is_rejected() {
        assert!(matches!(
            decode_err(&patched(1, 0b0000_0010)),
            RowCodecError::Invalid { field: "flags", .. }
        ));
    }

    /// Every field boundary is a separate length check.
    #[test]
    fn truncation_at_any_boundary_is_rejected() {
        for vector in [GOLDEN_EMPTY, GOLDEN_ASCII, GOLDEN_RICH] {
            for keep in 0..vector.len() {
                let err = decode_err(&vector[..keep]);
                assert!(
                    matches!(err, RowCodecError::Truncated { .. }),
                    "{keep} of {} bytes gave {err:?}",
                    vector.len()
                );
            }
        }
    }

    #[test]
    fn trailing_bytes_are_an_error_not_slack() {
        let mut bytes = GOLDEN_ASCII.to_vec();
        bytes.push(0x00);
        assert!(matches!(
            decode_err(&bytes),
            RowCodecError::TrailingBytes { found: 1 }
        ));
    }

    /// A peer must not be able to make the decoder reserve 64 Ki cells
    /// from an 8-byte frame.
    #[test]
    fn an_over_long_length_prefix_is_rejected_before_allocating() {
        let cases = [
            (2, "cols", MAX_CELLS_PER_ROW),
            (4, "attr_runs", MAX_ATTR_RUNS),
            (6, "sized_cells", MAX_SIZED_CELLS),
        ];
        for (at, field, limit) in cases {
            let mut bytes = GOLDEN_EMPTY.to_vec();
            bytes[at] = 0xFF;
            bytes[at + 1] = 0xFF;
            match decode_row(&bytes, &mut StyleTable::new()) {
                Err(RowCodecError::OverLimit {
                    field: got_field,
                    limit: got_limit,
                    found,
                }) => {
                    assert_eq!(got_field, field);
                    assert_eq!(got_limit, limit);
                    assert_eq!(found, 0xFFFF);
                }
                other => panic!("{field}: expected a pre-allocation rejection, got {other:?}"),
            }
        }
    }

    /// Every sized cell names a column of the row it rides, so the band
    /// cannot point past the decoded cells.
    #[test]
    fn a_sized_cell_past_the_decoded_row_is_rejected() {
        let cells = [Cell::BLANK; 2];
        let sizing = Sizing::new(1, 0, 0, 0, VAlign::Top, HAlign::Left).expect("sizing");
        let bytes = encode_row(
            RowEncode {
                cells: &cells,
                pad_to: cells.len(),
                sized_cells: &[(2, sizing)],
                soft_wrap_continued: false,
            },
            &StyleTable::new(),
        )
        .expect("encode");
        assert!(matches!(
            decode_err(&bytes),
            RowCodecError::SizedCellColumn { col: 2, cols: 2 }
        ));
    }

    /// A zero-length run would let a payload carry unbounded no-op runs.
    #[test]
    fn a_zero_length_run_is_rejected() {
        // `GOLDEN_ASCII`'s single run length sits right after the count.
        let mut bytes = GOLDEN_ASCII.to_vec();
        bytes[10] = 0;
        bytes[11] = 0;
        assert!(matches!(
            decode_err(&bytes),
            RowCodecError::Invalid {
                field: "attr_run len",
                found: 0
            }
        ));
    }

    /// Rejected rather than clamped: a desync must not shift every later
    /// column.
    #[test]
    fn runs_that_do_not_cover_the_graphemes_exactly_are_rejected() {
        let mut over = GOLDEN_ASCII.to_vec();
        over[10] = 4; // run claims 4 of the 2 columns
        match decode_err(&over) {
            RowCodecError::RunLengthMismatch { covered, graphemes } => {
                assert_eq!((covered, graphemes), (4, 2));
            }
            other => panic!("expected RunLengthMismatch, got {other:?}"),
        }

        let mut under = GOLDEN_ASCII.to_vec();
        under[10] = 1; // run covers 1 of the 2 columns
        match decode_err(&under) {
            RowCodecError::RunLengthMismatch { covered, graphemes } => {
                assert_eq!((covered, graphemes), (1, 2));
            }
            other => panic!("expected RunLengthMismatch, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_grapheme_tag_is_rejected() {
        assert!(matches!(
            decode_err(&patched(4, 6)),
            RowCodecError::Invalid {
                field: "grapheme tag",
                found: 6
            }
        ));
    }

    /// A high bit means the producer was not writing ASCII.
    #[test]
    fn a_non_ascii_ascii_payload_is_rejected() {
        let mut bytes = GOLDEN_ASCII.to_vec();
        bytes[5] = 0x80;
        assert!(matches!(
            decode_err(&bytes),
            RowCodecError::Invalid {
                field: "grapheme ascii",
                found: 0x80
            }
        ));
    }

    /// A peer that shipped `ESC` under the `Ascii` tag would mint a cell
    /// whose text reaches the clipboard as a live control sequence.
    #[test]
    fn a_control_byte_under_the_ascii_tag_is_rejected() {
        for byte in [0x00u8, 0x1B, 0x7F] {
            let mut bytes = GOLDEN_ASCII.to_vec();
            bytes[5] = byte;
            assert!(
                matches!(
                    decode_err(&bytes),
                    RowCodecError::Invalid {
                        field: "grapheme ascii",
                        found
                    } if found == u32::from(byte)
                ),
                "control byte {byte:#04X} under the Ascii tag was accepted"
            );
        }
    }

    /// `str::from_utf8` rejects both, so one row never has two spellings.
    #[test]
    fn a_malformed_utf8_scalar_is_rejected() {
        // C0 80: the overlong two-byte encoding of U+0000.
        let mut overlong = GOLDEN_RICH.to_vec();
        overlong.splice(4..8, [0x02, 0xC0, 0x80]);
        assert!(matches!(
            decode_err(&overlong),
            RowCodecError::Invalid {
                field: "grapheme char",
                ..
            }
        ));
        // ED A0 80: the UTF-16 surrogate U+D800.
        let mut surrogate = GOLDEN_RICH.to_vec();
        surrogate.splice(5..8, [0xED, 0xA0, 0x80]);
        assert!(matches!(
            decode_err(&surrogate),
            RowCodecError::Invalid {
                field: "grapheme char",
                ..
            }
        ));
    }

    #[test]
    fn a_non_minimal_varint_is_rejected() {
        // 0x80 0x00 is "0" spelled in two bytes (zero is not a cluster
        // handle anyway, so the canonicality check has to fire first).
        let mut bytes = GOLDEN_RICH.to_vec();
        bytes.splice(8..11, [0x03, 0x80, 0x00]);
        assert!(matches!(
            decode_err(&bytes),
            RowCodecError::Invalid {
                field: "grapheme cluster",
                ..
            }
        ));
    }

    #[test]
    fn a_zero_cluster_handle_is_rejected() {
        let mut bytes = GOLDEN_RICH.to_vec();
        bytes.splice(8..11, [0x03, 0x00]);
        assert!(matches!(
            decode_err(&bytes),
            RowCodecError::Invalid {
                field: "grapheme cluster",
                found: 0
            }
        ));
    }

    #[test]
    fn undefined_attribute_bits_and_enum_tags_are_rejected() {
        // Flag bit 15 is not an `AttrFlags` member.
        let mut flags = GOLDEN_ASCII.to_vec();
        flags[16] = 0x80;
        assert!(matches!(
            decode_err(&flags),
            RowCodecError::Invalid {
                field: "attr_run flags",
                ..
            }
        ));

        let mut style = GOLDEN_ASCII.to_vec();
        style[17] = 5;
        assert!(matches!(
            decode_err(&style),
            RowCodecError::Invalid {
                field: "attr_run underline_style",
                found: 5
            }
        ));

        let mut color = GOLDEN_ASCII.to_vec();
        color[12] = 3;
        assert!(matches!(
            decode_err(&color),
            RowCodecError::Invalid {
                field: "attr_run fg",
                found: 3
            }
        ));
    }

    #[test]
    fn a_sizing_outside_the_osc_66_ranges_is_rejected() {
        let mut bytes = GOLDEN_RICH.to_vec();
        let scale = bytes.len() - 6;
        bytes[scale] = 0;
        assert!(matches!(
            decode_err(&bytes),
            RowCodecError::InvalidSizing { col: 1 }
        ));

        let mut align = GOLDEN_RICH.to_vec();
        let valign = align.len() - 2;
        align[valign] = 3;
        assert!(matches!(
            decode_err(&align),
            RowCodecError::Invalid {
                field: "sized_cell valign",
                found: 3
            }
        ));
    }

    // ── Behavior ────────────────────────────────────────────────────

    /// A 1000-column blank row must not carry 1000 copies of the default
    /// pen.
    #[test]
    fn uniform_row_collapses_to_one_attr_run() {
        let wide = vec![Cell::default(); 1000];
        let one = vec![Cell::default(); 1];
        let wide_bytes = encode_plain(&wide, &StyleTable::new());
        let one_bytes = encode_plain(&one, &StyleTable::new());
        // The 999 extra columns cost exactly their grapheme tag byte.
        assert_eq!(wide_bytes.len() - one_bytes.len(), 999);
        assert_eq!(
            decode_row(&wide_bytes, &mut StyleTable::new())
                .unwrap()
                .cells,
            wide
        );
    }

    /// Maximality is a rule on the encoder, not one the decoder
    /// enforces: rejecting an uncoalesced peer would make an independent
    /// implementation's failure to coalesce an interop break instead of
    /// a size cost. Re-encoding normalizes it
    /// (`fuzz/fuzz_targets/row_codec.rs` proves a fixed point).
    #[test]
    fn adjacent_runs_sharing_a_pen_decode_and_re_encode_coalesced() {
        let mut split = GOLDEN_ASCII.to_vec();
        // One run of 2 becomes two runs of 1, with the second run's ten
        // bytes copied from the first.
        split[8] = 2;
        split[10] = 1;
        let run: Vec<u8> = split[10..10 + MIN_ATTR_RUN_BYTES].to_vec();
        split.splice(
            10 + MIN_ATTR_RUN_BYTES..10 + MIN_ATTR_RUN_BYTES,
            run.iter().copied(),
        );

        let mut styles = StyleTable::new();
        let row = decode_row(&split, &mut styles).unwrap();
        assert_eq!(row.cells.len(), 2);
        assert_eq!(
            encode_row(
                RowEncode {
                    cells: &row.cells,
                    pad_to: row.cells.len(),
                    sized_cells: &row.sized_cells,
                    soft_wrap_continued: row.soft_wrap_continued,
                },
                &styles,
            )
            .unwrap(),
            GOLDEN_ASCII
        );
    }

    #[test]
    fn a_row_past_the_column_cap_is_refused_on_encode() {
        let err = encode_row(
            RowEncode {
                cells: &[],
                pad_to: MAX_CELLS_PER_ROW + 1,
                sized_cells: &[],
                soft_wrap_continued: false,
            },
            &StyleTable::new(),
        )
        .expect_err("past the cap");
        assert!(matches!(
            err,
            RowCodecError::OverLimit {
                field: "cols",
                limit: MAX_CELLS_PER_ROW,
                ..
            }
        ));
    }
}

// ── Bounded model-checking proofs (run under `cargo kani`) ───────────
// The decoder as a whole allocates and interns through a `HashMap`, so
// its totality belongs to fuzz (`fuzz/fuzz_targets/row_codec.rs`); the
// reader kernel is pure byte arithmetic over a handful of bytes, the
// shape `docs/reference/testing.md` reserves for Kani.
#[cfg(kani)]
mod kani_proofs {
    use super::{Grapheme, Reader, put_varint_u32};

    /// Total on arbitrary bytes, whatever a peer sends.
    #[kani::proof]
    #[kani::unwind(7)]
    fn varint_decode_never_panics() {
        let bytes: [u8; 6] = kani::any();
        let mut reader = Reader::new(&bytes);
        let _ = reader.varint_u32("kani");
    }

    /// Proven over the whole `u32` domain, which no test can enumerate.
    #[kani::proof]
    #[kani::unwind(7)]
    fn varint_round_trips_for_every_u32() {
        let value: u32 = kani::any();
        let mut buf: Vec<u8> = Vec::new();
        put_varint_u32(&mut buf, value);
        let mut reader = Reader::new(&buf);
        let decoded = reader.varint_u32("kani").expect("its own output decodes");
        assert!(decoded == value);
        assert!(reader.remaining() == 0);
    }

    /// Five bytes is the longest record, so the domain reaches every
    /// arm, truncated ones included.
    #[kani::proof]
    #[kani::unwind(7)]
    fn grapheme_decode_never_panics() {
        let bytes: [u8; 5] = kani::any();
        let mut reader = Reader::new(&bytes);
        let _ = reader.grapheme();
    }

    /// An accepted overlong form or surrogate would make two byte strings
    /// decode to one row.
    #[kani::proof]
    #[kani::unwind(7)]
    fn a_decoded_char_re_encodes_to_the_bytes_it_came_from() {
        let bytes: [u8; 5] = kani::any();
        let mut reader = Reader::new(&bytes);
        if let Ok(Grapheme::Char(c)) = reader.grapheme() {
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf).as_bytes();
            assert!(encoded.len() == reader.pos - 1);
            let mut i = 0;
            while i < encoded.len() {
                assert!(encoded[i] == bytes[1 + i]);
                i += 1;
            }
        }
    }
}

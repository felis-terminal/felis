//! v1 DTO for one dirty row's cells. The packed bytes belong to
//! `docs/reference/row-codec.md`; this is the JSON spelling of what
//! they decode to, owned here so a change to how a `Cell` serializes
//! cannot reshape the format.

use std::num::{NonZeroU16, NonZeroU32};

use felis_protocol::RowPayload;
use felis_protocol::kitty_text_sizing::{HAlign, Sizing, VAlign};

use super::JsonError;
use super::grid::plain_enum;
use crate::wire::{self, DecodedRow, RowEncode};
#[cfg(feature = "schema")]
use crate::wire::{MAX_ATTR_RUNS, MAX_CELLS_PER_ROW, MAX_SIZED_CELLS};
use crate::{AttrFlags, Attributes, Cell, Color, Grapheme, StyleTable, UnderlineStyle};

/// Every SGR flag bit this build defines.
pub const ATTR_FLAGS_MAX: u16 = AttrFlags::all().bits();

/// Lowest byte `Grapheme::Ascii` admits (`crate::wire`'s `grapheme`).
pub const ASCII_MIN: u8 = 0x20;

/// Highest byte `Grapheme::Ascii` admits.
pub const ASCII_MAX: u8 = 0x7E;

json_dto! {
    /// A row's cells. One encoding today: attribute runs over a flat
    /// grapheme list, which is what the packed row stores.
    #[serde(tag = "encoding", rename_all = "snake_case")]
    pub enum RowCellsJson {
        Rle {
            /// One entry per column, left to right.
            #[cfg_attr(feature = "schema", schemars(length(max = MAX_CELLS_PER_ROW)))]
            graphemes: Vec<GraphemeJson>,
            /// Consecutive runs covering every grapheme exactly once.
            #[cfg_attr(feature = "schema", schemars(length(max = MAX_ATTR_RUNS)))]
            attr_runs: Vec<AttrRunJson>,
            #[cfg_attr(feature = "schema", schemars(length(max = MAX_SIZED_CELLS)))]
            sized_cells: Vec<SizedCellJson>,
            /// The row continues into the next one without a newline.
            soft_wrap_continued: bool,
        },
    }

    /// What one column holds.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum GraphemeJson {
        /// Never written to; renders as a blank.
        Empty,
        /// 7-bit printable ASCII.
        Ascii {
            #[cfg_attr(feature = "schema", schemars(range(min = ASCII_MIN, max = ASCII_MAX)))]
            byte: u8,
        },
        Char { ch: char },
        /// 1-based handle into the cluster registry `GridMsg::Cluster`
        /// populates.
        Cluster {
            #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
            id: u32,
        },
        /// Right half of a double-wide glyph in the previous column.
        Spacer,
        /// Continuation column of an OSC 66 sized run.
        SizedSpacer,
    }

    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ColorJson {
        /// The renderer's configured default for this channel.
        Default,
        /// xterm-256 palette slot.
        Indexed { index: u8 },
        Rgb { r: u8, g: u8, b: u8 },
    }

    #[serde(rename_all = "snake_case")]
    pub enum UnderlineStyleJson {
        Single,
        Double,
        Curly,
        Dotted,
        Dashed,
    }

    /// One pen. `flags` is a bitmap, bit 0 bold through bit 10
    /// ISO-protected (`docs/reference/row-codec.md` "Attribute runs").
    pub struct AttributesJson {
        pub fg: ColorJson,
        pub bg: ColorJson,
        /// `SGR 58`; `default` means "follow the foreground".
        pub underline_color: ColorJson,
        #[cfg_attr(feature = "schema", schemars(range(max = ATTR_FLAGS_MAX)))]
        pub flags: u16,
        /// Meaningful only while the underline flag is set.
        pub underline_style: UnderlineStyleJson,
    }

    /// One pen applied to `len` consecutive columns.
    pub struct AttrRunJson {
        /// Columns covered; `0` covers nothing and is not a run.
        #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
        pub len: u16,
        pub attrs: AttributesJson,
        /// 1-based handle into the hyperlink registry `GridMsg::Hyperlink`
        /// populates; `null` for an unlinked run.
        #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
        pub link: Option<u16>,
    }

    #[serde(rename_all = "snake_case")]
    pub enum VAlignJson {
        Top,
        Bottom,
        Center,
    }

    #[serde(rename_all = "snake_case")]
    pub enum HAlignJson {
        Left,
        Right,
        Center,
    }

    /// One run's OSC 66 sizing parameters.
    pub struct SizingJson {
        /// `s`, 1..=7.
        #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 7)))]
        pub scale: u8,
        /// `w`, 0..=7; `0` derives the width from the text.
        #[cfg_attr(feature = "schema", schemars(range(max = 7)))]
        pub cell_width: u8,
        /// `n`, 0..=15 and below `frac_den` whenever that is non-zero.
        #[cfg_attr(feature = "schema", schemars(range(max = 15)))]
        pub frac_num: u8,
        /// `d`, 0..=15; `0` means no fractional component.
        #[cfg_attr(feature = "schema", schemars(range(max = 15)))]
        pub frac_den: u8,
        pub valign: VAlignJson,
        pub halign: HAlignJson,
    }

    /// The sizing that applies to one column of this row.
    pub struct SizedCellJson {
        pub col: u16,
        pub sizing: SizingJson,
    }
}

plain_enum!(
    UnderlineStyleJson,
    UnderlineStyle,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed
);
plain_enum!(VAlignJson, VAlign, Top, Bottom, Center);
plain_enum!(HAlignJson, HAlign, Left, Right, Center);

impl From<Grapheme> for GraphemeJson {
    fn from(grapheme: Grapheme) -> Self {
        match grapheme {
            Grapheme::Empty => Self::Empty,
            Grapheme::Ascii(byte) => Self::Ascii { byte },
            Grapheme::Char(ch) => Self::Char { ch },
            Grapheme::Cluster(id) => Self::Cluster { id: id.get() },
            Grapheme::Spacer => Self::Spacer,
            Grapheme::SizedSpacer => Self::SizedSpacer,
        }
    }
}

impl TryFrom<GraphemeJson> for Grapheme {
    type Error = JsonError;

    fn try_from(grapheme: GraphemeJson) -> Result<Self, JsonError> {
        Ok(match grapheme {
            GraphemeJson::Empty => Self::Empty,
            GraphemeJson::Ascii { byte } => {
                if !(ASCII_MIN..=ASCII_MAX).contains(&byte) {
                    return Err(JsonError::field("byte", "not printable ASCII"));
                }
                Self::Ascii(byte)
            }
            GraphemeJson::Char { ch } => Self::Char(ch),
            GraphemeJson::Cluster { id } => Self::Cluster(
                NonZeroU32::new(id)
                    .ok_or_else(|| JsonError::field("id", "a cluster handle is 1-based"))?,
            ),
            GraphemeJson::Spacer => Self::Spacer,
            GraphemeJson::SizedSpacer => Self::SizedSpacer,
        })
    }
}

impl From<Color> for ColorJson {
    fn from(color: Color) -> Self {
        match color {
            Color::Default => Self::Default,
            Color::Indexed(index) => Self::Indexed { index },
            Color::Rgb(r, g, b) => Self::Rgb { r, g, b },
        }
    }
}

impl From<ColorJson> for Color {
    fn from(color: ColorJson) -> Self {
        match color {
            ColorJson::Default => Self::Default,
            ColorJson::Indexed { index } => Self::Indexed(index),
            ColorJson::Rgb { r, g, b } => Self::Rgb(r, g, b),
        }
    }
}

impl From<Attributes> for AttributesJson {
    fn from(attrs: Attributes) -> Self {
        Self {
            fg: attrs.fg.into(),
            bg: attrs.bg.into(),
            underline_color: attrs.underline_color.into(),
            flags: attrs.flags.bits(),
            underline_style: attrs.underline_style.into(),
        }
    }
}

impl TryFrom<AttributesJson> for Attributes {
    type Error = JsonError;

    fn try_from(attrs: AttributesJson) -> Result<Self, JsonError> {
        Ok(Self {
            fg: attrs.fg.into(),
            bg: attrs.bg.into(),
            underline_color: attrs.underline_color.into(),
            flags: AttrFlags::from_bits(attrs.flags)
                .ok_or_else(|| JsonError::field("flags", "a reserved SGR flag bit is set"))?,
            underline_style: attrs.underline_style.into(),
        })
    }
}

impl From<Sizing> for SizingJson {
    fn from(sizing: Sizing) -> Self {
        Self {
            scale: sizing.scale(),
            cell_width: sizing.cell_width(),
            frac_num: sizing.frac_num(),
            frac_den: sizing.frac_den(),
            valign: sizing.valign().into(),
            halign: sizing.halign().into(),
        }
    }
}

impl TryFrom<SizingJson> for Sizing {
    type Error = JsonError;

    fn try_from(sizing: SizingJson) -> Result<Self, JsonError> {
        Self::new(
            sizing.scale,
            sizing.cell_width,
            sizing.frac_num,
            sizing.frac_den,
            sizing.valign.into(),
            sizing.halign.into(),
        )
        .ok_or_else(|| JsonError::field("sizing", "an OSC 66 field outside its spec range"))
    }
}

/// Reads one packed row into its v1 form, run-length encoding the pens
/// exactly as the packed row stores them.
pub(super) fn from_payload(
    payload: &RowPayload,
    styles: &mut StyleTable,
) -> Result<RowCellsJson, JsonError> {
    // Transcoding has no grid: the caller's table interns each run's pen
    // on decode and resolves it here, so the resolved `Attributes` are
    // identical across the recode.
    let row = wire::decode_row(&payload.0, styles)?;
    Ok(RowCellsJson::Rle {
        graphemes: row.cells.iter().map(|c| c.grapheme.into()).collect(),
        attr_runs: attr_runs(&row.cells, styles),
        sized_cells: row
            .sized_cells
            .into_iter()
            .map(|(col, sizing)| SizedCellJson {
                col,
                sizing: sizing.into(),
            })
            .collect(),
        soft_wrap_continued: row.soft_wrap_continued,
    })
}

/// Inverse of [`from_payload`].
pub(super) fn to_payload(
    cells: &RowCellsJson,
    styles: &mut StyleTable,
) -> Result<RowPayload, JsonError> {
    let RowCellsJson::Rle {
        graphemes,
        attr_runs,
        sized_cells,
        soft_wrap_continued,
    } = cells;
    let mut expanded = Vec::with_capacity(graphemes.len());
    let mut graphemes = graphemes.iter();
    for run in attr_runs {
        let style = styles.intern(Attributes::try_from(run.attrs.clone())?);
        let link = link_handle(run.link)?;
        if run.len == 0 {
            return Err(JsonError::field("len", "a run covers at least one column"));
        }
        for _ in 0..run.len {
            let grapheme = graphemes
                .next()
                .ok_or_else(|| JsonError::field("attr_runs", "the runs outrun the graphemes"))?;
            expanded.push(Cell {
                grapheme: grapheme.clone().try_into()?,
                style,
                link,
                sizing: None,
            });
        }
    }
    if graphemes.next().is_some() {
        return Err(JsonError::field(
            "graphemes",
            "the graphemes outrun the attribute runs",
        ));
    }
    let sized = sized_cells
        .iter()
        .map(|entry| {
            if usize::from(entry.col) >= expanded.len() {
                return Err(JsonError::field(
                    "col",
                    "a sized cell names a column the row does not have",
                ));
            }
            Ok((entry.col, Sizing::try_from(entry.sizing.clone())?))
        })
        .collect::<Result<Vec<_>, JsonError>>()?;
    let row = DecodedRow {
        cells: expanded,
        sized_cells: sized,
        soft_wrap_continued: *soft_wrap_continued,
    };
    Ok(RowPayload(wire::encode_row(
        RowEncode {
            cells: &row.cells,
            pad_to: row.cells.len(),
            sized_cells: &row.sized_cells,
            soft_wrap_continued: row.soft_wrap_continued,
        },
        styles,
    )?))
}

fn link_handle(link: Option<u16>) -> Result<Option<NonZeroU16>, JsonError> {
    match link {
        None => Ok(None),
        Some(raw) => NonZeroU16::new(raw)
            .map(Some)
            .ok_or_else(|| JsonError::field("link", "a hyperlink handle is 1-based")),
    }
}

fn attr_runs(cells: &[Cell], styles: &StyleTable) -> Vec<AttrRunJson> {
    let mut runs: Vec<AttrRunJson> = Vec::new();
    for cell in cells {
        let attrs = AttributesJson::from(*styles.resolve(cell.style));
        let link = cell.link.map(NonZeroU16::get);
        match runs.last_mut() {
            Some(last) if last.attrs == attrs && last.link == link && last.len < u16::MAX => {
                last.len += 1;
            }
            _ => runs.push(AttrRunJson {
                len: 1,
                attrs,
                link,
            }),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use felis_protocol::kitty_text_sizing::{HAlign, VAlign};

    fn rle(flags: u16) -> RowCellsJson {
        RowCellsJson::Rle {
            graphemes: vec![GraphemeJson::Ascii { byte: b'x' }],
            attr_runs: vec![AttrRunJson {
                len: 1,
                attrs: AttributesJson {
                    fg: ColorJson::Default,
                    bg: ColorJson::Default,
                    underline_color: ColorJson::Default,
                    flags,
                    underline_style: UnderlineStyleJson::Single,
                },
                link: None,
            }],
            sized_cells: Vec::new(),
            soft_wrap_continued: false,
        }
    }

    fn indexed_row(fgs: &[u8]) -> RowCellsJson {
        RowCellsJson::Rle {
            graphemes: fgs
                .iter()
                .map(|_| GraphemeJson::Ascii { byte: b'x' })
                .collect(),
            attr_runs: fgs
                .iter()
                .map(|&index| AttrRunJson {
                    len: 1,
                    attrs: AttributesJson {
                        fg: ColorJson::Indexed { index },
                        bg: ColorJson::Default,
                        underline_color: ColorJson::Default,
                        flags: 0,
                        underline_style: UnderlineStyleJson::Single,
                    },
                    link: None,
                })
                .collect(),
            sized_cells: Vec::new(),
            soft_wrap_continued: false,
        }
    }

    /// A message's rows share one table, so a row must recode the same
    /// whatever pens the rows before it interned.
    #[test]
    fn rows_sharing_a_table_recode_as_rows_with_their_own() {
        let rows = [indexed_row(&[1, 2, 3]), indexed_row(&[3, 4, 1])];
        let mut shared = StyleTable::new();
        for row in &rows {
            let payload = to_payload(row, &mut shared).expect("the row encodes");
            assert_eq!(
                payload,
                to_payload(row, &mut StyleTable::new()).expect("the row encodes"),
            );
            assert_eq!(
                from_payload(&payload, &mut shared).expect("the row decodes"),
                from_payload(&payload, &mut StyleTable::new()).expect("the row decodes"),
            );
        }
    }

    /// Three distinct pens, both color forms, a wide character, a
    /// hyperlink and a sizing: every branch of the DTO tree in one
    /// row, compared as cells so a spelling change cannot hide.
    #[test]
    fn a_multi_pen_row_survives_wire_json_wire() {
        let mut styles = StyleTable::new();
        let bold_red = styles.intern(Attributes {
            fg: Color::Indexed(1),
            bg: Color::Default,
            flags: AttrFlags::BOLD,
            ..Attributes::default()
        });
        let rgb = styles.intern(Attributes {
            fg: Color::Rgb(0xAB, 0xCD, 0xEF),
            bg: Color::Indexed(4),
            underline_color: Color::Rgb(1, 2, 3),
            flags: AttrFlags::REVERSE | AttrFlags::UNDERLINE,
            underline_style: UnderlineStyle::Curly,
        });
        let cells = vec![
            Cell::default(),
            Cell {
                grapheme: Grapheme::Ascii(b'A'),
                style: bold_red,
                link: NonZeroU16::new(7),
                sizing: None,
            },
            Cell {
                grapheme: Grapheme::Char('あ'),
                style: rgb,
                link: None,
                sizing: None,
            },
            Cell {
                grapheme: Grapheme::Spacer,
                style: rgb,
                link: None,
                sizing: None,
            },
            Cell {
                grapheme: Grapheme::Cluster(NonZeroU32::new(3).expect("a cluster handle")),
                style: rgb,
                link: None,
                sizing: None,
            },
        ];
        let sized = vec![(
            1,
            Sizing::new(2, 0, 1, 3, VAlign::Bottom, HAlign::Center).expect("a legal sizing"),
        )];
        let payload = RowPayload(
            wire::encode_row(
                RowEncode {
                    cells: &cells,
                    pad_to: cells.len(),
                    sized_cells: &sized,
                    soft_wrap_continued: true,
                },
                &styles,
            )
            .expect("the row encodes"),
        );

        let json = from_payload(&payload, &mut StyleTable::new()).expect("the row recodes to JSON");
        assert_eq!(
            to_payload(&json, &mut StyleTable::new()).expect("the row recodes back"),
            payload
        );

        let mut back_styles = StyleTable::new();
        let back = wire::decode_row(
            &to_payload(&json, &mut StyleTable::new())
                .expect("the row recodes back")
                .0,
            &mut back_styles,
        )
        .expect("the re-encoded row decodes");
        assert_eq!(back.cells, cells);
        assert_eq!(back.sized_cells, sized);
        assert!(back.soft_wrap_continued);
    }

    /// A control byte would mint a cell no encoder produces, and the
    /// row codec refuses it on the way back in, so the DTO must refuse
    /// it here rather than hand a consumer an unusable payload.
    #[test]
    fn a_non_printable_ascii_grapheme_is_refused() {
        for byte in [0x00, 0x0A, 0x1B, 0x7F, 0xFF] {
            let RowCellsJson::Rle { attr_runs, .. } = rle(0);
            let cells = RowCellsJson::Rle {
                graphemes: vec![GraphemeJson::Ascii { byte }],
                attr_runs,
                sized_cells: Vec::new(),
                soft_wrap_continued: false,
            };
            assert!(
                matches!(
                    to_payload(&cells, &mut StyleTable::new()),
                    Err(JsonError::Field { field: "byte", .. })
                ),
                "byte {byte:#04x} must not cross",
            );
        }
    }

    #[test]
    fn the_printable_ascii_window_crosses() {
        for byte in [ASCII_MIN, b'A', ASCII_MAX] {
            let RowCellsJson::Rle { attr_runs, .. } = rle(0);
            let cells = RowCellsJson::Rle {
                graphemes: vec![GraphemeJson::Ascii { byte }],
                attr_runs,
                sized_cells: Vec::new(),
                soft_wrap_continued: false,
            };
            assert!(
                to_payload(&cells, &mut StyleTable::new()).is_ok(),
                "byte {byte:#04x} must cross"
            );
        }
    }

    /// The row codec refuses a sized cell outside the row, so a column
    /// the graphemes do not reach cannot be encoded here either.
    #[test]
    fn a_sized_cell_outside_the_row_is_refused() {
        let RowCellsJson::Rle {
            graphemes,
            attr_runs,
            ..
        } = rle(0);
        let cells = RowCellsJson::Rle {
            graphemes,
            attr_runs,
            sized_cells: vec![SizedCellJson {
                col: 1,
                sizing: SizingJson {
                    scale: 1,
                    cell_width: 0,
                    frac_num: 0,
                    frac_den: 0,
                    valign: VAlignJson::Top,
                    halign: HAlignJson::Left,
                },
            }],
            soft_wrap_continued: false,
        };
        assert!(matches!(
            to_payload(&cells, &mut StyleTable::new()),
            Err(JsonError::Field { field: "col", .. })
        ));
    }

    /// A bit this build does not define must not decode as the flags
    /// with that bit dropped.
    #[test]
    fn a_reserved_sgr_flag_bit_is_refused() {
        let reserved = ATTR_FLAGS_MAX + 1;
        assert!(matches!(
            to_payload(&rle(reserved), &mut StyleTable::new()),
            Err(JsonError::Field { field: "flags", .. })
        ));
    }

    #[test]
    fn every_defined_sgr_flag_bit_is_accepted() {
        assert!(to_payload(&rle(ATTR_FLAGS_MAX), &mut StyleTable::new()).is_ok());
    }

    #[test]
    fn a_zero_length_attribute_run_is_refused() {
        let RowCellsJson::Rle { mut attr_runs, .. } = rle(0);
        attr_runs[0].len = 0;
        let cells = RowCellsJson::Rle {
            graphemes: vec![GraphemeJson::Ascii { byte: b'x' }],
            attr_runs,
            sized_cells: Vec::new(),
            soft_wrap_continued: false,
        };
        assert!(matches!(
            to_payload(&cells, &mut StyleTable::new()),
            Err(JsonError::Field { field: "len", .. })
        ));
    }

    /// The JSON row has no length prefixes, so this coverage check is
    /// the only guard between a mismatched recording and a
    /// mis-attributed row.
    #[test]
    fn runs_that_outrun_the_graphemes_are_refused() {
        let RowCellsJson::Rle { mut attr_runs, .. } = rle(0);
        attr_runs[0].len = 4;
        let cells = RowCellsJson::Rle {
            graphemes: vec![GraphemeJson::Empty; 2],
            attr_runs,
            sized_cells: Vec::new(),
            soft_wrap_continued: false,
        };
        assert!(matches!(
            to_payload(&cells, &mut StyleTable::new()),
            Err(JsonError::Field {
                field: "attr_runs",
                ..
            })
        ));
    }

    #[test]
    fn graphemes_left_over_by_the_runs_are_refused() {
        let RowCellsJson::Rle { attr_runs, .. } = rle(0);
        let cells = RowCellsJson::Rle {
            graphemes: vec![GraphemeJson::Empty; 3],
            attr_runs,
            sized_cells: Vec::new(),
            soft_wrap_continued: false,
        };
        assert!(matches!(
            to_payload(&cells, &mut StyleTable::new()),
            Err(JsonError::Field {
                field: "graphemes",
                ..
            })
        ));
    }

    #[test]
    fn a_zero_cluster_handle_is_refused() {
        let RowCellsJson::Rle { attr_runs, .. } = rle(0);
        let cells = RowCellsJson::Rle {
            graphemes: vec![GraphemeJson::Cluster { id: 0 }],
            attr_runs,
            sized_cells: Vec::new(),
            soft_wrap_continued: false,
        };
        assert!(matches!(
            to_payload(&cells, &mut StyleTable::new()),
            Err(JsonError::Field { field: "id", .. })
        ));
    }

    /// The checked constructor owns the OSC 66 ranges; the DTO must
    /// not slip past it.
    #[test]
    fn an_out_of_range_sizing_is_refused() {
        let RowCellsJson::Rle {
            graphemes,
            attr_runs,
            ..
        } = rle(0);
        let cells = RowCellsJson::Rle {
            graphemes,
            attr_runs,
            sized_cells: vec![SizedCellJson {
                col: 0,
                sizing: SizingJson {
                    scale: 0,
                    cell_width: 0,
                    frac_num: 0,
                    frac_den: 0,
                    valign: VAlignJson::Top,
                    halign: HAlignJson::Left,
                },
            }],
            soft_wrap_continued: false,
        };
        assert!(matches!(
            to_payload(&cells, &mut StyleTable::new()),
            Err(JsonError::Field {
                field: "sizing",
                ..
            })
        ));
    }

    #[test]
    fn a_zero_hyperlink_handle_is_refused() {
        let RowCellsJson::Rle {
            graphemes,
            mut attr_runs,
            ..
        } = rle(0);
        attr_runs[0].link = Some(0);
        let cells = RowCellsJson::Rle {
            graphemes,
            attr_runs,
            sized_cells: Vec::new(),
            soft_wrap_continued: false,
        };
        assert!(matches!(
            to_payload(&cells, &mut StyleTable::new()),
            Err(JsonError::Field { field: "link", .. })
        ));
    }
}

//! Property tests for attribute-run-length row codec (`docs/reference/row-codec.md`).
//! Proptest rather than Kani: codec allocates and interns through `HashMap`
//! (`docs/explanation/testing.md` "Deliberately not verified with Kani").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::num::{NonZeroU16, NonZeroU32};

use felis_grid::{
    AttrFlags, Attributes, Cell, Color, DecodedRow, Grapheme, HAlign, RowEncode, Sizing, StyleId,
    StyleTable, UnderlineStyle, VAlign, decode_row, encode_row,
};
use proptest::prelude::*;

const WIDE_CHARS: &[char] = &['あ', '漢', '한', '🙂'];

const NARROW_CHARS: &[char] = &['é', 'ß', 'λ', '→'];

fn color() -> impl Strategy<Value = Color> {
    prop_oneof![
        Just(Color::Default),
        any::<u8>().prop_map(Color::Indexed),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b)| Color::Rgb(r, g, b)),
    ]
}

fn underline_style() -> impl Strategy<Value = UnderlineStyle> {
    prop::sample::select(vec![
        UnderlineStyle::Single,
        UnderlineStyle::Double,
        UnderlineStyle::Curly,
        UnderlineStyle::Dotted,
        UnderlineStyle::Dashed,
    ])
}

fn attributes() -> impl Strategy<Value = Attributes> {
    (color(), color(), color(), any::<u16>(), underline_style()).prop_map(
        |(fg, bg, underline_color, bits, underline_style)| Attributes {
            fg,
            bg,
            underline_color,
            flags: AttrFlags::from_bits_truncate(bits),
            underline_style,
        },
    )
}

#[derive(Debug, Clone)]
enum CellShape {
    Single(Grapheme),
    Wide(char),
}

fn cell_shape() -> impl Strategy<Value = CellShape> {
    prop_oneof![
        Just(CellShape::Single(Grapheme::Empty)),
        (0x20u8..=0x7E).prop_map(|b| CellShape::Single(Grapheme::Ascii(b))),
        prop::sample::select(NARROW_CHARS).prop_map(|c| CellShape::Single(Grapheme::Char(c))),
        (1u32..64).prop_map(|h| CellShape::Single(Grapheme::Cluster(NonZeroU32::new(h).unwrap()))),
        Just(CellShape::Single(Grapheme::SizedSpacer)),
        prop::sample::select(WIDE_CHARS).prop_map(CellShape::Wide),
    ]
}

fn decorated_shape() -> impl Strategy<Value = (CellShape, usize, Option<NonZeroU16>)> {
    (
        cell_shape(),
        0usize..8,
        prop_oneof![
            3 => Just(None::<NonZeroU16>),
            1 => (1u16..8).prop_map(NonZeroU16::new),
        ],
    )
}

fn sizing() -> impl Strategy<Value = Sizing> {
    let valign = prop::sample::select(vec![VAlign::Top, VAlign::Bottom, VAlign::Center]);
    let halign = prop::sample::select(vec![HAlign::Left, HAlign::Right, HAlign::Center]);
    (1u8..=7, 0u8..=7, 0u8..=15, 0u8..=15, valign, halign).prop_filter_map(
        "Sizing rejects frac_num >= frac_den when frac_den != 0",
        |(scale, cell_width, frac_num, frac_den, valign, halign)| {
            Sizing::new(scale, cell_width, frac_num, frac_den, valign, halign)
        },
    )
}

fn sized_cells() -> impl Strategy<Value = Vec<(u16, Sizing)>> {
    proptest::collection::vec((0u16..64, sizing()), 0..4)
}

fn build_row(
    pens: &[Attributes],
    shapes: &[(CellShape, usize, Option<NonZeroU16>)],
) -> (Vec<Cell>, StyleTable) {
    let mut styles = StyleTable::new();
    let mut cells = Vec::new();
    for (shape, pen_idx, link) in shapes {
        let style = styles.intern(pens[pen_idx % pens.len()]);
        let mut push = |grapheme| {
            cells.push(Cell {
                grapheme,
                style,
                link: *link,
                sizing: None,
            });
        };
        match shape {
            CellShape::Single(grapheme) => push(*grapheme),
            CellShape::Wide(c) => {
                push(Grapheme::Char(*c));
                push(Grapheme::Spacer);
            }
        }
    }
    (cells, styles)
}

const fn plain(cells: &[Cell]) -> RowEncode<'_> {
    RowEncode {
        cells,
        pad_to: cells.len(),
        sized_cells: &[],
        soft_wrap_continued: false,
    }
}

/// Pens are compared by resolution, not by id: ids are table-local and
/// the client's registry mints its own.
fn check_round_trip(
    cells: &[Cell],
    daemon_styles: &StyleTable,
    client_styles: &mut StyleTable,
    pad_to: usize,
    sized: &[(u16, Sizing)],
    soft_wrap_continued: bool,
) -> Result<DecodedRow, TestCaseError> {
    let width = pad_to.max(cells.len());
    // The band names columns of the row it rides, and the decoder
    // refuses one that points past them, so the property drives only
    // bands a daemon could emit.
    let sized: Vec<(u16, Sizing)> = sized
        .iter()
        .copied()
        .filter(|(col, _)| usize::from(*col) < width)
        .collect();
    let bytes = encode_row(
        RowEncode {
            cells,
            pad_to,
            sized_cells: &sized,
            soft_wrap_continued,
        },
        daemon_styles,
    )
    .unwrap();
    let back = decode_row(&bytes, client_styles).unwrap();

    prop_assert_eq!(back.cells.len(), width, "column count");
    for (col, decoded) in back.cells.iter().enumerate() {
        let original = cells.get(col).copied().unwrap_or(Cell::BLANK);
        prop_assert_eq!(decoded.grapheme, original.grapheme, "col {} grapheme", col);
        prop_assert_eq!(
            client_styles.resolve(decoded.style),
            daemon_styles.resolve(original.style),
            "col {} pen",
            col
        );
        prop_assert_eq!(decoded.link, original.link, "col {} link", col);
        // OSC 66 sizing rides `sized_cells`, never the cell stream.
        prop_assert_eq!(decoded.sizing, None, "col {} sizing", col);
    }
    prop_assert_eq!(&back.sized_cells, &sized, "sized-cell band");
    prop_assert_eq!(
        back.soft_wrap_continued,
        soft_wrap_continued,
        "soft-wrap bit"
    );
    Ok(back)
}

proptest! {
    #[test]
    fn a_row_round_trips(
        pens in proptest::collection::vec(attributes(), 1..=4),
        shapes in proptest::collection::vec(decorated_shape(), 0..24),
        pad_extra in 0usize..8,
        sized in sized_cells(),
        soft_wrap_continued in any::<bool>(),
    ) {
        let (cells, daemon_styles) = build_row(&pens, &shapes);
        let pad_to = cells.len() + pad_extra;
        check_round_trip(
            &cells,
            &daemon_styles,
            &mut StyleTable::new(),
            pad_to,
            &sized,
            soft_wrap_continued,
        )?;
    }

    #[test]
    fn pad_to_below_cell_count_ships_every_cell(
        pens in proptest::collection::vec(attributes(), 1..=4),
        shapes in proptest::collection::vec(decorated_shape(), 1..24),
        clip in 0usize..24,
    ) {
        let (cells, daemon_styles) = build_row(&pens, &shapes);
        let pad_to = cells.len().saturating_sub(clip);
        check_round_trip(
            &cells,
            &daemon_styles,
            &mut StyleTable::new(),
            pad_to,
            &[],
            false,
        )?;
    }

    #[test]
    fn pens_resolve_against_an_independently_populated_client_table(
        pens in proptest::collection::vec(attributes(), 1..=4),
        client_seed in proptest::collection::vec(attributes(), 1..=4),
        shapes in proptest::collection::vec(decorated_shape(), 0..24),
    ) {
        let (cells, daemon_styles) = build_row(&pens, &shapes);
        let mut client_styles = StyleTable::new();
        for pen in &client_seed {
            client_styles.intern(*pen);
        }
        check_round_trip(
            &cells,
            &daemon_styles,
            &mut client_styles,
            cells.len(),
            &[],
            false,
        )?;
    }

    #[test]
    fn a_shared_client_table_decodes_successive_rows_independently(
        pens in proptest::collection::vec(attributes(), 1..=4),
        rows in proptest::collection::vec(
            proptest::collection::vec(decorated_shape(), 0..12),
            1..4,
        ),
    ) {
        let mut client_styles = StyleTable::new();
        for shapes in &rows {
            let (cells, daemon_styles) = build_row(&pens, shapes);
            check_round_trip(
                &cells,
                &daemon_styles,
                &mut client_styles,
                cells.len(),
                &[],
                false,
            )?;
        }
        prop_assert!(client_styles.len() <= pens.len() + 1);
    }

    #[test]
    fn uniform_rows_encode_smaller_than_alternating_ones(
        pen in attributes(),
        cols in 8usize..64,
    ) {
        let mut styles = StyleTable::new();
        let uniform_id = styles.intern(pen);
        let uniform = vec![
            Cell { grapheme: Grapheme::Ascii(b'x'), style: uniform_id, link: None, sizing: None };
            cols
        ];
        let alternating: Vec<Cell> = (0..cols)
            .map(|i| {
                let style = styles.intern(Attributes {
                    fg: Color::Rgb(u8::try_from(i % 256).unwrap(), 0, 0),
                    ..pen
                });
                Cell { grapheme: Grapheme::Ascii(b'x'), style, link: None, sizing: None }
            })
            .collect();

        let uniform_bytes = encode_row(plain(&uniform), &styles).unwrap();
        let alternating_bytes = encode_row(plain(&alternating), &styles).unwrap();
        prop_assert!(
            uniform_bytes.len() < alternating_bytes.len(),
            "uniform row ({} B) did not beat the per-column-pen row ({} B) over {} columns",
            uniform_bytes.len(),
            alternating_bytes.len(),
            cols
        );
        prop_assert_eq!(
            decode_row(&uniform_bytes, &mut StyleTable::new())
                .unwrap()
                .cells
                .len(),
            cols
        );
    }

    #[test]
    fn truncated_payloads_never_decode(
        pens in proptest::collection::vec(attributes(), 1..=4),
        shapes in proptest::collection::vec(decorated_shape(), 1..24),
        cut in 1usize..64,
    ) {
        let (cells, daemon_styles) = build_row(&pens, &shapes);
        let bytes = encode_row(
            RowEncode {
                cells: &cells,
                pad_to: cells.len(),
                sized_cells: &[],
                soft_wrap_continued: false,
            },
            &daemon_styles,
        )
        .unwrap();
        let keep = bytes.len().saturating_sub(cut.min(bytes.len() - 1));
        prop_assert!(
            decode_row(&bytes[..keep], &mut StyleTable::new()).is_err(),
            "truncation to {} of {} B decoded",
            keep,
            bytes.len()
        );
    }
}

/// The default pen is id 0 in every registry, so padding decodes to
/// `Cell::BLANK` outright.
#[test]
fn blank_padding_decodes_to_blank_cells() {
    let bytes = encode_row(
        RowEncode {
            cells: &[],
            pad_to: 8,
            sized_cells: &[],
            soft_wrap_continued: false,
        },
        &StyleTable::new(),
    )
    .unwrap();
    let back = decode_row(&bytes, &mut StyleTable::new()).unwrap();
    assert_eq!(back.cells, vec![Cell::BLANK; 8]);
    assert_eq!(back.cells[0].style, StyleId::DEFAULT);
}

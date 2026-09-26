//! The felis-json v1 row DTO admits nothing the row codec refuses: a
//! replay that gets a frame past `json_v1::decode` must not be able to
//! hand the daemon bytes that kill the consuming client's connection.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "json")]

use felis_grid::json_v1::{
    self, ASCII_MAX, ASCII_MIN, AttrRunJson, AttributesJson, ColorJson, GraphemeJson, HAlignJson,
    RowCellsJson, SizedCellJson, SizingJson, UnderlineStyleJson, VAlignJson,
};
use felis_grid::{StyleTable, decode_row};
use felis_protocol::messages::GridMsg;
use felis_protocol::{MessageKind, codec};
use proptest::prelude::*;
use serde_json::{Value, json};

fn color() -> impl Strategy<Value = ColorJson> {
    prop_oneof![
        Just(ColorJson::Default),
        any::<u8>().prop_map(|index| ColorJson::Indexed { index }),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b)| ColorJson::Rgb { r, g, b }),
    ]
}

fn underline_style() -> impl Strategy<Value = UnderlineStyleJson> {
    prop::sample::select(vec![
        UnderlineStyleJson::Single,
        UnderlineStyleJson::Double,
        UnderlineStyleJson::Curly,
        UnderlineStyleJson::Dotted,
        UnderlineStyleJson::Dashed,
    ])
}

/// `flags` spans the whole `u16`, so reserved bits are generated as
/// often as defined ones.
fn attributes() -> impl Strategy<Value = AttributesJson> {
    (color(), color(), color(), any::<u16>(), underline_style()).prop_map(
        |(fg, bg, underline_color, flags, underline_style)| AttributesJson {
            fg,
            bg,
            underline_color,
            flags,
            underline_style,
        },
    )
}

/// The ASCII arm spans every byte, not the printable window, so a
/// control byte is a generated case rather than a corner one.
fn grapheme() -> impl Strategy<Value = GraphemeJson> {
    prop_oneof![
        Just(GraphemeJson::Empty),
        any::<u8>().prop_map(|byte| GraphemeJson::Ascii { byte }),
        prop::sample::select(vec!['é', 'あ', '🙂', '\u{1}'])
            .prop_map(|ch| GraphemeJson::Char { ch }),
        (0u32..8).prop_map(|id| GraphemeJson::Cluster { id }),
        Just(GraphemeJson::Spacer),
        Just(GraphemeJson::SizedSpacer),
    ]
}

fn sizing() -> impl Strategy<Value = SizingJson> {
    (0u8..9, 0u8..9, 0u8..17, 0u8..17).prop_map(|(scale, cell_width, frac_num, frac_den)| {
        SizingJson {
            scale,
            cell_width,
            frac_num,
            frac_den,
            valign: VAlignJson::Top,
            halign: HAlignJson::Left,
        }
    })
}

fn attr_run() -> impl Strategy<Value = AttrRunJson> {
    (0u16..6, attributes(), prop::option::of(0u16..4)).prop_map(|(len, attrs, link)| AttrRunJson {
        len,
        attrs,
        link,
    })
}

fn row_cells() -> impl Strategy<Value = RowCellsJson> {
    (
        prop::collection::vec(grapheme(), 0..8),
        prop::collection::vec(attr_run(), 0..4),
        prop::collection::vec((0u16..10, sizing()), 0..3),
        any::<bool>(),
    )
        .prop_map(
            |(graphemes, attr_runs, sized, soft_wrap_continued)| RowCellsJson::Rle {
                graphemes,
                attr_runs,
                sized_cells: sized
                    .into_iter()
                    .map(|(col, sizing)| SizedCellJson { col, sizing })
                    .collect(),
                soft_wrap_continued,
            },
        )
}

fn envelope(cells: &RowCellsJson) -> Value {
    json!({
        "felis_json": 1,
        "kind": "grid",
        "msg": {
            "type": "row_delta",
            "rows": [{ "row": 0, "cells": serde_json::to_value(cells).unwrap() }],
        },
    })
}

proptest! {
    /// Whatever the envelope accepts, the row codec accepts: the two
    /// admission rules are one rule.
    #[test]
    fn a_row_the_envelope_accepts_is_a_row_the_codec_accepts(cells in row_cells()) {
        let Ok((kind, body)) = json_v1::decode(&envelope(&cells)) else {
            return Ok(());
        };
        prop_assert_eq!(kind, MessageKind::Grid);
        let GridMsg::RowDelta { rows } = codec::decode::<GridMsg>(&body).unwrap() else {
            panic!("a row delta decodes as one");
        };
        for (_, payload) in rows {
            prop_assert!(
                decode_row(&payload.0, &mut StyleTable::new()).is_ok(),
                "the envelope admitted a row the codec refuses: {cells:?}",
            );
        }
    }

    /// The window is the row codec's, not a second one written here.
    #[test]
    fn only_printable_ascii_crosses(byte in any::<u8>()) {
        let cells = RowCellsJson::Rle {
            graphemes: vec![GraphemeJson::Ascii { byte }],
            attr_runs: vec![AttrRunJson {
                len: 1,
                attrs: AttributesJson {
                    fg: ColorJson::Default,
                    bg: ColorJson::Default,
                    underline_color: ColorJson::Default,
                    flags: 0,
                    underline_style: UnderlineStyleJson::Single,
                },
                link: None,
            }],
            sized_cells: Vec::new(),
            soft_wrap_continued: false,
        };
        prop_assert_eq!(
            json_v1::decode(&envelope(&cells)).is_ok(),
            (ASCII_MIN..=ASCII_MAX).contains(&byte),
        );
    }
}

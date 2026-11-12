//! Shared helpers for the crate's per-module unit tests.

use felis_vt::Parser;
use proptest::prelude::*;

use crate::{
    ApcBody, AttrFlags, Attributes, Color, Grapheme, Grid, PtyEffect, ScreenSwitch, ScrollOp,
    UnderlineStyle,
};

pub(crate) fn drive(parser: &mut Parser, grid: &mut Grid, bytes: &[u8]) {
    parser.advance(grid, bytes);
}

pub(crate) fn drive_new(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
    let mut grid = Grid::new(rows, cols);
    Parser::new().advance(&mut grid, bytes);
    grid
}

pub(crate) fn drive_take(rows: u16, cols: u16, bytes: &[u8]) -> (Grid, Vec<Vec<u8>>) {
    let mut grid = drive_new(rows, cols, bytes);
    let queued = responses(&mut grid);
    (grid, queued)
}

pub(crate) fn reply_text(grid: &mut Grid) -> Vec<String> {
    responses(grid)
        .into_iter()
        .map(|reply| String::from_utf8(reply).unwrap())
        .collect()
}

pub(crate) fn responses(grid: &mut Grid) -> Vec<Vec<u8>> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Response(bytes) => Some(bytes),
            _ => None,
        })
        .collect()
}

/// `None` for a cell that is not a cluster, so a match also pins the
/// cell kind.
pub(crate) fn cluster_at(grid: &Grid, row: u16, col: u16) -> Option<&str> {
    match grid.cell(row, col)?.grapheme {
        Grapheme::Cluster(id) => grid.cluster_str(id),
        _ => None,
    }
}

pub(crate) fn apc_bodies(grid: &mut Grid) -> Vec<ApcBody> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Apc(body) => Some(body),
            _ => None,
        })
        .collect()
}

pub(crate) fn scroll_ops(grid: &mut Grid) -> Vec<ScrollOp> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Scrolled { op, .. } => Some(op),
            _ => None,
        })
        .collect()
}

pub(crate) fn screen_switches(grid: &mut Grid) -> Vec<ScreenSwitch> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::ScreenSwitch(switch) => Some(switch),
            _ => None,
        })
        .collect()
}

pub(crate) fn arb_color() -> impl Strategy<Value = Color> {
    prop_oneof![
        Just(Color::Default),
        any::<u8>().prop_map(Color::Indexed),
        any::<(u8, u8, u8)>().prop_map(|(r, g, b)| Color::Rgb(r, g, b)),
    ]
}

pub(crate) fn arb_attrs() -> impl Strategy<Value = Attributes> {
    (
        arb_color(),
        arb_color(),
        arb_color(),
        any::<u16>(),
        prop::sample::select(vec![
            UnderlineStyle::Single,
            UnderlineStyle::Double,
            UnderlineStyle::Curly,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ]),
    )
        .prop_map(
            |(fg, bg, underline_color, bits, underline_style)| Attributes {
                fg,
                bg,
                underline_color,
                flags: AttrFlags::from_bits_truncate(bits),
                underline_style,
            },
        )
}

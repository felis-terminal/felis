//! Screen-scoped operations (ED, DECSED, DECALN, DECCOLM's clear, the
//! rectangle edits) act on the live viewport only: history shares the
//! viewport's ring, and none of these may reach it.

use felis_grid::{Attributes, Cell, Color, Grapheme, Grid};
use felis_vt::Parser;
use proptest::prelude::*;

use crate::common::drive_with;

type HistoryRow = (Vec<(Grapheme, Attributes, bool)>, bool);

fn history(g: &Grid) -> Vec<HistoryRow> {
    let sb = g.scrollback();
    (0..sb.len())
        .map(|i| {
            let cells = sb
                .row(i)
                .unwrap()
                .iter()
                .map(|c| (c.grapheme, *g.style(c.style), c.link.is_some()))
                .collect();
            (cells, sb.soft_wrap_continued(i).unwrap())
        })
        .collect()
}

/// Styled, soft-wrapped lines scrolled well into history, so the ring's
/// viewport starts past physical row 0.
fn grid_with_history() -> (Parser, Grid) {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    for i in 0..9 {
        let line = format!("\x1b[3{}mline{i}\x1b[m-and-a-wrapped-tail\r\n", i % 8);
        drive_with(&mut p, &mut g, line.as_bytes());
    }
    drive_with(&mut p, &mut g, b"live");
    assert!(g.scrollback().len() > 4, "the fixture must build history");
    (p, g)
}

fn viewport_cells(g: &Grid) -> impl Iterator<Item = (u16, u16, Cell)> + '_ {
    (0..g.rows()).flat_map(move |r| (0..g.cols()).map(move |c| (r, c, *g.cell(r, c).unwrap())))
}

fn assert_no_viewport_wraps(g: &Grid) {
    for r in 0..g.rows() {
        assert!(!g.row_soft_wrap_continued(r), "row {r} kept its wrap bit");
    }
}

#[test]
fn colored_ed_2_blanks_the_viewport_with_the_pen_background_and_keeps_history() {
    let (mut p, mut g) = grid_with_history();
    let before = history(&g);
    drive_with(&mut p, &mut g, b"\x1b[44m\x1b[2J");
    assert_eq!(history(&g), before);
    for (r, c, cell) in viewport_cells(&g) {
        assert_eq!(cell.grapheme, Grapheme::Empty, "({r},{c})");
        assert_eq!(g.style(cell.style).bg, Color::Indexed(4), "({r},{c})");
    }
    assert_no_viewport_wraps(&g);
}

#[test]
fn default_ed_2_blanks_the_viewport_and_keeps_history() {
    let (mut p, mut g) = grid_with_history();
    let before = history(&g);
    drive_with(&mut p, &mut g, b"\x1b[2J");
    assert_eq!(history(&g), before);
    for (r, c, cell) in viewport_cells(&g) {
        assert_eq!(cell, Cell::default(), "({r},{c})");
    }
    assert_no_viewport_wraps(&g);
}

#[test]
fn decaln_fills_the_viewport_with_e_and_keeps_history() {
    let (mut p, mut g) = grid_with_history();
    let before = history(&g);
    drive_with(&mut p, &mut g, b"\x1b#8");
    assert_eq!(history(&g), before);
    for (r, c, cell) in viewport_cells(&g) {
        assert_eq!(cell.grapheme, Grapheme::Ascii(b'E'), "({r},{c})");
        assert_eq!(*g.style(cell.style), Attributes::default(), "({r},{c})");
    }
    assert_no_viewport_wraps(&g);
}

#[test]
fn deccolm_clears_the_viewport_and_keeps_history() {
    let (mut p, mut g) = grid_with_history();
    let before = history(&g);
    drive_with(&mut p, &mut g, b"\x1b[?40h\x1b[?3h");
    assert_eq!(history(&g), before);
    for (r, c, cell) in viewport_cells(&g) {
        assert_eq!(cell, Cell::default(), "({r},{c})");
    }
    assert_no_viewport_wraps(&g);
    assert_eq!((g.cursor().row, g.cursor().col), (0, 0));
}

#[test]
fn decsed_2_erases_the_unprotected_viewport_and_keeps_history() {
    let (mut p, mut g) = grid_with_history();
    // A protected `P` at the top-left, then the viewport otherwise full.
    drive_with(&mut p, &mut g, b"\x1b[H\x1b[1\"qP\x1b[0\"qxyz");
    let before = history(&g);
    drive_with(&mut p, &mut g, b"\x1b[?2J");
    assert_eq!(history(&g), before);
    for (r, c, cell) in viewport_cells(&g) {
        let expected = if (r, c) == (0, 0) {
            Grapheme::Ascii(b'P')
        } else {
            Grapheme::Empty
        };
        assert_eq!(cell.grapheme, expected, "({r},{c})");
    }
}

#[test]
fn decsed_inside_an_active_scroll_band_erases_the_viewport_rows_it_names() {
    let (mut p, mut g) = grid_with_history();
    // A region scroll with history present rotates an interior band.
    drive_with(
        &mut p,
        &mut g,
        b"\x1b[H\x1b[2J\x1b[1;1Hr0\x1b[2;1Hr1\x1b[3;1Hr2\x1b[4;1Hr3\x1b[2;3r\x1b[3;1H\n\x1b[r",
    );
    let before = history(&g);
    let r0 = g.cell(0, 1).unwrap().grapheme;
    drive_with(&mut p, &mut g, b"\x1b[2;1H\x1b[?0J");
    assert_eq!(history(&g), before);
    assert_eq!(
        g.cell(0, 1).unwrap().grapheme,
        r0,
        "rows above the cursor survive"
    );
    for (r, c, cell) in viewport_cells(&g).filter(|&(r, _, _)| r >= 1) {
        assert_eq!(cell.grapheme, Grapheme::Empty, "({r},{c})");
    }
}

const SCREEN_SCOPED: &[&[u8]] = &[
    b"\x1b[2J",
    b"\x1b[44m\x1b[2J",
    b"\x1b[J",
    b"\x1b[45m\x1b[1J",
    b"\x1b[?J",
    b"\x1b[?1J",
    b"\x1b[46m\x1b[?2J",
    b"\x1b#8",
    b"\x1b[?40h\x1b[?3h",
    b"\x1b[41m\x1b[K",
    b"\x1b[2K",
    b"\x1b[42m\x1b[3X",
    b"\x1b[1;1;2;4$z",
    b"\x1b[65;1;1;3;5$x",
    b"\x1b[1;1;4;9${",
];

fn setup_chunk() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        "[a-z]{1,25}".prop_map(String::into_bytes),
        Just(b"\r\n".to_vec()),
        Just(b"\x1bM".to_vec()),
        Just(b"\x1b[L".to_vec()),
        Just(b"\x1b[M".to_vec()),
        Just(b"\x1b[2;3r".to_vec()),
        Just(b"\x1b[r".to_vec()),
        Just(b"\x1b[1\"q".to_vec()),
        Just(b"\x1b[0\"q".to_vec()),
        (30u8..38).prop_map(|c| format!("\x1b[{c}m").into_bytes()),
        (40u8..48).prop_map(|c| format!("\x1b[{c}m").into_bytes()),
        Just(b"\x1b[m".to_vec()),
        (1u8..5, 1u8..10).prop_map(|(r, c)| format!("\x1b[{r};{c}H").into_bytes()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn screen_scoped_operations_never_change_a_history_row(
        rounds in prop::collection::vec(
            (prop::collection::vec(setup_chunk(), 0..12), 0..SCREEN_SCOPED.len()),
            1..8,
        ),
    ) {
        let (mut p, mut g) = grid_with_history();
        for (setup, op) in rounds {
            for chunk in &setup {
                drive_with(&mut p, &mut g, chunk);
            }
            let before = history(&g);
            drive_with(&mut p, &mut g, SCREEN_SCOPED[op]);
            prop_assert_eq!(history(&g), before, "op {:?}", String::from_utf8_lossy(SCREEN_SCOPED[op]));
        }
    }
}

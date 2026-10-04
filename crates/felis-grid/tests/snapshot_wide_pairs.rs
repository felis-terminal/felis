//! Wide-pair snapshots: writing over, erasing, or repeating one half of a
//! two-cell glyph, for both a single wide scalar and a multi-codepoint
//! cluster. Each row renders cell by cell with its role, so an orphaned
//! half (a `Spacer` with no owner, an owner with no `Spacer`) is visible.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use felis_grid::{Grapheme, Grid, row_text_trim};
use unicode_width::UnicodeWidthChar;

mod common;
use common::{drive, write_cursor};

/// `_` is a `Spacer`, `·` an `Empty`; a zero-width scalar inside a
/// cluster is spelled `\u{…}` so a dropped selector shows.
fn render(grid: &Grid) -> String {
    let mut out = String::new();
    write_cursor(&mut out, grid);
    for r in 0..grid.rows() {
        let row = grid.row_cells(r).unwrap();
        let cells: Vec<String> = row.iter().map(|c| cell_token(grid, c.grapheme)).collect();
        writeln!(out, "cells: {}", cells.join("|")).unwrap();
        writeln!(out, "text: {:?}", row_text_trim(row, grid.cluster_table())).unwrap();
    }
    out
}

fn cell_token(grid: &Grid, g: Grapheme) -> String {
    match g {
        Grapheme::Empty => "·".into(),
        Grapheme::Spacer => "_".into(),
        Grapheme::SizedSpacer => "#".into(),
        Grapheme::Ascii(b) => char::from(b).into(),
        Grapheme::Char(c) => spell(c),
        Grapheme::Cluster(id) => grid
            .cluster_str(id)
            .map_or_else(|| "?".into(), |s| s.chars().map(spell).collect()),
    }
}

fn spell(c: char) -> String {
    if c.width() == Some(0) || c == '\u{200D}' {
        format!("\\u{{{:x}}}", u32::from(c))
    } else {
        c.into()
    }
}

fn snap(cols: u16, bytes: &str) -> String {
    render(&drive(1, cols, bytes.as_bytes()))
}

#[test]
fn reprinting_a_vs16_emoji_over_itself_keeps_the_line() {
    insta::assert_snapshot!(snap(8, "❄️ x\r❄️ x"), @r#"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    cells: ❄\u{fe0f}|_| |x|·|·|·|·
    text: "❄\u{fe0f} x"
    "#);
}

#[test]
fn reprinting_a_flag_over_itself_keeps_the_line() {
    insta::assert_snapshot!(snap(8, "🇯🇵 x\r🇯🇵 x"), @r#"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    cells: 🇯🇵|_| |x|·|·|·|·
    text: "🇯🇵 x"
    "#);
}

#[test]
fn reprinting_a_keycap_over_itself_keeps_the_line() {
    insta::assert_snapshot!(snap(8, "1️⃣ x\r1️⃣ x"), @r#"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    cells: 1\u{fe0f}\u{20e3}|_| |x|·|·|·|·
    text: "1\u{fe0f}\u{20e3} x"
    "#);
}

#[test]
fn ascii_over_the_left_half_of_a_zwj_sequence_blanks_its_spacer() {
    insta::assert_snapshot!(snap(8, "👩‍💻z\rXY"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: X|Y|z|·|·|·|·|·
    text: "XYz"
    "#);
}

#[test]
fn a_narrow_scalar_over_the_left_half_of_a_cluster_blanks_its_spacer() {
    insta::assert_snapshot!(snap(8, "👩‍💻z\ré"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: é|·|z|·|·|·|·|·
    text: "é z"
    "#);
}

#[test]
fn a_wide_scalar_over_a_cluster_replaces_the_pair() {
    insta::assert_snapshot!(snap(8, "👩‍💻z\r字"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: 字|_|z|·|·|·|·|·
    text: "字z"
    "#);
}

#[test]
fn ech_on_the_left_half_of_a_cluster_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "👩‍💻z\r\x1b[X"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: ·|·|z|·|·|·|·|·
    text: "  z"
    "#);
}

#[test]
fn el_to_the_cursor_on_a_clusters_left_half_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "x❤️z\r\x1b[C\x1b[1K"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: ·|·|·|z|·|·|·|·
    text: "   z"
    "#);
}

#[test]
fn decsel_to_the_cursor_on_a_clusters_left_half_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "❤️z\r\x1b[?1K"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: ·|·|z|·|·|·|·|·
    text: "  z"
    "#);
}

#[test]
fn decsel_from_a_clusters_spacer_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "x❤️z\r\x1b[2C\x1b[?0K"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: x|·|·|·|·|·|·|·
    text: "x"
    "#);
}

#[test]
fn decsel_to_the_cursor_on_a_wide_scalars_left_half_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "字z\r\x1b[?1K"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: ·|·|z|·|·|·|·|·
    text: "  z"
    "#);
}

#[test]
fn decsel_leaves_a_protected_pair_whole() {
    insta::assert_snapshot!(snap(8, "\x1b[1\"q❤️\x1b[0\"qz\r\x1b[?1K"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: ❤\u{fe0f}|_|z|·|·|·|·|·
    text: "❤\u{fe0f}z"
    "#);
}

#[test]
fn decsed_to_the_cursor_on_a_clusters_left_half_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "❤️z\r\x1b[?1J"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: ·|·|z|·|·|·|·|·
    text: "  z"
    "#);
}

#[test]
fn decsed_from_a_clusters_spacer_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "x❤️z\r\x1b[2C\x1b[?0J"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: x|·|·|·|·|·|·|·
    text: "x"
    "#);
}

#[test]
fn decera_over_a_clusters_left_half_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "👩‍💻z\x1b[1;1;1;1$z"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: ·|·|z|·|·|·|·|·
    text: "  z"
    "#);
}

#[test]
fn decsera_over_a_clusters_left_half_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "👩‍💻z\x1b[1;1;1;1${"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: ·|·|z|·|·|·|·|·
    text: "  z"
    "#);
}

#[test]
fn decsera_leaves_a_protected_pair_whole() {
    insta::assert_snapshot!(snap(8, "\x1b[1\"q👩‍💻\x1b[0\"qz\x1b[1;1;1;1${"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: 👩\u{200d}💻|_|z|·|·|·|·|·
    text: "👩\u{200d}💻z"
    "#);
}

#[test]
fn decfra_over_a_clusters_left_half_blanks_its_spacer() {
    insta::assert_snapshot!(snap(8, "👩‍💻z\x1b[42;1;1;1;1$x"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: *|·|z|·|·|·|·|·
    text: "* z"
    "#);
}

#[test]
fn decera_whose_right_edge_splits_a_wide_scalar_erases_the_pair() {
    insta::assert_snapshot!(snap(8, "a字z\x1b[1;1;1;2$z"), @r#"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    cells: ·|·|·|z|·|·|·|·
    text: "   z"
    "#);
}

#[test]
fn decfra_whose_left_edge_splits_a_wide_scalar_blanks_its_owner() {
    insta::assert_snapshot!(snap(8, "a字z\x1b[42;1;3;1;4$x"), @r#"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    cells: a|·|*|*|·|·|·|·
    text: "a **"
    "#);
}

#[test]
fn rep_after_a_vs16_emoji_repeats_the_whole_cluster() {
    insta::assert_snapshot!(snap(8, "❤️\x1b[2b"), @r#"
    cursor: row=0 col=6 visible=1 pending_wrap=0
    cells: ❤\u{fe0f}|_|❤\u{fe0f}|_|❤\u{fe0f}|_|·|·
    text: "❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}"
    "#);
}

#[test]
fn rep_after_a_flag_repeats_the_whole_flag() {
    insta::assert_snapshot!(snap(8, "🇯🇵\x1b[2b"), @r#"
    cursor: row=0 col=6 visible=1 pending_wrap=0
    cells: 🇯🇵|_|🇯🇵|_|🇯🇵|_|·|·
    text: "🇯🇵🇯🇵🇯🇵"
    "#);
}

#[test]
fn an_osc_66_block_over_a_wide_scalars_left_half_blanks_its_spacer() {
    insta::assert_snapshot!(snap(8, "x字z\x1b[1;1H\x1b]66;w=2;A\x07"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: A|#|·|z|·|·|·|·
    text: "A z"
    "#);
}

#[test]
fn an_osc_66_block_over_a_clusters_left_half_blanks_its_spacer() {
    insta::assert_snapshot!(snap(8, "x👩‍💻z\x1b[1;1H\x1b]66;w=2;A\x07"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: A|#|·|z|·|·|·|·
    text: "A z"
    "#);
}

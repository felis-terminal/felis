//! Wide-pair snapshots: writing over, erasing, repeating or moving one half
//! of a two-cell glyph, for both a single wide scalar and a multi-codepoint
//! cluster. Each row renders cell by cell with its role, so an orphaned
//! half (a `Spacer` with no owner, an owner with no `Spacer`) is visible.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::Grid;

mod common;
use common::{drive, render_roles};

fn snap(cols: u16, bytes: &str) -> String {
    render_roles(&drive(1, cols, bytes.as_bytes()))
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

fn snap_rows(rows: u16, cols: u16, bytes: &str) -> String {
    render_roles(&drive(rows, cols, bytes.as_bytes()))
}

#[test]
fn dch_on_a_wide_scalars_left_half_erases_the_pair() {
    insta::assert_snapshot!(snap(6, "字b\r\x1b[P"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: ·|b|·|·|·|·
    text: " b"
    "#);
}

#[test]
fn dch_on_a_wide_scalars_right_half_erases_the_pair() {
    insta::assert_snapshot!(snap(6, "字b\r\x1b[C\x1b[P"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: ·|b|·|·|·|·
    text: " b"
    "#);
}

#[test]
fn ich_on_a_wide_scalars_right_half_erases_the_pair() {
    insta::assert_snapshot!(snap(6, "字b\r\x1b[C\x1b[@"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: ·|·|·|b|·|·
    text: "   b"
    "#);
}

#[test]
fn ich_that_pushes_half_a_pair_off_the_line_erases_it() {
    insta::assert_snapshot!(snap(8, "abcdef字\r\x1b[@"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: ·|a|b|c|d|e|f|·
    text: " abcdef"
    "#);
}

#[test]
fn an_irm_insert_on_a_wide_scalars_right_half_erases_the_pair() {
    insta::assert_snapshot!(snap(6, "字b\r\x1b[C\x1b[4hX"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: ·|X|·|b|·|·
    text: " X b"
    "#);
}

#[test]
fn an_irm_insert_that_pushes_half_a_pair_off_the_line_erases_it() {
    insta::assert_snapshot!(snap(8, "abcdef字\r\x1b[4hX"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: X|a|b|c|d|e|f|·
    text: "Xabcdef"
    "#);
}

#[test]
fn an_irm_wide_insert_that_pushes_half_a_pair_off_the_line_erases_it() {
    insta::assert_snapshot!(snap(8, "a字字字\r\x1b[4h字"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: 字|_|a|字|_|字|_|·
    text: "字a字字"
    "#);
}

#[test]
fn sr_that_pushes_half_a_pair_off_the_line_erases_it() {
    insta::assert_snapshot!(snap(6, "abcd字\x1b[1 A"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=1
    cells: ·|a|b|c|d|·
    text: " abcd"
    "#);
}

#[test]
fn sl_that_drops_half_a_pair_erases_it() {
    insta::assert_snapshot!(snap(6, "字bcd\x1b[1 @"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: ·|b|c|d|·|·
    text: " bcd"
    "#);
}

#[test]
fn ich_that_pushes_half_a_pair_past_the_right_margin_erases_it() {
    insta::assert_snapshot!(snap(6, "\x1b[?69h\x1b[1;4sab字\x1b[1;1H\x1b[@"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: ·|a|b|·|·|·
    text: " ab"
    "#);
}

#[test]
fn dch_inside_margins_that_a_pair_straddles_erases_it() {
    insta::assert_snapshot!(snap(6, "abc字x\x1b[?69h\x1b[1;4s\x1b[1;1H\x1b[P"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: b|c|·|·|·|x
    text: "bc   x"
    "#);
}

#[test]
fn decic_on_a_wide_scalars_right_half_erases_the_pair() {
    insta::assert_snapshot!(snap(6, "字b\r\x1b[C\x1b['}"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: ·|·|·|b|·|·
    text: "   b"
    "#);
}

#[test]
fn decdc_on_a_wide_scalars_right_half_erases_the_pair() {
    insta::assert_snapshot!(snap(6, "字b\r\x1b[C\x1b['~"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: ·|b|·|·|·|·
    text: " b"
    "#);
}

#[test]
fn decbi_at_a_left_margin_that_a_pair_straddles_erases_it() {
    insta::assert_snapshot!(snap(6, "a字bcd\x1b[?69h\x1b[3;6s\x1b[1;3H\x1b6"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: a|·|·|·|b|c
    text: "a   bc"
    "#);
}

#[test]
fn decfi_at_a_right_margin_that_a_pair_straddles_erases_it() {
    insta::assert_snapshot!(snap(6, "abc字\x1b[?69h\x1b[1;4s\x1b[1;4H\x1b9"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: b|c|·|·|·|·
    text: "bc"
    "#);
}

#[test]
fn su_inside_margins_that_pairs_straddle_erases_them() {
    insta::assert_snapshot!(snap_rows(2, 6, "abcd字\r\na字def\x1b[?69h\x1b[3;5s\x1b[S"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: a|b|·|d|e|·
    text: "ab de"
    cells: a|·|·|·|·|f
    text: "a    f"
    "#);
}

#[test]
fn deccra_whose_source_edges_cut_pairs_copies_no_half() {
    insta::assert_snapshot!(snap_rows(2, 8, "a字b字\x1b[1;3;1;5;1;2;1;1$v"), @r#"
    cursor: row=0 col=6 visible=1 pending_wrap=0
    cells: a|字|_|b|字|_|·|·
    text: "a字b字"
    cells: ·|b|·|·|·|·|·|·
    text: " b"
    "#);
}

#[test]
fn deccra_whose_destination_edges_cut_pairs_erases_them() {
    insta::assert_snapshot!(snap_rows(2, 6, "xy\r\n字字字\x1b[1;1;1;2;1;2;2;1$v"), @r#"
    cursor: row=1 col=5 visible=1 pending_wrap=1
    cells: x|y|·|·|·|·
    text: "xy"
    cells: ·|x|y|·|字|_
    text: " xy 字"
    "#);
}

#[test]
fn an_alt_screen_resize_that_cuts_a_pair_erases_it() {
    let mut grid = drive(1, 8, "\x1b[?1049habc字".as_bytes());
    grid.resize(1, 4);
    insta::assert_snapshot!(render_roles(&grid), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: a|b|c|·
    text: "abc"
    "#);
}

#[test]
fn a_vs16_at_the_right_margin_stays_narrow() {
    insta::assert_snapshot!(snap(8, "\x1b[?69h\x1b[1;7s\x1b[1;7H❤️"), @r#"
    cursor: row=0 col=6 visible=1 pending_wrap=1
    cells: ·|·|·|·|·|·|❤\u{fe0f}|·
    text: "      ❤\u{fe0f}"
    "#);
}

#[test]
fn reflow_keeps_a_refused_widen_in_one_cell() {
    let mut grid = drive(1, 8, "❤b\r\x1b[C\u{fe0f}".as_bytes());
    grid.reflow(1, 6);
    insta::assert_snapshot!(render_roles(&grid), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: ❤\u{fe0f}|b|·|·|·|·
    text: "❤\u{fe0f}b"
    "#);
}

#[test]
fn dch_on_a_sized_wide_scalars_spacer_erases_the_whole_block() {
    insta::assert_snapshot!(snap_rows(2, 8, "\x1b]66;s=2;字\x07z\x1b[1;2H\x1b[P"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: ·|·|·|z|·|·|·|·
    text: "   z"
    cells: ·|·|·|·|·|·|·|·
    text: ""
    "#);
}

#[test]
fn ich_clamped_to_the_line_from_a_right_half_erases_the_pair() {
    insta::assert_snapshot!(snap(6, "a字b\r\x1b[2C\x1b[9@"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: a|·|·|·|·|·
    text: "a"
    "#);
}

#[test]
fn dch_clamped_to_the_line_from_a_right_half_erases_the_pair() {
    insta::assert_snapshot!(snap(6, "a字b\r\x1b[2C\x1b[9P"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: a|·|·|·|·|·
    text: "a"
    "#);
}

#[test]
fn ich_left_of_the_margins_leaves_a_straddling_pair_alone() {
    insta::assert_snapshot!(snap(6, "a字bcd\x1b[?69h\x1b[3;5s\x1b[1;1H\x1b[@"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: a|字|_|b|c|d
    text: "a字bcd"
    "#);
}

#[test]
fn dch_right_of_the_margins_leaves_a_straddling_pair_alone() {
    insta::assert_snapshot!(snap(6, "abcd字\x1b[?69h\x1b[1;4s\x1b[1;6H\x1b[P"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: a|b|c|d|字|_
    text: "abcd字"
    "#);
}

#[test]
fn dch_keeps_a_pair_it_moves_next_to_the_vacated_tail() {
    insta::assert_snapshot!(snap(7, "\x1b[6G字\x1b[3G\x1b[P"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: ·|·|·|·|字|_|·
    text: "    字"
    "#);
}

#[test]
fn a_keycap_widens_over_a_recycled_rows_stale_tail() {
    insta::assert_snapshot!(snap_rows(3, 5, "\x1b[2;2H❤️\x1b[2Lx1️⃣"), @r#"
    cursor: row=1 col=3 visible=1 pending_wrap=0
    cells: ·|·|·|·|·
    text: ""
    cells: x|1\u{fe0f}\u{20e3}|_|·|·
    text: "x1\u{fe0f}\u{20e3}"
    cells: ·|·|·|·|·
    text: ""
    "#);
}

#[test]
fn decfi_shifts_no_recycled_tail_into_view() {
    insta::assert_snapshot!(snap_rows(3, 5, "\x1b[?1049h字\x1b[S\x1b[3;5H\x1b9"), @r#"
    cursor: row=2 col=4 visible=1 pending_wrap=0
    cells: ·|·|·|·|·
    text: ""
    cells: ·|·|·|·|·
    text: ""
    cells: ·|·|·|·|·
    text: ""
    "#);
}

#[test]
fn sr_shifts_no_recycled_tail_into_view() {
    insta::assert_snapshot!(snap_rows(3, 5, "\x1b[?1049h字\x1b[S\x1b[1 A"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: ·|·|·|·|·
    text: ""
    cells: ·|·|·|·|·
    text: ""
    cells: ·|·|·|·|·
    text: ""
    "#);
}

#[test]
fn decsed_exposes_no_recycled_tail() {
    insta::assert_snapshot!(snap_rows(3, 5, "\x1b[?1049h字\x1b[S\x1b[1;1H\x1b[?1J"), @r#"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    cells: ·|·|·|·|·
    text: ""
    cells: ·|·|·|·|·
    text: ""
    cells: ·|·|·|·|·
    text: ""
    "#);
}

#[test]
fn ich_that_cuts_one_sized_character_keeps_the_rest_of_its_run() {
    insta::assert_snapshot!(snap(8, "\x1b]66;w=2;AB\x07\x1b[1;2H\x1b[@"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: ·|·|·|B|#|·|·|·
    text: "   B"
    "#);
}

#[test]
fn deccra_copies_no_part_of_a_sized_block_its_source_cuts() {
    insta::assert_snapshot!(snap_rows(2, 8, "\x1b]66;s=2;A\x07\x1b[1;2;2;2;1;1;5$v"), @r#"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    cells: A|#|·|·|·|·|·|·
    text: "A"
    cells: #|#|·|·|·|·|·|·
    text: ""
    "#);
}

#[test]
fn deccra_over_part_of_a_sized_block_erases_the_whole_block() {
    insta::assert_snapshot!(snap_rows(2, 8, "xy\x1b[1;5H\x1b]66;s=2;A\x07\x1b[1;1;1;2;1;2;5$v"), @r#"
    cursor: row=0 col=6 visible=1 pending_wrap=0
    cells: x|y|·|·|·|·|·|·
    text: "xy"
    cells: ·|·|·|·|x|y|·|·
    text: "    xy"
    "#);
}

#[test]
fn a_saved_alt_screen_resize_that_cuts_a_sized_block_drops_its_sizing() {
    let mut parser = felis_vt::Parser::new();
    let mut grid = Grid::new(1, 8);
    parser.advance(&mut grid, b"\x1b[?47h\x1b[1;4H\x1b]66;w=2;A\x07\x1b[?47l");
    grid.resize(1, 4);
    parser.advance(&mut grid, b"\x1b[?47h");
    insta::assert_snapshot!(render_roles(&grid), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: ·|·|·|A
    text: "   A"
    "#);
    assert_eq!(grid.sized_cell_count(), 0);
}

#[test]
fn an_alt_screen_resize_that_cuts_a_sized_wide_scalar_erases_it() {
    let mut grid = drive(1, 8, "\x1b[?1049h\x1b[1;4H\x1b]66;w=3;字\x07".as_bytes());
    grid.resize(1, 4);
    insta::assert_snapshot!(render_roles(&grid), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: ·|·|·|·
    text: ""
    "#);
    assert_eq!(grid.sized_cell_count(), 0);
}

#[test]
fn a_saved_alt_screen_resize_that_cuts_a_sized_wide_scalar_erases_it() {
    let mut parser = felis_vt::Parser::new();
    let mut grid = Grid::new(1, 8);
    parser.advance(
        &mut grid,
        "\x1b[?47h\x1b[1;4H\x1b]66;w=3;字\x07\x1b[?47l".as_bytes(),
    );
    grid.resize(1, 4);
    parser.advance(&mut grid, b"\x1b[?47h");
    insta::assert_snapshot!(render_roles(&grid), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: ·|·|·|·
    text: ""
    "#);
    assert_eq!(grid.sized_cell_count(), 0);
}

#[test]
fn dch_beside_a_sized_wide_scalar_narrower_than_its_glyph_leaves_it() {
    insta::assert_snapshot!(snap(8, "\x1b]66;w=1;字\x07\x1b[1;4Hz\x1b[1;2H\x1b[P"), @r#"
    cursor: row=0 col=1 visible=1 pending_wrap=0
    cells: 字|·|z|·|·|·|·|·
    text: "字 z"
    "#);
}

#[test]
fn an_alt_screen_resize_keeps_a_sized_wide_scalar_whose_one_column_block_still_fits() {
    let mut grid = drive(1, 8, "\x1b[?1049h\x1b[1;4H\x1b]66;w=1;字\x07".as_bytes());
    grid.resize(1, 4);
    insta::assert_snapshot!(render_roles(&grid), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: ·|·|·|字
    text: "   字"
    "#);
    assert_eq!(grid.sized_cell_count(), 1);
}

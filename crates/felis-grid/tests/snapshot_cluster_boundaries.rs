//! Where a scalar after a ZWJ starts a new cell: UAX#29 GB11 joins only a
//! pictographic after a pictographic, so any other follower takes its own
//! cells and the cursor advances as an application summing widths expects.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{drive, render_roles};

fn snap(text: &str) -> String {
    render_roles(&drive(1, 8, format!("{text}|").as_bytes()))
}

#[test]
fn a_wide_letter_after_an_emoji_and_zwj_takes_its_own_cells() {
    insta::assert_snapshot!(snap("👩\u{200D}字"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👩\u{200d}|_|字|_|||·|·|·
    text: "👩\u{200d}字|"
    "#);
}

#[test]
fn indic_letters_joined_by_zwj_stay_one_cell_each() {
    insta::assert_snapshot!(snap("क\u{200D}ख\u{200D}ग"), @r#"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    cells: क\u{200d}|ख\u{200d}|ग|||·|·|·|·
    text: "क\u{200d}ख\u{200d}ग|"
    "#);
}

#[test]
fn a_virama_and_zwj_conjunct_splits_at_the_zwj() {
    insta::assert_snapshot!(snap("क\u{094D}\u{200D}ख"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: क\u{94d}\u{200d}|ख|||·|·|·|·|·
    text: "क\u{94d}\u{200d}ख|"
    "#);
}

#[test]
fn an_emoji_after_ascii_and_zwj_takes_its_own_cells() {
    insta::assert_snapshot!(snap("x\u{200D}👍"), @r#"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    cells: x\u{200d}|👍|_|||·|·|·|·
    text: "x\u{200d}👍|"
    "#);
}

#[test]
fn a_wide_letter_after_a_latin_letter_and_zwj_takes_its_own_cells() {
    insta::assert_snapshot!(snap("Ā\u{200D}字"), @r#"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    cells: Ā\u{200d}|字|_|||·|·|·|·
    text: "Ā\u{200d}字|"
    "#);
}

#[test]
fn a_narrow_pictographic_base_joins_an_emoji_across_zwj() {
    insta::assert_snapshot!(snap("❤\u{200D}🔥"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: ❤\u{200d}🔥|_|||·|·|·|·|·
    text: "❤\u{200d}🔥|"
    "#);
}

#[test]
fn a_text_symbol_base_joins_an_emoji_across_zwj() {
    insta::assert_snapshot!(snap("©\u{200D}🔥"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: ©\u{200d}🔥|_|||·|·|·|·|·
    text: "©\u{200d}🔥|"
    "#);
}

#[test]
fn a_family_sequence_chains_into_one_cluster() {
    insta::assert_snapshot!(snap("👨\u{200D}👩\u{200D}👧"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: 👨\u{200d}👩\u{200d}👧|_|||·|·|·|·|·
    text: "👨\u{200d}👩\u{200d}👧|"
    "#);
}

#[test]
fn a_skin_tone_modifier_before_the_zwj_still_joins() {
    insta::assert_snapshot!(snap("👍🏻\u{200D}🔥"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: 👍🏻\u{200d}🔥|_|||·|·|·|·|·
    text: "👍🏻\u{200d}🔥|"
    "#);
}

#[test]
fn a_selector_before_the_zwj_still_joins() {
    insta::assert_snapshot!(snap("❤\u{FE0F}\u{200D}🔥"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: ❤\u{fe0f}\u{200d}🔥|_|||·|·|·|·|·
    text: "❤\u{fe0f}\u{200d}🔥|"
    "#);
}

#[test]
fn a_doubled_zwj_does_not_join() {
    insta::assert_snapshot!(snap("👍\u{200D}\u{200D}🔥"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👍\u{200d}\u{200d}|_|🔥|_|||·|·|·
    text: "👍\u{200d}\u{200d}🔥|"
    "#);
}

#[test]
fn a_flag_does_not_join_an_emoji_across_zwj() {
    insta::assert_snapshot!(snap("🇯🇵\u{200D}🔥"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 🇯🇵\u{200d}|_|🔥|_|||·|·|·
    text: "🇯🇵\u{200d}🔥|"
    "#);
}

#[test]
fn a_modifier_after_a_letter_stays_in_its_cluster() {
    insta::assert_snapshot!(snap("a🏻b"), @r#"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    cells: a🏻|_|b|||·|·|·|·
    text: "a🏻b|"
    "#);
}

#[test]
fn a_bidi_override_after_the_base_breaks_the_join() {
    insta::assert_snapshot!(snap("👩\u{202E}\u{200D}💻"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👩\u{202e}\u{200d}|_|💻|_|||·|·|·
    text: "👩\u{202e}\u{200d}💻|"
    "#);
}

#[test]
fn a_bidi_override_held_from_before_the_base_does_not_break_the_join() {
    insta::assert_snapshot!(snap("\u{202E}👩\u{200D}💻"), @r#"
    cursor: row=0 col=3 visible=1 pending_wrap=0
    cells: 👩\u{202e}\u{200d}💻|_|||·|·|·|·|·
    text: "👩\u{202e}\u{200d}💻|"
    "#);
}

#[test]
fn a_bidi_override_after_a_modifier_breaks_the_join() {
    insta::assert_snapshot!(snap("👍🏻\u{202E}\u{200D}🔥"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👍🏻\u{202e}\u{200d}|_|🔥|_|||·|·|·
    text: "👍🏻\u{202e}\u{200d}🔥|"
    "#);
}

#[test]
fn a_zero_width_space_before_the_zwj_breaks_the_join() {
    insta::assert_snapshot!(snap("👍\u{200B}\u{200D}🔥"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👍\u{200b}\u{200d}|_|🔥|_|||·|·|·
    text: "👍\u{200b}\u{200d}🔥|"
    "#);
}

#[test]
fn an_override_after_the_base_breaks_the_join_across_reads() {
    let mut parser = felis_vt::Parser::new();
    let mut grid = felis_grid::Grid::new(1, 8);
    for chunk in ["👩\u{202E}", "\u{200D}", "💻|"] {
        common::drive_with(&mut parser, &mut grid, chunk.as_bytes());
    }
    insta::assert_snapshot!(render_roles(&grid), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👩\u{202e}\u{200d}|_|💻|_|||·|·|·
    text: "👩\u{202e}\u{200d}💻|"
    "#);
}

#[test]
fn an_override_on_an_earlier_emoji_does_not_break_a_later_join() {
    insta::assert_snapshot!(snap("👩\u{202E}👩\u{200D}💻"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👩\u{202e}|_|👩\u{200d}💻|_|||·|·|·
    text: "👩\u{202e}👩\u{200d}💻|"
    "#);
}

#[test]
fn an_override_on_an_earlier_emoji_does_not_break_a_later_join_across_reads() {
    let mut parser = felis_vt::Parser::new();
    let mut grid = felis_grid::Grid::new(1, 8);
    for chunk in ["👩\u{202E}", "👩\u{200D}", "💻|"] {
        common::drive_with(&mut parser, &mut grid, chunk.as_bytes());
    }
    insta::assert_snapshot!(render_roles(&grid), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👩\u{202e}|_|👩\u{200d}💻|_|||·|·|·
    text: "👩\u{202e}👩\u{200d}💻|"
    "#);
}

#[test]
fn a_query_between_the_override_and_the_zwj_keeps_the_break() {
    insta::assert_snapshot!(snap("👩\u{202E}\x1b[?2027$p\u{200D}💻"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👩\u{202e}\u{200d}|_|💻|_|||·|·|·
    text: "👩\u{202e}\u{200d}💻|"
    "#);
}

#[test]
fn a_nul_between_the_override_and_the_zwj_keeps_the_break() {
    insta::assert_snapshot!(snap("👩\u{202E}\0\u{200D}💻"), @r#"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    cells: 👩\u{202e}\u{200d}|_|💻|_|||·|·|·
    text: "👩\u{202e}\u{200d}💻|"
    "#);
}

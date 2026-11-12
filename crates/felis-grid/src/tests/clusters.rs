use super::*;

#[test]
fn utf8_print_lands_a_single_char() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 5);
    drive(&mut p, &mut g, "あ".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Char('あ'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn east_asian_wide_at_end_of_line_wraps_instead_of_squashing() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, "abあ".as_bytes());
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Char('あ'));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cursor().col, 3);
    assert!(g.cursor().pending_wrap);

    drive(&mut p, &mut g, "い".as_bytes());
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Char('い'));
    assert_eq!(g.cell(1, 1).unwrap().grapheme, Grapheme::Spacer);
}

#[test]
fn wide_glyph_at_last_two_columns_parks_cursor_with_pending_wrap() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 6);
    drive(&mut p, &mut g, "abcdあ".as_bytes());
    assert_eq!(g.cell(0, 4).unwrap().grapheme, Grapheme::Char('あ'));
    assert_eq!(g.cell(0, 5).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cursor().col, 5);
    assert!(g.cursor().pending_wrap);
    drive(&mut p, &mut g, b"X");
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'X'));
}

#[test]
fn wide_glyph_starting_at_last_column_wraps_instead_of_overflowing() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, "abcあ".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'c'));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Char('あ'));
    assert_eq!(g.cell(1, 1).unwrap().grapheme, Grapheme::Spacer);
}

#[test]
fn typing_over_wide_left_half_blanks_the_orphan_spacer() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, "あ\rX".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'X'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
}

#[test]
fn typing_over_wide_right_half_blanks_the_orphan_owner() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, "あ\r\x1b[CY".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'Y'));
}

#[test]
fn ech_extends_left_to_clean_an_orphan_spacer() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, "あ\r\x1b[C\x1b[X".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
}

#[test]
fn ech_extends_right_to_clean_an_orphan_owner() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 5);
    drive(&mut p, &mut g, "aあ\r\x1b[2X".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
}

#[test]
fn el_to_eol_extends_left_when_starting_on_a_spacer() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, "あbc\r\x1b[C\x1b[K".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
}

#[test]
fn combining_mark_attaches_to_previous_cell_as_cluster() {
    // U+0301 is width 0: the previous cell becomes a cluster and the
    // cursor does not advance.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, "e\u{0301}".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold a cluster");
    };
    assert_eq!(g.cluster_str(id), Some("e\u{0301}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cursor().col, 1);
}

#[test]
fn second_combining_mark_extends_the_existing_cluster() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, "e\u{0301}\u{0327}".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold a cluster");
    };
    assert_eq!(g.cluster_str(id), Some("e\u{0301}\u{0327}"));
    assert_eq!(g.cursor().col, 1);
}

#[test]
fn zwj_emoji_sequence_forms_one_wide_cluster() {
    // GB11: the base after a ZWJ joins the same cluster; otherwise the
    // font never sees the joined form and renders two glyphs across four
    // columns.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "\u{1F469}\u{200D}\u{1F4BB}".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the joined cluster");
    };
    assert_eq!(g.cluster_str(id), Some("\u{1F469}\u{200D}\u{1F4BB}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn zwj_family_emoji_chains_into_one_cluster() {
    // Each interior ZWJ re-arms the continuation.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(
        &mut p,
        &mut g,
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".as_bytes(),
    );
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the joined family cluster");
    };
    assert_eq!(
        g.cluster_str(id),
        Some("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}")
    );
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn stray_zwj_does_not_swallow_following_ascii() {
    // GB11 joins only a pictographic, so a ZWJ never absorbs ordinary text.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "\u{1F469}\u{200D}X".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the woman+ZWJ cluster");
    };
    assert_eq!(g.cluster_str(id), Some("\u{1F469}\u{200D}"));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'X'));
    assert_eq!(g.cursor().col, 3);
}

const WOMAN_ZWJ: &str = "\u{1F469}\u{200D}";

const LAPTOP: char = '\u{1F4BB}';

#[test]
fn backspace_disarms_a_pending_zwj() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(
        &mut p,
        &mut g,
        format!("{WOMAN_ZWJ}\x08{LAPTOP}").as_bytes(),
    );
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Char(LAPTOP));
}

#[test]
fn tab_disarms_a_pending_zwj() {
    // The cluster ends one column short of the right edge, so HT leaves
    // the cursor right behind it.
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(
        &mut p,
        &mut g,
        format!("\x1b[1;6H{WOMAN_ZWJ}\t{LAPTOP}").as_bytes(),
    );
    let Grapheme::Cluster(id) = g.cell(0, 5).unwrap().grapheme else {
        panic!("cell 0,5 should hold the woman+ZWJ cluster");
    };
    assert_eq!(g.cluster_str(id), Some(WOMAN_ZWJ));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Char(LAPTOP));
}

#[test]
fn carriage_return_disarms_a_pending_zwj() {
    // With the left margin right behind the cluster, CR leaves the
    // cursor where the joiner put it.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(
        &mut p,
        &mut g,
        format!("\x1b[?69h\x1b[3;8s\x1b[1;1H{WOMAN_ZWJ}\r{LAPTOP}").as_bytes(),
    );
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Char(LAPTOP));
}

#[test]
fn line_feed_vt_and_ff_disarm_a_pending_zwj() {
    // The row below already holds a cluster ending in a joiner, right
    // behind where the line feed lands.
    for ctrl in ['\n', '\x0b', '\x0c'] {
        let mut p = Parser::new();
        let mut g = Grid::new(2, 8);
        drive(
            &mut p,
            &mut g,
            format!("\x1b[2;1H{WOMAN_ZWJ}\x1b[1;1H{WOMAN_ZWJ}{ctrl}{LAPTOP}").as_bytes(),
        );
        let Grapheme::Cluster(id) = g.cell(1, 0).unwrap().grapheme else {
            panic!("cell 1,0 should hold the woman+ZWJ cluster");
        };
        assert_eq!(g.cluster_str(id), Some(WOMAN_ZWJ), "{ctrl:?}");
        assert_eq!(
            g.cell(1, 2).unwrap().grapheme,
            Grapheme::Char(LAPTOP),
            "{ctrl:?}"
        );
    }
}

#[test]
fn bell_keeps_a_pending_zwj_armed() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(
        &mut p,
        &mut g,
        format!("{WOMAN_ZWJ}\x07{LAPTOP}").as_bytes(),
    );
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the joined cluster");
    };
    assert_eq!(g.cluster_str(id), Some("\u{1F469}\u{200D}\u{1F4BB}"));
}

#[test]
fn skin_tone_modifier_folds_into_emoji_base() {
    // A skin-tone modifier is a UAX#29 Extend, so it joins the base
    // cluster even though unicode-width reports it as width 2.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "\u{1F44B}\u{1F3FD}".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the wave+skin cluster");
    };
    assert_eq!(g.cluster_str(id), Some("\u{1F44B}\u{1F3FD}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn regional_indicator_pair_forms_one_flag_cluster() {
    // GB12/GB13: the second indicator folds onto the first into one
    // width-2 cluster.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "\u{1F1EF}\u{1F1F5}".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the flag cluster");
    };
    assert_eq!(g.cluster_str(id), Some("\u{1F1EF}\u{1F1F5}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn two_flags_pair_independently() {
    // GB12/GB13 pairs indicators 2-and-2: once a pair has folded into a
    // Cluster, the third cannot fold into it.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(
        &mut p,
        &mut g,
        "\u{1F1EF}\u{1F1F5}\u{1F1F0}\u{1F1F7}".as_bytes(),
    );
    let Grapheme::Cluster(jp) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the JP flag");
    };
    let Grapheme::Cluster(kr) = g.cell(0, 2).unwrap().grapheme else {
        panic!("cell 0,2 should hold the KR flag");
    };
    assert_eq!(g.cluster_str(jp), Some("\u{1F1EF}\u{1F1F5}"));
    assert_eq!(g.cluster_str(kr), Some("\u{1F1F0}\u{1F1F7}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cursor().col, 4);
}

#[test]
fn odd_regional_indicator_stays_a_lone_letter() {
    // A lone trailing indicator has no pair, so it stays its own
    // width-1 cell.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "\u{1F1EF}\u{1F1F5}\u{1F1F0}".as_bytes());
    let Grapheme::Cluster(_) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the flag cluster");
    };
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Char('\u{1F1F0}'));
    assert_eq!(g.cursor().col, 3);
}

#[test]
fn vs16_widens_a_narrow_base_to_two_cells() {
    // VS16 requests emoji presentation (width 2); the heart was placed
    // as one cell before the selector arrived, so the fold must claim a
    // second cell and advance the cursor.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "\u{2764}\u{FE0F}".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the heart+VS16 cluster");
    };
    assert_eq!(g.cluster_str(id), Some("\u{2764}\u{FE0F}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn keycap_sequence_forms_a_two_cell_cluster() {
    // Both marks are width 0; the completed keycap sequence is width 2,
    // so the ASCII base widens to two cells.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "1\u{FE0F}\u{20E3}".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the keycap cluster");
    };
    assert_eq!(g.cluster_str(id), Some("1\u{FE0F}\u{20E3}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn heart_on_fire_joins_despite_width_one_base() {
    // VS16 first widens the heart to two cells; the ZWJ then arms GB11
    // into that owner.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(
        &mut p,
        &mut g,
        "\u{2764}\u{FE0F}\u{200D}\u{1F525}".as_bytes(),
    );
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the heart-on-fire cluster");
    };
    assert_eq!(g.cluster_str(id), Some("\u{2764}\u{FE0F}\u{200D}\u{1F525}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn heart_on_fire_joins_without_vs16() {
    // Some producers omit the VS16: the width-1 base must not block the
    // ZWJ join.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "\u{2764}\u{200D}\u{1F525}".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the VS16-less heart-on-fire cluster");
    };
    assert_eq!(g.cluster_str(id), Some("\u{2764}\u{200D}\u{1F525}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn combining_mark_at_column_zero_with_no_prior_print_drops() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, "\u{0301}".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cursor().col, 0);
}

#[test]
fn char_span_covers_a_wide_character_from_either_half() {
    let mut g = Grid::new(1, 6);
    let mut p = Parser::new();
    drive(&mut p, &mut g, "a字b".as_bytes());
    let s = g.screen();
    assert_eq!(s.char_span(0, 0), (0, 0), "narrow");
    assert_eq!(s.char_span(0, 1), (1, 2), "lead");
    assert_eq!(s.char_span(0, 2), (1, 2), "spacer");
    assert_eq!(s.char_span(0, 3), (3, 3), "narrow after the pair");
    assert_eq!(s.char_span(0, 5), (5, 5), "blank last column");
}

#[test]
fn char_span_is_one_column_for_an_emoji_that_could_not_widen_at_the_edge() {
    let mut g = Grid::new(1, 3);
    let mut p = Parser::new();
    drive(&mut p, &mut g, "ab❤\u{FE0F}".as_bytes());
    assert_eq!(g.screen().char_span(0, 2), (2, 2));
}

#[test]
fn a_vs15_after_a_wide_emoji_folds_into_one_wide_cluster() {
    // The renderer picks a text face for the cluster from the VS15, so
    // the selector must reach it inside the cluster, not as a lone cell.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "\u{2B50}\u{FE0E}x".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the star and its selector");
    };
    assert_eq!(g.cluster_str(id), Some("\u{2B50}\u{FE0E}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

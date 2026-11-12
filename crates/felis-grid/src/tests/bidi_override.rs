use super::*;

fn cell_text(g: &Grid, row: u16, col: u16) -> String {
    let mut text = String::new();
    push_cell_text(
        g.cell(row, col).unwrap(),
        g.screen().cluster_table(),
        &mut text,
    );
    text
}

fn after_feeds(rows: u16, cols: u16, feeds: &[&str]) -> Grid {
    let mut p = Parser::new();
    let mut g = Grid::new(rows, cols);
    for feed in feeds {
        drive(&mut p, &mut g, feed.as_bytes());
    }
    g
}

#[test]
fn bidi_override_at_column_zero_folds_into_the_next_character() {
    let g = after_feeds(1, 8, &["\u{202E}evil"]);
    assert_eq!(cell_text(&g, 0, 0), "e\u{202E}");
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'v'));
    assert_eq!(g.cursor().col, 4);
}

#[test]
fn bidi_override_on_an_empty_cell_folds_into_the_next_character() {
    let g = after_feeds(1, 8, &["\x1b[1;4H\u{2066}x"]);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(cell_text(&g, 0, 3), "x\u{2066}");
    assert_eq!(g.cursor().col, 4);
}

#[test]
fn pending_bidi_override_survives_a_read_boundary() {
    let g = after_feeds(1, 8, &["\u{202E}", "x"]);
    assert_eq!(cell_text(&g, 0, 0), "x\u{202E}");
}

#[test]
fn pending_bidi_overrides_fold_together_in_order() {
    let g = after_feeds(1, 8, &["\u{2067}\u{202B}\u{2068}x"]);
    assert_eq!(cell_text(&g, 0, 0), "x\u{2067}\u{202B}\u{2068}");
    assert_eq!(g.cursor().col, 1);
}

#[test]
fn pending_bidi_overrides_past_the_queue_are_dropped() {
    let g = after_feeds(1, 8, &[&"\u{202E}".repeat(20), "x"]);
    assert_eq!(cell_text(&g, 0, 0), format!("x{}", "\u{202E}".repeat(8)));
}

#[test]
fn pending_bidi_override_folds_into_a_wide_character_keeping_its_width() {
    let g = after_feeds(1, 8, &["\u{202E}\u{3042}\u{3044}"]);
    assert_eq!(cell_text(&g, 0, 0), "\u{3042}\u{202E}");
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Char('\u{3044}'));
    assert_eq!(g.cursor().col, 4);
}

#[test]
fn pending_bidi_override_folds_into_a_wide_character_that_wraps() {
    let g = after_feeds(2, 3, &["\x1b[1;3H\u{202E}\u{3042}"]);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(cell_text(&g, 1, 0), "\u{3042}\u{202E}");
}

#[test]
fn combining_mark_after_a_pending_bidi_override_joins_the_same_cell() {
    let g = after_feeds(1, 8, &["\u{202E}e\u{0301}x"]);
    assert_eq!(cell_text(&g, 0, 0), "e\u{202E}\u{0301}");
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

#[test]
fn combining_mark_with_no_owner_leaves_the_bidi_override_pending() {
    let g = after_feeds(1, 8, &["\u{202E}\u{0301}x"]);
    assert_eq!(cell_text(&g, 0, 0), "x\u{202E}");
}

#[test]
fn zwj_sequence_after_a_pending_bidi_override_still_joins() {
    let g = after_feeds(1, 8, &["\u{202E}\u{1F469}\u{200D}\u{1F4BB}x"]);
    assert_eq!(cell_text(&g, 0, 0), "\u{1F469}\u{202E}\u{200D}\u{1F4BB}");
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

#[test]
fn flag_after_a_pending_bidi_override_still_pairs() {
    let g = after_feeds(1, 8, &["\u{202E}\u{1F1EF}\u{1F1F5}x"]);
    assert_eq!(cell_text(&g, 0, 0), "\u{1F1EF}\u{202E}\u{1F1F5}");
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

#[test]
fn bidi_override_refused_by_a_full_cluster_folds_into_the_next_character() {
    // 1 + 63 * 2 bytes leaves the `a` cluster one byte short of the cap.
    let full = format!("a{}", "\u{0301}".repeat(63));
    let g = after_feeds(1, 8, &[&full, "\u{202E}b"]);
    assert_eq!(cell_text(&g, 0, 0), full);
    assert_eq!(cell_text(&g, 0, 1), "b\u{202E}");
}

#[test]
fn bidi_override_past_the_watermark_skips_a_recycled_rows_stale_tail() {
    // The scroll recycles the row with `abcd` still in storage past a
    // watermark of 0; the override must not fold into the stale `b`.
    let g = after_feeds(1, 4, &["\x1b[?1049habcd\n", "\x1b[1;3H\u{202E}x"]);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(cell_text(&g, 0, 2), "x\u{202E}");
}

#[test]
fn line_end_discards_a_pending_bidi_override() {
    let g = after_feeds(2, 8, &["\u{202E}\r\nx"]);
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'x'));
    let g = after_feeds(1, 8, &["\u{202E}\nx"]);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

#[test]
fn cursor_motion_discards_a_pending_bidi_override() {
    for motion in ["\x1b[1;3H", "\x1b[C", "\x08", "\t", "\r"] {
        let g = after_feeds(1, 16, &["\u{202E}", motion, "x"]);
        let col = g.cursor().col - 1;
        assert_eq!(
            g.cell(0, col).unwrap().grapheme,
            Grapheme::Ascii(b'x'),
            "{motion:?}"
        );
    }
}

#[test]
fn escape_sequences_that_leave_the_cursor_discard_a_pending_bidi_override() {
    for seq in [
        "\x1b7",
        "\x1b[K",
        "\x1b[2J",
        "\x1b[?25l",
        "\x1bP$qm\x1b\\",
        "\x1b_Gi=1\x1b\\",
    ] {
        let g = after_feeds(1, 8, &["\u{202E}", seq, "x"]);
        assert_eq!(
            g.cell(0, 0).unwrap().grapheme,
            Grapheme::Ascii(b'x'),
            "{seq:?}"
        );
    }
}

#[test]
fn sgr_osc_and_bel_keep_a_bidi_override_pending() {
    for seq in [
        "\x1b[1;31m",
        "\x1b]8;;https://example.test/\x1b\\",
        "\x1b]0;t\x07",
        "\x07",
    ] {
        let g = after_feeds(1, 8, &["\u{202E}", seq, "x"]);
        assert_eq!(cell_text(&g, 0, 0), "x\u{202E}", "{seq:?}");
    }
}

#[test]
fn discarded_sos_and_pm_strings_keep_a_bidi_override_pending() {
    // The parser drops these strings whole, so the grid never sees them.
    for seq in ["\x1bXsos\x1b\\", "\x1b^pm\x1b\\"] {
        let g = after_feeds(1, 8, &["\u{202E}", seq, "x"]);
        assert_eq!(cell_text(&g, 0, 0), "x\u{202E}", "{seq:?}");
    }
}

#[test]
fn screen_switch_discards_a_pending_bidi_override() {
    let g = after_feeds(1, 8, &["\u{202E}\x1b[?1049hx"]);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'x'));
    let g = after_feeds(1, 8, &["\x1b[?1049h\u{202E}\x1b[?1049lx"]);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

#[test]
fn resize_discards_a_pending_bidi_override() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, "\u{202E}".as_bytes());
    g.resize(3, 8);
    drive(&mut p, &mut g, b"x");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'x'));
    drive(&mut p, &mut g, "\r\u{202E}".as_bytes());
    drop(g.reflow(3, 10));
    drive(&mut p, &mut g, b"y");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'y'));
}

#[test]
fn folded_bidi_override_scrolls_into_history_with_its_character() {
    let g = after_feeds(1, 8, &["\u{202E}x\r\n"]);
    let row = g.scrollback().row(0).unwrap();
    let mut text = String::new();
    push_cell_text(&row[0], g.screen().cluster_table(), &mut text);
    assert_eq!(text, "x\u{202E}");
}

/// The table is full but already holds `👩‍` and `👩‍💻`, so the joiner
/// and the pictograph fold while the override between them is refused.
#[test]
fn a_bidi_override_refused_by_a_full_table_still_breaks_the_join() {
    let mut g = Grid::new(1, 8);
    g.intern_cluster("\u{1F469}\u{200D}");
    g.intern_cluster("\u{1F469}\u{200D}\u{1F4BB}");
    for i in 0..CLUSTER_TABLE_CAP {
        if g.intern_cluster(&format!("a{i}")).is_none() {
            break;
        }
    }
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        "\u{1F469}\u{202E}\u{200D}\u{1F4BB}x".as_bytes(),
    );
    assert_eq!(cell_text(&g, 0, 0), "\u{1F469}\u{200D}");
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Char('\u{1F4BB}'));
    assert_eq!(g.cursor().col, 5);
}

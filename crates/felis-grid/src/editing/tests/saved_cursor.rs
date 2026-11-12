use super::*;

/// Without DECLRMM, `CSI s` is SCOSC (save cursor); DECSLRM is
/// ignored.
#[test]
fn csi_s_falls_through_to_scosc_when_decll_rmm_is_off() {
    let mut g = Grid::new(4, 16);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[3;6H\x1b[3;9s");
    assert_eq!(g.left_margin_for_test(), 0, "margins untouched");
    assert_eq!(g.right_margin_for_test(), 15, "margins untouched");
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b[u");
    assert_eq!(g.cursor().row, 2);
    assert_eq!(g.cursor().col, 5);
}

/// `XTERM_SAVE` / `XTERM_RESTORE` round-trip DECAWM (esctest's
/// `test_XtermSave_SaveSetState`).
#[test]
fn xterm_save_restore_round_trips_decawm() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[?7s\x1b[?7l\x1b[?7r");
    assert!(g.autowrap(), "DECAWM restored to set");
    drive(&mut p, &mut g, b"\x1b[?7l\x1b[?7s\x1b[?7h\x1b[?7r");
    assert!(!g.autowrap(), "DECAWM restored to reset");
}

/// `XTERM_RESTORE` of `?69` lands in the functional field, so DECSLRM
/// works again afterwards rather than falling through to SCOSC.
#[test]
fn xterm_save_restore_round_trips_declrmm() {
    let mut g = Grid::new(4, 10);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[?69s\x1b[?69l\x1b[?69r");
    assert!(
        g.left_right_margin_mode,
        "DECLRMM restored into the functional field"
    );
    drive(&mut p, &mut g, b"\x1b[3;6s");
    assert_eq!(g.left_margin_for_test(), 2);
    assert_eq!(g.right_margin_for_test(), 5);
}

/// `XTERM_RESTORE` on an unknown slot is a no-op (matches xterm).
#[test]
fn xterm_restore_on_unsaved_slot_is_a_noop() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7l\x1b[?7r");
    assert!(!g.autowrap());
}

/// `?1048` aliases DECSC / DECRC (esctest's
/// `test_DECSET_SaveRestoreCursor`).
#[test]
fn decset_1048_saves_cursor_then_decreset_restores_it() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[3;2H\x1b[?1048h\x1b[5;5H\x1b[?1048l");
    assert_eq!(g.cursor().row, 2, "cursor restored to row 2 (0-based)");
    assert_eq!(g.cursor().col, 1, "cursor restored to col 1 (0-based)");
}

/// `?47` re-entry restores the prior alt-buffer contents (esctest's
/// `test_DECSET_ALTBUF`).
#[test]
fn decset_47_re_entry_restores_prior_alt_contents() {
    let mut g = Grid::new(3, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?47h\x1b[1;1Hab\x1b[?47l\x1b[?47h");
    assert_eq!(
        g.cell(0, 0).map(|c| &c.grapheme),
        Some(&Grapheme::Ascii(b'a')),
    );
    assert_eq!(
        g.cell(0, 1).map(|c| &c.grapheme),
        Some(&Grapheme::Ascii(b'b')),
    );
}

/// `?1047` clears the alt buffer on leave, so re-entry lands on a
/// blank screen (xterm; esctest's `test_DECSET_OPT_ALTBUF`).
#[test]
fn decset_1047_re_entry_lands_on_blank_alt_buffer() {
    let mut g = Grid::new(3, 4);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"\x1b[?1047h\x1b[1;1Hab\x1b[?1047l\x1b[?1047h",
    );
    assert_eq!(g.cell(0, 0).map(|c| &c.grapheme), Some(&Grapheme::Empty));
    assert_eq!(g.cell(0, 1).map(|c| &c.grapheme), Some(&Grapheme::Empty));
}

/// `?47` does not home the cursor on entry (esctest's
/// `test_DECSET_ALTBUF`).
#[test]
fn decset_47_alt_buffer_preserves_cursor_on_entry() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[3;5H\x1b[?47h");
    assert_eq!(g.cursor().row, 2, "cursor stays at row 2 (0-based)");
    assert_eq!(g.cursor().col, 4, "cursor stays at col 4 (0-based)");
}

#[test]
fn decsc_decrc_round_trips_cursor_and_pen() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 8);
    drive(
        &mut p,
        &mut g,
        b"\x1b[2;3H\x1b[1;31m\x1b7\x1b[1;1H\x1b[0m\x1b8X",
    );
    let cell = *g.cell(1, 2).unwrap();
    assert_eq!(cell.grapheme, Grapheme::Ascii(b'X'));
    let attrs = g.style(cell.style);
    assert!(attrs.flags.contains(AttrFlags::BOLD));
    assert_eq!(attrs.fg, Color::Indexed(1));
    assert_eq!(g.cursor().row, 1);
    assert_eq!(g.cursor().col, 3);
}

#[test]
fn decrc_with_no_save_returns_cursor_to_default_home() {
    // xterm homes DECRC with an empty slot (esctest's
    // `test_*_MoveToHomeWhenNotSaved`).
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"\x1b[2;3H\x1b8");
    assert_eq!(g.cursor().row, 0, "cursor moved to home row");
    assert_eq!(g.cursor().col, 0, "cursor moved to home col");
}

/// esctest's `test_*_Reset`: write + save + DECSTR + write + restore +
/// write yields "cb" in row 0.
#[test]
fn decstr_resets_saved_slot_without_moving_cursor_then_decrc_homes() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"a\x1b7\x1b[!pb\x1b8c");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'c'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
}

/// DECRC restores origin mode alongside the cursor (esctest's
/// `test_*_ResetsOriginMode`).
#[test]
fn decrc_restores_origin_mode_alongside_cursor() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 8);
    drive(&mut p, &mut g, b"\x1b7\x1b[?6h\x1b8");
    assert!(!g.origin_mode());
    drive(&mut p, &mut g, b"\x1b[?6h\x1b7\x1b[?6l\x1b8");
    assert!(g.origin_mode());
}

#[test]
fn decsc_overwrites_a_prior_save() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b[2;2H\x1b7\x1b[3;3H\x1b7\x1b[1;1H\x1b8",
    );
    assert_eq!(g.cursor().row, 2);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn sco_csi_s_csi_u_share_the_same_slot_as_decsc_decrc() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    drive(&mut p, &mut g, b"\x1b[2;2H\x1b[s\x1b[1;1H\x1b8");
    assert_eq!(g.cursor().row, 1);
    assert_eq!(g.cursor().col, 1);

    let mut g = Grid::new(3, 4);
    drive(&mut p, &mut g, b"\x1b[3;3H\x1b7\x1b[1;1H\x1b[u");
    assert_eq!(g.cursor().row, 2);
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn decrc_clamps_when_the_screen_shrank_since_the_save() {
    let mut p = Parser::new();
    let mut g = Grid::new(5, 10);
    drive(&mut p, &mut g, b"\x1b[5;10H\x1b7");
    g.resize(2, 4);
    drive(&mut p, &mut g, b"\x1b8");
    assert!(g.cursor().row < g.rows());
    assert!(g.cursor().col < g.cols());
}

use super::*;

/// `?41` `MoreFix`: HT at a pending-wrap cell line-feeds first; with the
/// bit reset it leaves `pending_wrap` intact (esctest's
/// `test_DECSET_MoreFix`).
#[test]
fn morefix_changes_ht_behavior_at_pending_wrap() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?41h");
    drive(&mut p, &mut g, b"xxxxxxxx");
    assert_eq!(g.cursor().col, 7);
    assert!(g.cursor().pending_wrap);
    drive(&mut p, &mut g, b"\t");
    assert_eq!(g.cursor().row, 1);
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?41lxxxxxxxx");
    assert_eq!(g.cursor().col, 7);
    assert!(g.cursor().pending_wrap);
    drive(&mut p, &mut g, b"\t");
    assert_eq!(g.cursor().col, 7);
    assert!(
        g.cursor().pending_wrap,
        "HT at pending_wrap with MoreFix off keeps the deferred wrap"
    );
}

/// DECCOLM at Level 4 clears and homes even with DECNCSM set
/// (esctest's `test_DECSCL_Level4_SupportsDECSLRMDoesntSupportDECNCSM`).
#[test]
fn deccolm_clears_at_level4_even_when_decncsm_is_set() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"\x1b[64;1\"p\x1b[?40h\x1b[?3l\x1b[?95h\x1b[1;1H1\x1b[?3h",
    );
    assert_eq!(g.cell(0, 0).map(|c| &c.grapheme), Some(&Grapheme::Empty));
    assert_eq!(g.cursor().row, 0);
    assert_eq!(g.cursor().col, 0);
}

/// DECCOLM without `Allow80To132` is a no-op.
#[test]
fn deccolm_without_allow_80_to_132_is_a_noop() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1;1H1\x1b[?3h");
    assert_eq!(
        g.cell(0, 0).map(|c| &c.grapheme),
        Some(&Grapheme::Ascii(b'1')),
    );
}

#[test]
fn cha_moves_cursor_to_an_absolute_column() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"abc\x1b[6GX");
    assert_eq!(g.cell(0, 5).unwrap().grapheme, Grapheme::Ascii(b'X'));
    assert_eq!(g.cursor().row, 0);
    assert_eq!(g.cursor().col, 6);
}

#[test]
fn cha_default_is_column_one() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"abc\x1b[GZ");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'Z'));
}

#[test]
fn cha_clamps_overshoot_to_last_column() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b[99G");
    assert_eq!(g.cursor().col, 3);
}

#[test]
fn vpa_moves_cursor_to_an_absolute_row() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 4);
    drive(&mut p, &mut g, b"\x1b[3;2H\x1b[1dX");
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'X'));
    assert_eq!(g.cursor().row, 0);
}

#[test]
fn vpa_clamps_overshoot_to_last_row() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    drive(&mut p, &mut g, b"\x1b[99d");
    assert_eq!(g.cursor().row, 2);
}

#[test]
fn ech_erases_n_cells_without_moving_cursor() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 6);
    drive(&mut p, &mut g, b"abcdef\x1b[3;1H\x1b[3X");
    assert!(g.cell(0, 0).unwrap().is_blank());
    assert!(g.cell(0, 1).unwrap().is_blank());
    assert!(g.cell(0, 2).unwrap().is_blank());
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'd'));
    assert_eq!(g.cursor().col, 0);
}

#[test]
fn ech_default_is_one_cell() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"abcd\r\x1b[X");
    assert!(g.cell(0, 0).unwrap().is_blank());
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
}

#[test]
fn ech_clamps_count_at_end_of_line() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"abcd\x1b[3G\x1b[99X");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert!(g.cell(0, 2).unwrap().is_blank());
    assert!(g.cell(0, 3).unwrap().is_blank());
}

#[test]
fn decstr_resets_modes_and_pen_but_keeps_cells() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(
        &mut p,
        &mut g,
        b"hello\x1b[?2004h\x1b[?1004h\x1b[?1006h\x1b[?1003h\x1b[1;31m\x1b[2;3H\x1b7",
    );
    assert!(g.bracketed_paste());
    assert!(g.focus_reporting());
    assert_eq!(g.mouse_protocol(), MouseProtocol::AnyMotion);
    assert_eq!(g.mouse_encoding(), MouseEncoding::Sgr);

    // xterm's DECSTR leaves the cursor put (esctest's `test_*_Reset`).
    drive(&mut p, &mut g, b"\x1b[!p");

    assert!(!g.bracketed_paste());
    assert!(!g.focus_reporting());
    assert_eq!(g.mouse_protocol(), MouseProtocol::Off);
    assert_eq!(g.mouse_encoding(), MouseEncoding::Default);
    assert_eq!(g.pen(), Attributes::default());
    assert_eq!(g.cursor().row, 1);
    assert_eq!(g.cursor().col, 2);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'h'));
    assert_eq!(g.cell(0, 4 - 1).unwrap().grapheme, Grapheme::Ascii(b'l'));

    drive(&mut p, &mut g, b"\x1b8");
    assert_eq!(g.cursor().row, 0);
    assert_eq!(g.cursor().col, 0);
}

/// `EL 2` blanks the entire line regardless of cursor column and
/// touches only that line.
#[test]
fn el_2_clears_the_whole_line_regardless_of_cursor() {
    let mut g = Grid::new(2, 6);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1;1Habcdef\x1b[2;1HABCDEF");
    drive(&mut p, &mut g, b"\x1b[1;3H\x1b[2K");
    for c in 0..6 {
        assert_eq!(
            g.cell(0, c).unwrap().grapheme,
            Grapheme::Empty,
            "EL 2 must blank row 0 col {c}"
        );
    }
    assert_eq!(logical_row(&g, 1), "ABCDEF", "row 1 untouched");
}

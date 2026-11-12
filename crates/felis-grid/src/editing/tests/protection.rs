use super::*;

/// DECSERA preserves PROTECTED cells (esctest's `test_DECSERA_basic`).
#[test]
fn decsera_preserves_protected_cells_inside_rectangle() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1\"qab\x1b[0\"qcd");
    drive(&mut p, &mut g, b"\x1b[1;1;1;4${");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Empty);
}

/// DECSERA blanks ISO-protected cells; only DECSCA protection
/// survives (esctest's `test_DECSERA_doesNotRespectISOProtect`).
#[test]
fn decsera_does_not_respect_iso_protect() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"a\x1bVb\x1bW");
    drive(&mut p, &mut g, b"\x1b[1;1;1;2${");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
}

/// ED / EL / ECH skip ISO-protected cells but not DECSCA-protected
/// ones (esctest's `test_{ED,EL,ECH}_respectsISOProtection`).
#[test]
fn ed_el_ech_respect_iso_protect_only() {
    let iso = Grapheme::Ascii(b'c');
    let blank = Grapheme::Empty;
    // An EL 2 turned no-op would keep the protected cell too, so each
    // case also pins the unprotected cells blanked.
    let cases: [(&str, &[u8], [Grapheme; 3]); 4] = [
        (
            "ED 0 over SPA",
            b"ab\x1bVc\x1bW\x1b[1;1H\x1b[0J",
            [blank, blank, iso],
        ),
        (
            "EL 2 over SPA",
            b"ab\x1bVc\x1bW\x1b[1;1H\x1b[2K",
            [blank, blank, iso],
        ),
        (
            "ECH 3 over SPA",
            b"ab\x1bVc\x1bW\x1b[1;1H\x1b[3X",
            [blank, blank, iso],
        ),
        (
            "ED 0 over DECSCA",
            b"ab\x1b[1\"qc\x1b[0\"q\x1b[1;1H\x1b[0J",
            [blank, blank, blank],
        ),
    ];
    for (what, bytes, expected) in cases {
        let mut g = Grid::new(2, 8);
        let mut p = Parser::new();
        drive(&mut p, &mut g, bytes);
        let got: Vec<Grapheme> = (0..3).map(|c| g.cell(0, c).unwrap().grapheme).collect();
        assert_eq!(got, expected, "{what}");
    }
}

/// ED 2 blanks even ISO-protected cells: esctest's `reset()` issues
/// it between tests, so protection must not leak into the next test.
#[test]
fn ed_2_unconditionally_clears_iso_protected_cells() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bVab\x1bW\x1b[2J");
    for c in 0..2 {
        assert_eq!(g.cell(0, c).unwrap().grapheme, Grapheme::Empty);
    }
}

/// DECSED honors ISO protection (vttest's SPA / EPA suite); xterm
/// models it differently from DECSERA, which honors only the DEC bit.
#[test]
fn decsed_respects_iso_protect() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"a\x1bVb\x1bW\x1b[1;1H");
    drive(&mut p, &mut g, b"\x1b[?0J");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
}

/// Erases row `row` with EL 2 and returns what is left in column `col`.
fn after_el_2(g: &mut Grid, p: &mut Parser, row: u16, col: u16) -> Grapheme {
    drive(p, g, format!("\x1b[{};1H\x1b[2K", row + 1).as_bytes());
    g.cell(row, col).unwrap().grapheme
}

#[test]
fn el_skips_no_cell_while_no_iso_protected_style_exists() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1\"qab\x1b[0\"q\x1b[31mcd\x1b[m");
    assert!(!g.screen.style_table.has_iso_protected());
    for c in 0..4 {
        assert_eq!(after_el_2(&mut g, &mut p, 0, c), Grapheme::Empty);
    }
    assert_eq!(g.row_content(0).map(<[Cell]>::len), Some(0));
}

/// A protected survivor keeps the row's content in view past the erase.
#[test]
fn el_keeps_the_row_content_up_to_a_protected_survivor() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"a\x1bVb\x1bWc");
    assert_eq!(after_el_2(&mut g, &mut p, 0, 1), Grapheme::Ascii(b'b'));
    assert!(g.row_content(0).is_some_and(|row| row.len() >= 2));
}

#[test]
fn iso_protection_survives_a_style_sweep_on_either_path() {
    for grid_level in [true, false] {
        let mut g = Grid::new(2, 8);
        let mut p = Parser::new();
        drive(&mut p, &mut g, b"a\x1bVb\x1bW\x1b[31m\x1b[m");
        let before = g.style_table_len();
        if grid_level {
            g.gc_styles();
        } else {
            g.screen.gc_styles();
        }
        assert!(g.style_table_len() < before, "the sweep must compact");
        assert_eq!(after_el_2(&mut g, &mut p, 0, 1), Grapheme::Ascii(b'b'));
        assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    }
}

/// A sweep drops a protected pen no cell holds; DECRC must bring the
/// protection back with the pen.
#[test]
fn a_protected_pen_restored_after_a_sweep_protects_again() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bV\x1b7\x1bW");
    g.gc_styles();
    assert!(!g.screen.style_table.has_iso_protected());
    drive(&mut p, &mut g, b"\x1b8c");
    assert_eq!(after_el_2(&mut g, &mut p, 0, 0), Grapheme::Ascii(b'c'));
}

#[test]
fn iso_protection_is_honored_again_after_ris() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bVa\x1bW\x1bc");
    assert!(!g.screen.style_table.has_iso_protected());
    drive(&mut p, &mut g, b"\x1bVb\x1bW");
    assert_eq!(after_el_2(&mut g, &mut p, 0, 0), Grapheme::Ascii(b'b'));
}

/// Both screens share one style table: a protected cell set aside on
/// the primary still counts while the alternate screen is up, and a
/// sweep there must keep it.
#[test]
fn iso_protection_holds_across_the_alternate_screen() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bVa\x1bW\x1b[?1049h\x1b[31m\x1b[m");
    g.gc_styles();
    assert_eq!(after_el_2(&mut g, &mut p, 0, 0), Grapheme::Empty);
    drive(&mut p, &mut g, b"\x1b[?1049l");
    assert_eq!(after_el_2(&mut g, &mut p, 0, 0), Grapheme::Ascii(b'a'));

    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?1049h\x1bVb\x1bW");
    assert_eq!(after_el_2(&mut g, &mut p, 0, 0), Grapheme::Ascii(b'b'));
}

#[test]
fn iso_protection_survives_a_resize() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bVa\x1bW");
    g.resize(3, 4);
    g.resize(2, 8);
    g.gc_styles();
    assert_eq!(after_el_2(&mut g, &mut p, 0, 0), Grapheme::Ascii(b'a'));
}

/// `DECSEL 0` erases cursor to EOL, sparing DECSCA-protected cells.
#[test]
fn decsel_0_erases_cursor_to_eol_and_spares_protected_cells() {
    let mut g = Grid::new(1, 6);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1;1Habc\x1b[1\"qP\x1b[0\"qef");
    drive(&mut p, &mut g, b"\x1b[1;3H\x1b[?0K");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 4).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 5).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'P'));
}

/// `DECSEL 1` erases start of line through the cursor cell inclusive,
/// sparing protected cells.
#[test]
fn decsel_1_erases_start_to_cursor_inclusive_and_spares_protected_cells() {
    let mut g = Grid::new(1, 6);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1;1Ha\x1b[1\"qP\x1b[0\"qcdef");
    drive(&mut p, &mut g, b"\x1b[1;3H\x1b[?1K");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'P'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'd'));
    assert_eq!(g.cell(0, 4).unwrap().grapheme, Grapheme::Ascii(b'e'));
    assert_eq!(g.cell(0, 5).unwrap().grapheme, Grapheme::Ascii(b'f'));
}

/// `DECSED 2` wipes every physical cell, honoring protection.
#[test]
fn decsed_2_erases_the_whole_unprotected_display() {
    let mut g = Grid::new(3, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc");
    drive(&mut p, &mut g, b"\x1b[2;3H\x1b[?2J");
    for r in 0..3 {
        assert_eq!(logical_row(&g, r), "    ", "DECSED 2 left row {r} dirty");
    }
}

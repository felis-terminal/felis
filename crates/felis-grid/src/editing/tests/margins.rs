use super::*;

/// Rectangle ops shift their column arguments by `left_margin` under
/// DECOM + DECLRMM (esctest's
/// `test_DEC{ERA,FRA,SERA,CRA}_respectsOriginMode`).
#[test]
fn rectangle_ops_shift_column_origin_under_decom_with_declrmm() {
    let mut g = Grid::new(8, 10);
    let mut p = Parser::new();
    for r in 0..8 {
        drive(&mut p, &mut g, format!("\x1b[{};1H", r + 1).as_bytes());
        drive(&mut p, &mut g, b"abcdefghij".get(0..10).unwrap());
    }
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[2;9s\x1b[2;7r\x1b[?6h");
    drive(&mut p, &mut g, b"\x1b[1;1;3;3$z");
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    for c in 1..=3 {
        assert_eq!(g.cell(1, c).unwrap().grapheme, Grapheme::Empty);
    }
    assert_eq!(g.cell(1, 4).unwrap().grapheme, Grapheme::Ascii(b'e'));
}

/// DECBI / DECFI scroll only the DECSTBM × DECSLRM rectangle
/// (esctest's `test_DECBI_Scrolls` / `test_DECFI_Scrolls`).
#[test]
fn decbi_and_decfi_scroll_only_within_decstbm_x_decslrm_rectangle() {
    let mut g = Grid::new(7, 7);
    let mut p = Parser::new();
    let rows_data: [&[u8]; 5] = [b"abcde", b"fghij", b"klmno", b"pqrst", b"uvwxy"];
    for (i, s) in rows_data.iter().enumerate() {
        drive(&mut p, &mut g, format!("\x1b[{};2H", 3 + i).as_bytes());
        drive(&mut p, &mut g, s);
    }
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[3;5s\x1b[4;6r\x1b[5;3H");
    drive(&mut p, &mut g, b"\x1b6");
    assert_eq!(logical_row(&g, 2), " abcde ");
    assert_eq!(logical_row(&g, 3), " f ghj ");
    assert_eq!(logical_row(&g, 6), " uvwxy ");
}

/// CHT clamps at `right_margin` under DECLRMM regardless of the start
/// column (esctest's `test_CHT_IgnoresScrollingRegion`).
#[test]
fn cht_clamps_at_right_margin_when_declrmm_is_engaged() {
    let mut g = Grid::new(4, 40);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[5;30s");
    drive(&mut p, &mut g, b"\x1b[1;7H\x1b[2I");
    assert_eq!(g.cursor().col, 16);
    drive(&mut p, &mut g, b"\x1b[2I");
    assert_eq!(g.cursor().col, 29);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b[9I");
    assert_eq!(g.cursor().col, 29);
}

/// CBT ignores the DECSLRM band and reaches column 0 (esctest's
/// `test_CBT_IgnoresRegion`).
#[test]
fn cbt_ignores_left_margin_and_reaches_column_zero() {
    let mut g = Grid::new(4, 40);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[5;30s");
    drive(&mut p, &mut g, b"\x1b[9;7H\x1b[2Z");
    assert_eq!(g.cursor().col, 0);
}

/// DECALN clears DECSTBM and DECLRMM (esctest's
/// `test_DECALN_ClearsMargins`).
#[test]
fn decaln_clears_top_bottom_and_left_right_margins() {
    let mut g = Grid::new(8, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[2;3s\x1b[4;5r\x1b#8");
    assert!(!g.left_right_margin_mode, "DECLRMM must reset");
    assert_eq!(g.margins.top, 0, "DECSTBM top must reset");
    assert_eq!(g.margins.bottom, 7, "DECSTBM bottom must reset");
    assert_eq!(g.margins.left, 0, "left margin must reset");
    assert_eq!(g.margins.right, 7, "right margin must reset");
    drive(&mut p, &mut g, b"\x1b[4;2H\x1b[A");
    assert_eq!(g.cursor().row, 2);
    assert_eq!(g.cursor().col, 1);
}

/// DECSLRM is ignored at conformance level 3 (esctest's
/// `test_DSCSCL_Level3_SupportsDECRQMDoesntSupportDECSLRM`).
#[test]
fn decslrm_is_silently_dropped_at_conformance_level_below_four() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[63;1\"p");
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[2;4s");
    assert_eq!(g.margins.left, 0, "DECSLRM ignored → left margin stays 0");
    assert_eq!(
        g.margins.right,
        g.screen.cols.saturating_sub(1),
        "DECSLRM ignored → right margin stays cols-1"
    );
    drive(&mut p, &mut g, b"\x1b[64;1\"p\x1b[2;4s");
    assert_eq!(g.margins.left, 1);
    assert_eq!(g.margins.right, 3);
}

/// SU under DECLRMM scrolls only the margin-bounded sub-rectangle
/// (esctest's `test_SU_RespectsLeftRightScrollRegion`).
#[test]
fn su_under_declrmm_scrolls_only_within_left_right_margins() {
    let mut g = Grid::new(5, 5);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcde\r\nfghij\r\nklmno\r\npqrst\r\nuvwxy");
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[2;4s\x1b[3;2H\x1b[2S");
    let row_str = |g: &Grid, r: u16| -> String {
        (0..g.cols())
            .map(|c| match &g.cell(r, c).unwrap().grapheme {
                Grapheme::Ascii(b) => *b as char,
                Grapheme::Empty => '.',
                _ => '?',
            })
            .collect()
    };
    assert_eq!(row_str(&g, 0), "almne");
    assert_eq!(row_str(&g, 1), "fqrsj");
    assert_eq!(row_str(&g, 2), "kvwxo");
    assert_eq!(row_str(&g, 3), "p...t");
    assert_eq!(row_str(&g, 4), "u...y");
}

/// SD mirror: rectangle slides down, top `n` rows blank.
#[test]
fn sd_under_declrmm_scrolls_only_within_left_right_margins() {
    let mut g = Grid::new(5, 5);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcde\r\nfghij\r\nklmno\r\npqrst\r\nuvwxy");
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[2;4s\x1b[3;2H\x1b[2T");
    let row_str = |g: &Grid, r: u16| -> String {
        (0..g.cols())
            .map(|c| match &g.cell(r, c).unwrap().grapheme {
                Grapheme::Ascii(b) => *b as char,
                Grapheme::Empty => '.',
                _ => '?',
            })
            .collect()
    };
    assert_eq!(row_str(&g, 0), "a...e");
    assert_eq!(row_str(&g, 1), "f...j");
    assert_eq!(row_str(&g, 2), "kbcdo");
    assert_eq!(row_str(&g, 3), "pghit");
    assert_eq!(row_str(&g, 4), "ulmny");
}

/// DCH under DECLRMM bounds its shift to `[cursor, right_margin]`
/// (esctest's `test_DCH_RespectsMargins`).
#[test]
fn dch_respects_left_right_margins_under_declrmm() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdefgh\x1b[?69h\x1b[2;5s\x1b[1;3H\x1b[P");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'd'));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'e'));
    assert_eq!(g.cell(0, 4).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 5).unwrap().grapheme, Grapheme::Ascii(b'f'));
    assert_eq!(g.cell(0, 7).unwrap().grapheme, Grapheme::Ascii(b'h'));
}

/// DCH outside the left margin is a no-op.
#[test]
fn dch_outside_left_right_margins_is_noop() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"abcdefgh\x1b[?69h\x1b[2;5s\x1b[1;1H\x1b[99P",
    );
    for (i, want) in b"abcdefgh".iter().enumerate() {
        assert_eq!(
            g.cell(0, i as u16).unwrap().grapheme,
            Grapheme::Ascii(*want)
        );
    }
}

/// DECIC under DECLRMM bounds its shift at `right_margin` (esctest's
/// `test_DECIC_ScrollOffRightMarginInScrollRegion`).
#[test]
fn decic_respects_left_right_margins_under_declrmm() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"abcdefg\r\nABCDEFG\x1b[?69h\x1b[2;5s\x1b[1;3H\x1b['}",
    );
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'c'));
    assert_eq!(g.cell(0, 4).unwrap().grapheme, Grapheme::Ascii(b'd'));
    assert_eq!(g.cell(0, 5).unwrap().grapheme, Grapheme::Ascii(b'f'));
}

/// DECIC outside the left-right margin is a no-op.
#[test]
fn decic_outside_left_right_margins_is_noop() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"abcdefg\r\nABCDEFG\x1b[?69h\x1b[2;5s\x1b[1;1H\x1b[10'}",
    );
    for (i, want) in b"abcdefg".iter().enumerate() {
        assert_eq!(
            g.cell(0, i as u16).unwrap().grapheme,
            Grapheme::Ascii(*want)
        );
        assert_eq!(
            g.cell(1, i as u16).unwrap().grapheme,
            Grapheme::Ascii(want.to_ascii_uppercase())
        );
    }
}

/// DECSLRM parks the cursor at home: (0, 0) with DECOM off, the
/// region origin with DECOM on (esctest's `test_DECSET_DECLRMM`).
#[test]
fn decslrm_sets_margins_and_moves_cursor_to_home() {
    let mut g = Grid::new(4, 16);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[3;9s");
    assert_eq!(g.left_margin_for_test(), 2, "left_margin = 3-1");
    assert_eq!(g.right_margin_for_test(), 8, "right_margin = 9-1");
    assert_eq!(g.cursor().row, 0);
    assert_eq!(g.cursor().col, 0, "DECOM off → cursor at screen origin");
    let mut g = Grid::new(4, 16);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?6h\x1b[2;3r\x1b[?69h\x1b[3;9s");
    assert_eq!(g.cursor().row, 1, "DECOM on → scroll_top");
    assert_eq!(g.cursor().col, 2, "DECOM on → left_margin");
}

/// CR moves to the left margin under DECLRMM (esctest's
/// `test_CR_MovesToLeftMarginWhenRightOfLeftMargin` and variants).
#[test]
fn cr_respects_left_margin_under_declrmm() {
    let mut g = Grid::new(4, 16);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[5;10s\x1b[1;6H\r");
    assert_eq!(
        g.cursor().col,
        4,
        "cursor at col 5 (1-based 6) → left_margin (5)"
    );
    drive(&mut p, &mut g, b"\x1b[1;5H\r");
    assert_eq!(g.cursor().col, 4);
    drive(&mut p, &mut g, b"\x1b[1;4H\r");
    assert_eq!(g.cursor().col, 0);
    drive(&mut p, &mut g, b"\x1b[?6h\x1b[1;4H\r");
    assert_eq!(g.cursor().col, 4);
}

/// CUB stops at `left_margin` under DECLRMM.
/// esctest's `test_CUB_StopsAtLeftMarginInScrollRegion`.
#[test]
fn cub_stops_at_left_margin_under_declrmm() {
    let mut g = Grid::new(4, 16);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[5;10s\x1b[1;8H\x1b[20D");
    assert_eq!(g.cursor().col, 4, "CUB clamped to left_margin");
}

/// DECIC with the default param inserts one column in every row
/// (`test_DECIC_DefaultParam`).
#[test]
fn decic_inserts_one_blank_column_at_cursor_in_every_row() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdefg\r\nABCDEFG");
    drive(&mut p, &mut g, b"\x1b[1;2H");
    drive(&mut p, &mut g, b"\x1b['}");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert_eq!(g.cell(0, 6).unwrap().grapheme, Grapheme::Ascii(b'f'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'A'));
    assert_eq!(g.cell(1, 1).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(1, 6).unwrap().grapheme, Grapheme::Ascii(b'F'));
}

/// DECDC with the default param deletes one column in every row
/// (`test_DECDC_DefaultParam`).
#[test]
fn decdc_deletes_one_column_at_cursor_in_every_row() {
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdefg\r\nABCDEFG");
    drive(&mut p, &mut g, b"\x1b[1;2H\x1b['~");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'c'));
    assert_eq!(g.cell(0, 5).unwrap().grapheme, Grapheme::Ascii(b'g'));
    assert_eq!(g.cell(0, 6).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'A'));
    assert_eq!(g.cell(1, 5).unwrap().grapheme, Grapheme::Ascii(b'G'));
    assert_eq!(g.cell(1, 6).unwrap().grapheme, Grapheme::Empty);
}

/// DECFI advances or, at the right edge, scrolls the region left
/// (`test_DECFI_Basic`, `test_DECFI_WholeScreenScrolls`).
#[test]
fn decfi_moves_cursor_right_or_scrolls_region_left_at_edge() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1;2H\x1b9");
    assert_eq!(g.cursor().col, 2);
    drive(&mut p, &mut g, b"\x1b[1;4Hx\x1b9");
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'x'));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Empty);
}

/// DECBI retreats or, at the left edge, scrolls the region right.
#[test]
fn decbi_moves_cursor_left_or_scrolls_region_right_at_edge() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1;3H\x1b6");
    assert_eq!(g.cursor().col, 1);
    drive(&mut p, &mut g, b"\x1b[1;1Hx\x1b[1;1H\x1b6");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

/// DECCRA snapshots the source so an overlapping copy is well-defined.
#[test]
fn deccra_handles_overlapping_source_and_destination() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcd\r\nefgh");
    drive(&mut p, &mut g, b"\x1b[1;1;1;3;1;1;2;1$v");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'c'));
}

/// An inverted rectangle (Pt > Pb) is a no-op for all four
/// rectangular ops.
#[test]
fn rectangular_ops_with_inverted_rectangle_are_no_ops() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdefgh");
    drive(&mut p, &mut g, b"\x1b[5;5;4;4$z");
    drive(&mut p, &mut g, b"\x1b[33;5;5;4;4$x");
    drive(&mut p, &mut g, b"\x1b[5;5;4;4${");
    drive(&mut p, &mut g, b"\x1b[5;5;4;4;1;1;1;1$v");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 7).unwrap().grapheme, Grapheme::Ascii(b'h'));
}

/// HT clamps at `right_margin` under DECLRMM (esctest's
/// `test_DECSET_DECAWM_NoLineWrapOnTabWithLeftRightMargin`).
#[test]
fn ht_clamps_at_right_margin_under_declrmm() {
    let mut g = Grid::new(2, 30);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[?69h\x1b[10;20s\x1b[1;1H");
    drive(&mut p, &mut g, b"\t");
    assert_eq!(g.cursor().col, 8);
    drive(&mut p, &mut g, b"\t");
    assert_eq!(g.cursor().col, 16);
    drive(&mut p, &mut g, b"\t");
    assert_eq!(g.cursor().col, 19);
    drive(&mut p, &mut g, b"\t");
    assert_eq!(g.cursor().col, 19);
}

/// `IL` inserts blank rows at the cursor and shifts the rest down
/// within the region.
#[test]
fn il_inserts_blank_rows_and_shifts_rest_down_within_region() {
    let mut g = Grid::new(5, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    drive(&mut p, &mut g, b"\x1b[2;1H\x1b[2L");
    assert_eq!(logical_row(&g, 0), "aaaa");
    assert_eq!(logical_row(&g, 1), "    ");
    assert_eq!(logical_row(&g, 2), "    ");
    assert_eq!(logical_row(&g, 3), "bbbb");
    assert_eq!(logical_row(&g, 4), "cccc");
}

/// `DL` deletes rows at the cursor and shifts the rest up within the
/// region.
#[test]
fn dl_deletes_rows_and_shifts_rest_up_within_region() {
    let mut g = Grid::new(5, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    drive(&mut p, &mut g, b"\x1b[2;1H\x1b[2M");
    assert_eq!(logical_row(&g, 0), "aaaa");
    assert_eq!(logical_row(&g, 1), "dddd");
    assert_eq!(logical_row(&g, 2), "eeee");
    assert_eq!(logical_row(&g, 3), "    ");
    assert_eq!(logical_row(&g, 4), "    ");
}

/// `IL` / `DL` are no-ops when the cursor is outside the DECSTBM
/// region.
#[test]
fn il_dl_are_no_ops_outside_the_scroll_region() {
    let mut g = Grid::new(5, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    drive(&mut p, &mut g, b"\x1b[2;4r");
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b[2L");
    for r in 0..5 {
        let expected = ["aaaa", "bbbb", "cccc", "dddd", "eeee"][usize::from(r)];
        assert_eq!(
            logical_row(&g, r),
            expected,
            "IL above region moved row {r}"
        );
    }
    drive(&mut p, &mut g, b"\x1b[5;1H\x1b[2M");
    for r in 0..5 {
        let expected = ["aaaa", "bbbb", "cccc", "dddd", "eeee"][usize::from(r)];
        assert_eq!(
            logical_row(&g, r),
            expected,
            "DL below region moved row {r}"
        );
    }
}

/// `DCH` shifts the cells right of the cursor left by `n` and blanks
/// the rightmost `n`.
#[test]
fn dch_shifts_cells_left_and_blanks_tail_without_declrmm() {
    let mut g = Grid::new(1, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdefgh");
    drive(&mut p, &mut g, b"\x1b[1;3H\x1b[2P");
    assert_eq!(logical_row(&g, 0), "abefgh  ");
    assert_eq!(g.cursor().col, 2);
}

/// `ICH` inserts `n` blanks at the cursor and drops the rightmost `n`.
#[test]
fn ich_shifts_cells_right_and_blanks_gap_without_declrmm() {
    let mut g = Grid::new(1, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdefgh");
    drive(&mut p, &mut g, b"\x1b[1;3H\x1b[2@");
    assert_eq!(logical_row(&g, 0), "ab  cdef");
    assert_eq!(g.cursor().col, 2);
}

/// `CUF` clamps at the right margin when starting at or left of it
/// under DECLRMM, else at the physical edge.
#[test]
fn cuf_clamps_at_right_margin_then_physical_edge_under_declrmm() {
    let mut g = Grid::new(4, 20);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[3;12s");
    drive(&mut p, &mut g, b"\x1b[1;3H\x1b[100C");
    assert_eq!(g.cursor().col, 11);
    drive(&mut p, &mut g, b"\x1b[1;16H\x1b[100C");
    assert_eq!(g.cursor().col, 19);
}

/// `CUU` floors at the region top and `CUD` ceils at the region
/// bottom when the cursor is inside the region.
#[test]
fn cuu_and_cud_clamp_to_scroll_region_boundaries() {
    let mut g = Grid::new(10, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[3;8r");
    drive(&mut p, &mut g, b"\x1b[5;1H\x1b[100A");
    assert_eq!(g.cursor().row, 2);
    drive(&mut p, &mut g, b"\x1b[5;1H\x1b[100B");
    assert_eq!(g.cursor().row, 7);
}

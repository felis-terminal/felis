use crate::test_support::{drive, responses};
use crate::*;
use felis_vt::Parser;

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
    let row_str = |g: &Grid, row: u16| -> String {
        (0..g.screen.cols)
            .map(|c| match g.cell(row, c).unwrap().grapheme {
                Grapheme::Ascii(b) => char::from(b),
                Grapheme::Empty => ' ',
                _ => '?',
            })
            .collect()
    };
    assert_eq!(row_str(&g, 2), " abcde ");
    assert_eq!(row_str(&g, 3), " f ghj ");
    assert_eq!(row_str(&g, 6), " uvwxy ");
}

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

/// `DSR ? 996 n` answers the last client-reported OS preference,
/// defaulting to light; DECSET 2031 records the opt-in.
#[test]
fn decset_2031_and_dsr_996_report_color_scheme() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?2031h");
    assert!(g.color_scheme_notify());
    drive(&mut p, &mut g, b"\x1b[?996n");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1b[?997;2n");
    g.set_os_dark(true);
    drive(&mut p, &mut g, b"\x1b[?996n");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1b[?997;1n");
    drive(&mut p, &mut g, b"\x1b[?2031l");
    assert!(!g.color_scheme_notify());
}

/// `DECSET 2048` records the opt-in and queues no response of its own
/// (the daemon is the only emitter of the report); the report reads
/// the live geometry.
#[test]
fn decset_2048_records_the_mode_without_queueing_a_response() {
    let mut g = Grid::new(24, 80);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?2048h");
    assert!(g.in_band_resize_notify());
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    assert_eq!(
        std::str::from_utf8(&g.resize_notify_report_bytes()).unwrap(),
        "\x1b[48;24;80;0;0t"
    );
    drive(&mut p, &mut g, b"\x1b[?2048l");
    assert!(!g.in_band_resize_notify());
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
}

/// DECRQM round-trips mode 2048 as a functional mode (Ps=1 set /
/// Ps=2 reset), not the unknown Ps=0.
#[test]
fn decrqm_reports_2048_state() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?2048h\x1b[?2048$p");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(r.last().unwrap()).unwrap(),
        "\x1b[?2048;1$y"
    );
    drive(&mut p, &mut g, b"\x1b[?2048l\x1b[?2048$p");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1b[?2048;2$y");
}

/// DECRQM round-trips mode 2031 as a functional mode (Ps=1 set /
/// Ps=2 reset), not the unknown Ps=0.
#[test]
fn decrqm_reports_2031_state() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?2031h");
    drive(&mut p, &mut g, b"\x1b[?2031$p");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1b[?2031;1$y");
    drive(&mut p, &mut g, b"\x1b[?2031l");
    drive(&mut p, &mut g, b"\x1b[?2031$p");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1b[?2031;2$y");
}

/// `CSI Pn t` with Pn >= 24 (DECSLPP) is consumed silently without
/// falling through to a window-op reply; codes below 24 keep working.
#[test]
fn csi_pn_t_ge_24_is_consumed_and_decslpp_is_unsupported() {
    let mut g = Grid::new(8, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bP$qt\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP0$r\x1b\\");
    drive(&mut p, &mut g, b"\x1b[27t");
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    drive(&mut p, &mut g, b"\x1b[11t");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1b[1t");
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

/// DECRQM is swallowed at conformance level 2 (esctest's
/// `test_DECSCL_Level2DoesntSupportDECRQM`) and works again at level 4.
#[test]
fn decrqm_returns_no_reply_at_conformance_level_below_three() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[62;1\"p");
    drive(&mut p, &mut g, b"\x1b[4$p");
    let r = responses(&mut g);
    assert!(
        r.is_empty(),
        "DECRQM at level 2 must produce no reply (got {:?})",
        r.iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>()
    );
    drive(&mut p, &mut g, b"\x1b[64;1\"p\x1b[4$p");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1b[4;2$y");
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

/// DECRQCRA: bold subtracts one from the checksum.
#[test]
fn decrqcra_bold_attribute_subtracts_one_from_checksum() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1ma\x1b[3;1;1;1;1;1*y");
    let responses = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&responses[0]).unwrap(),
        "\x1bP3!~FF9E\x1b\\"
    );
}

/// DECRQCRA: missing rectangle params default to the whole grid.
#[test]
fn decrqcra_missing_rect_params_default_to_full_grid() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[4*y");
    let responses = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&responses[0]).unwrap(),
        "\x1bP4!~FC00\x1b\\"
    );
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

/// DECRQSS `s` reports the current margins as 1-based inclusive.
#[test]
fn decrqss_s_reports_left_right_margins() {
    let mut g = Grid::new(4, 16);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[5;10s\x1bP$qs\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP1$r5;10s\x1b\\");
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

/// DECRQSS answers `*x` (DECSACE) and `"p` (DECSCL) with the last-set
/// value, and the status-line / page-length requests (`$}`, `$~`,
/// `*|`, `t`) with the invalid form `0$r`.
#[test]
fn decrqss_answers_supported_settings_and_rejects_the_rest() {
    let mut g = Grid::new(8, 16);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[0*x\x1bP$q*x\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP1$r0*x\x1b\\");

    drive(&mut p, &mut g, b"\x1b[65;1\"p\x1bP$q\"p\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP1$r64;1\"p\x1b\\");

    for (setter, query) in [
        (&b"\x1b[1$}"[..], &b"\x1bP$q$}\x1b\\"[..]),
        (&b"\x1b[2$~"[..], &b"\x1bP$q$~\x1b\\"[..]),
        (&b"\x1b[24*|"[..], &b"\x1bP$q*|\x1b\\"[..]),
        (&b"\x1b[27t"[..], &b"\x1bP$qt\x1b\\"[..]),
    ] {
        drive(&mut p, &mut g, setter);
        drive(&mut p, &mut g, query);
        let r = responses(&mut g);
        assert_eq!(
            std::str::from_utf8(&r[0]).unwrap(),
            "\x1bP0$r\x1b\\",
            "unsupported DECRQSS must answer invalid"
        );
    }
}

/// BS without reverse-wraparound stops at col 0
/// (`test_BS_StopsAtOrigin`).
#[test]
fn backspace_stops_at_column_zero_without_reverse_wrap() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1;1H\x08");
    assert_eq!(g.cursor().col, 0);
    assert_eq!(g.cursor().row, 0);
}

/// `?1045` + DECAWM: BS at col 0 wraps to the previous row's last
/// column (`test_BS_WrapsInWraparoundMode`).
#[test]
fn backspace_with_reverse_wrap_extend_moves_to_previous_row_last_col() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[?1045h\x1b[3;1H\x08");
    assert_eq!(g.cursor().col, 3);
    assert_eq!(g.cursor().row, 1);
}

/// `?1045` + DECAWM: BS at top-left wraps to bottom-right
/// (`test_BS_ReverseWrapGoesToBottom`).
#[test]
fn backspace_with_reverse_wrap_extend_wraps_from_top_to_bottom() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[?1045h\x1b[1;1H\x08");
    assert_eq!(g.cursor().col, 3);
    assert_eq!(g.cursor().row, 3);
}

/// `?45`: BS at col 0 of a continuation row retraces onto the
/// previous row (xterm 380+ arm of `test_BS_AfterOneWrappedInline`).
#[test]
fn backspace_with_reverse_wrap_inline_retraces_a_wrapped_line() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[?45habcde\r\x08");
    assert_eq!(g.cursor().col, 3);
    assert_eq!(g.cursor().row, 0);
}

/// `?45`: BS does not wrap from a row that never autowrapped
/// (`test_BS_InitialReverseWraparound`, xterm 380+).
#[test]
fn backspace_with_reverse_wrap_inline_stays_on_unwrapped_row() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[?45h\x1b[1;1H\x1bE\x08");
    assert_eq!(g.cursor().col, 0);
    assert_eq!(g.cursor().row, 1);
}

/// With `pending_wrap` set, BS under `?45` only clears the flag
/// (`test_DECSET_ReverseWraparoundLastCol_BS`).
#[test]
fn backspace_with_pending_wrap_and_reverse_wrap_is_a_noop() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[?45h\x1b[1;3Hab\x08");
    assert_eq!(g.cursor().col, 3);
    assert_eq!(g.cursor().row, 0);
}

/// CUB under `?1045` + DECAWM wraps through the previous row
/// (`test_DECSET_ReverseWraparound_Multi`).
#[test]
fn cub_with_reverse_wrap_extend_walks_back_through_previous_row() {
    let mut g = Grid::new(3, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[?1045h\x1b[2;1H\x1b[2D");
    assert_eq!(g.cursor().col, 2);
    assert_eq!(g.cursor().row, 0);
}

/// CUB under `?45` stops at the logical line's head (xterm 380+ arm
/// of `test_CUB_AfterOneWrappedInline`).
#[test]
fn cub_with_reverse_wrap_inline_stops_at_the_logical_line_head() {
    let mut g = Grid::new(3, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[?45habcdef\r\x1b[1C\x1b[20D");
    assert_eq!(g.cursor().col, 0);
    assert_eq!(g.cursor().row, 0);
}

/// CUB without reverse-wrap clamps at col 0 even with DECAWM
/// (`test_CUB_StopsAtLeftEdge`).
#[test]
fn cub_without_reverse_wrap_clamps_at_column_zero() {
    let mut g = Grid::new(3, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?7h\x1b[2;2H\x1b[5D");
    assert_eq!(g.cursor().col, 0);
    assert_eq!(g.cursor().row, 1);
}

/// DECRQM `?45` reads the functional `reverse_wrap_inline` field.
#[test]
fn decrqm_mode_45_reports_reverse_wraparound_state() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?45$p");
    let initial = responses(&mut g);
    assert_eq!(std::str::from_utf8(&initial[0]).unwrap(), "\x1b[?45;2$y");
    drive(&mut p, &mut g, b"\x1b[?45h\x1b[?45$p");
    let set = responses(&mut g);
    assert_eq!(std::str::from_utf8(&set[0]).unwrap(), "\x1b[?45;1$y");
}

/// ?45 and ?1045 carry distinct state since the xterm-2023 split:
/// setting one must not flip the other's DECRQM report.
#[test]
fn decrqm_modes_45_and_1045_are_independent() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?45h\x1b[?1045$p");
    let extend = responses(&mut g);
    assert_eq!(std::str::from_utf8(&extend[0]).unwrap(), "\x1b[?1045;2$y");
    drive(
        &mut p,
        &mut g,
        b"\x1b[?1045h\x1b[?45l\x1b[?45$p\x1b[?1045$p",
    );
    let both = responses(&mut g);
    assert_eq!(std::str::from_utf8(&both[0]).unwrap(), "\x1b[?45;2$y");
    assert_eq!(std::str::from_utf8(&both[1]).unwrap(), "\x1b[?1045;1$y");
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
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"ab\x1bVc\x1bW\x1b[1;1H\x1b[0J");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'c'));
    // An EL 2 turned no-op would keep the protected cell too and
    // still pass a protected-only oracle.
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"ab\x1bVc\x1bW\x1b[1;1H\x1b[2K");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'c'));
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"ab\x1bVc\x1bW\x1b[1;1H\x1b[3X");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'c'));
    let mut g = Grid::new(2, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"ab\x1b[1\"qc\x1b[0\"q\x1b[1;1H\x1b[0J");
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
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

/// DECXCPR (`?6n`) replies with the page number, always 1 (esctest's
/// `test_DECDSR_DECXCPR`).
#[test]
fn decdsr_decxcpr_replies_with_three_param_cursor_position() {
    let mut g = Grid::new(8, 16);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[6;5H\x1b[?6n");
    let responses = responses(&mut g);
    assert_eq!(std::str::from_utf8(&responses[0]).unwrap(), "\x1b[?6;5;1R");
}

/// DECMSR (`?62n`) and DECCKSR (`?63;Pid n`) report zero: felis has
/// no macro store.
#[test]
fn decdsr_macro_status_replies_with_zero_space_and_checksum() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?62n\x1b[?63;42n");
    let responses = responses(&mut g);
    assert_eq!(std::str::from_utf8(&responses[0]).unwrap(), "\x1b[0*{");
    assert_eq!(
        std::str::from_utf8(&responses[1]).unwrap(),
        "\x1bP42!~0000\x1b\\"
    );
}

/// `?75n` / `?85n` reply "OK" / "not configured", the xterm replies
/// esctest accepts.
#[test]
fn decdsr_integrity_and_session_replies_match_xterm() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?75n\x1b[?85n");
    let responses = responses(&mut g);
    assert_eq!(std::str::from_utf8(&responses[0]).unwrap(), "\x1b[?70n");
    assert_eq!(std::str::from_utf8(&responses[1]).unwrap(), "\x1b[?83n");
}

/// Locator DSRs report "no locator" (Ps=50) and type 0.
#[test]
fn decdsr_locator_replies_with_no_locator_attached() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?53n\x1b[?55n\x1b[?56n");
    let responses = responses(&mut g);
    assert_eq!(std::str::from_utf8(&responses[0]).unwrap(), "\x1b[?50n");
    assert_eq!(std::str::from_utf8(&responses[1]).unwrap(), "\x1b[?50n");
    assert_eq!(std::str::from_utf8(&responses[2]).unwrap(), "\x1b[?57;0n");
}

/// Mode 2027 always reports set; DECRESET does not move it.
#[test]
fn decrqm_dec_mode_2027_always_reports_set() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?2027$p");
    let initial = responses(&mut g);
    assert_eq!(std::str::from_utf8(&initial[0]).unwrap(), "\x1b[?2027;1$y");
    drive(&mut p, &mut g, b"\x1b[?2027l\x1b[?2027$p");
    let after_reset = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&after_reset[0]).unwrap(),
        "\x1b[?2027;1$y"
    );
}

/// Unknown DEC modes report Ps=0 rather than the soft-state default,
/// until a DECSET / DECRST writes them: from then on they are
/// soft-tracked and report the written state
/// (`docs/reference/protocols/vt-compliance.md` "Consumed without
/// effect").
#[test]
fn decrqm_dec_unknown_mode_reports_ps_zero_until_written() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?9999$p");
    let cold = responses(&mut g);
    assert_eq!(std::str::from_utf8(&cold[0]).unwrap(), "\x1b[?9999;0$y");

    drive(&mut p, &mut g, b"\x1b[?9999h\x1b[?9999$p");
    let set = responses(&mut g);
    assert_eq!(std::str::from_utf8(&set[0]).unwrap(), "\x1b[?9999;1$y");

    drive(&mut p, &mut g, b"\x1b[?9999l\x1b[?9999$p");
    let reset = responses(&mut g);
    assert_eq!(std::str::from_utf8(&reset[0]).unwrap(), "\x1b[?9999;2$y");
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

/// `DECSED 1` addresses rows through the ring base: after a scroll the
/// physical order is rotated and `logical * cols` disagrees.
#[test]
fn decsed_1_addresses_rows_through_the_ring_base_after_scroll() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[4;1H\n");
    assert_ne!(
        g.screen.base, 0,
        "scroll must advance the base, else this test cannot catch the bug"
    );
    for (row1, ch) in [(1u8, b'a'), (2, b'b'), (3, b'c'), (4, b'd')] {
        drive(&mut p, &mut g, format!("\x1b[{row1};1H").as_bytes());
        drive(&mut p, &mut g, &[ch, ch, ch, ch]);
    }
    drive(&mut p, &mut g, b"\x1b[2;2H\x1b[?1J");
    assert_eq!(logical_row(&g, 0), "    ", "logical row 0 fully erased");
    assert_eq!(
        logical_row(&g, 1),
        "  bb",
        "row 1 erased through the cursor"
    );
    assert_eq!(logical_row(&g, 2), "cccc", "row 2 untouched");
    assert_eq!(logical_row(&g, 3), "dddd", "row 3 untouched");
}

/// `DECSED 0` on a scrolled (rotated) grid.
#[test]
fn decsed_0_addresses_rows_through_the_ring_base_after_scroll() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[4;1H\n");
    assert_ne!(g.screen.base, 0, "scroll must advance the base");
    for (row1, ch) in [(1u8, b'a'), (2, b'b'), (3, b'c'), (4, b'd')] {
        drive(&mut p, &mut g, format!("\x1b[{row1};1H").as_bytes());
        drive(&mut p, &mut g, &[ch, ch, ch, ch]);
    }
    drive(&mut p, &mut g, b"\x1b[2;2H\x1b[?0J");
    assert_eq!(
        logical_row(&g, 0),
        "aaaa",
        "row 0 untouched (before the cursor)"
    );
    assert_eq!(
        logical_row(&g, 1),
        "b   ",
        "row 1 erased from the cursor to EOL"
    );
    assert_eq!(logical_row(&g, 2), "    ", "row 2 fully erased");
    assert_eq!(logical_row(&g, 3), "    ", "row 3 fully erased");
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

fn logical_row(g: &Grid, row: u16) -> String {
    (0..g.screen.cols)
        .map(|c| match g.cell(row, c).unwrap().grapheme {
            Grapheme::Ascii(b) => char::from(b),
            Grapheme::Empty => ' ',
            _ => '?',
        })
        .collect()
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

/// Each `xterm_window_op` report arm emits its fixed wire bytes.
#[test]
fn xterm_window_op_report_arms_emit_their_fixed_replies() {
    let mut g = Grid::new(8, 10);
    let mut p = Parser::new();
    let reply = |g: &mut Grid, p: &mut Parser, seq: &[u8]| -> String {
        drive(p, g, seq);
        let r = responses(g);
        String::from_utf8(r.into_iter().next().unwrap_or_default()).unwrap()
    };
    assert_eq!(reply(&mut g, &mut p, b"\x1b[11t"), "\x1b[1t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[13t"), "\x1b[3;0;0t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[14t"), "\x1b[4;0;0t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[15t"), "\x1b[5;0;0t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[16t"), "\x1b[6;0;0t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[19t"), "\x1b[9;8;10t");
}

/// `CSI 20 t` / `CSI 21 t` report icon name / window title with the
/// `L` / `l` marker.
#[test]
fn xterm_window_op_reports_icon_and_window_title() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b]2;WIN\x1b\\\x1b]1;ICON\x1b\\");
    drive(&mut p, &mut g, b"\x1b[21t");
    let r = responses(&mut g);
    assert_eq!(
        String::from_utf8(r.into_iter().next().unwrap()).unwrap(),
        "\x1b]lWIN\x1b\\"
    );
    drive(&mut p, &mut g, b"\x1b[20t");
    let r = responses(&mut g);
    assert_eq!(
        String::from_utf8(r.into_iter().next().unwrap()).unwrap(),
        "\x1b]LICON\x1b\\"
    );
}

/// DECRQSS ` q` reports the DECSCUSR Ps that produced the cursor
/// shape.
#[test]
fn decrqss_space_q_reports_cursor_shape() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[4 q\x1bP$q q\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        String::from_utf8(r.into_iter().next().unwrap()).unwrap(),
        "\x1bP1$r4 q\x1b\\"
    );
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

/// A combining mark after a width-2 print folds into the wide owner,
/// not the Spacer.
#[test]
fn combining_mark_attaches_to_wide_glyph_owner_not_spacer() {
    let mut g = Grid::new(2, 6);
    let mut p = Parser::new();
    drive(&mut p, &mut g, "世".as_bytes());
    assert_eq!(g.cursor().col, 2);
    drive(&mut p, &mut g, "\u{0301}".as_bytes());
    assert!(
        matches!(g.cell(0, 0).unwrap().grapheme, Grapheme::Cluster(_)),
        "combining mark must uplevel the wide owner to a Cluster"
    );
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
}

/// A mark stream stops folding at `ClusterText::CAP`: the cell keeps
/// the last cluster that fit and the table stops growing.
#[test]
fn a_mark_stream_stops_folding_at_the_cluster_cap() {
    let mut g = Grid::new(1, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"a");
    // U+0301 is two UTF-8 bytes, so base + 63 marks lands exactly on
    // the 128-byte cap.
    let marks = "\u{0301}".repeat(4096);
    drive(&mut p, &mut g, marks.as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("the marks that did fit must leave a Cluster");
    };
    let text = g.cluster_str(id).expect("resolvable");
    assert!(text.len() <= ClusterText::CAP, "held {} bytes", text.len());
    assert_eq!(text.chars().count(), 64, "base + 63 marks fills the cap");
    assert_eq!(g.cluster_count(), 63);
}

/// A refused trailing ZWJ must not arm the GB11 continuation, or the
/// cap would become a way to blank the rest of the line.
#[test]
fn a_refused_zwj_does_not_swallow_the_next_base_character() {
    let mut g = Grid::new(1, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, "👨".as_bytes());
    drive(&mut p, &mut g, "\u{200D}".repeat(200).as_bytes());
    let before = g.cluster_count();
    drive(&mut p, &mut g, b"X");
    assert_eq!(
        g.cluster_count(),
        before,
        "the base after a refused ZWJ must not extend the cluster"
    );
    let printed = (0..8)
        .filter_map(|c| match g.cell(0, c).unwrap().grapheme {
            Grapheme::Ascii(b) => Some(char::from(b)),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(printed, "X", "the base must land in its own cell");
}

/// The longest real emoji ZWJ sequence still folds into one cluster.
#[test]
fn the_longest_real_emoji_sequence_still_folds() {
    let mut g = Grid::new(1, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        "👨\u{200D}👩\u{200D}👧\u{200D}👦".as_bytes(),
    );
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("the family sequence must fold into one cluster");
    };
    assert_eq!(g.cluster_str(id), Some("👨\u{200D}👩\u{200D}👧\u{200D}👦"));
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

/// An interior DECSTBM scroll is a band rotation: no cell content
/// moves physically, and every logical row reads back through the
/// band.
#[test]
fn interior_region_scroll_reads_back_through_the_band() {
    let mut g = Grid::new(5, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    drive(&mut p, &mut g, b"\x1b[2;4r\x1b[4;1H\n");
    assert_eq!(g.screen.band_len, 3, "region scroll must arm the band");
    assert_eq!(g.screen.band_top, 1);
    assert_eq!(g.screen.band_rot, 1);
    assert_eq!(logical_row(&g, 0), "aaaa", "above the region: untouched");
    assert_eq!(logical_row(&g, 1), "cccc");
    assert_eq!(logical_row(&g, 2), "dddd");
    assert_eq!(logical_row(&g, 3), "    ", "recycled region bottom blanks");
    assert_eq!(logical_row(&g, 4), "eeee", "below the region: untouched");
    drive(&mut p, &mut g, b"\x1b[4;1HXXXX\n");
    assert_eq!(g.screen.band_rot, 2);
    assert_eq!(logical_row(&g, 1), "dddd");
    assert_eq!(logical_row(&g, 2), "XXXX", "the print rode the rotation");
    assert_eq!(logical_row(&g, 3), "    ");
}

/// RI at the region top rotates the band back; equal up and down
/// scrolls cancel in `band_rot`.
#[test]
fn region_scroll_down_rotates_the_band_back() {
    let mut g = Grid::new(5, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    drive(&mut p, &mut g, b"\x1b[2;4r\x1b[4;1H\n");
    drive(&mut p, &mut g, b"\x1b[2;1H\x1bM");
    assert_eq!(g.screen.band_rot, 0, "up then down cancels the rotation");
    assert_eq!(logical_row(&g, 1), "    ", "down-scroll blanks the top");
    assert_eq!(logical_row(&g, 2), "cccc");
    assert_eq!(logical_row(&g, 3), "dddd");
    assert_eq!(logical_row(&g, 4), "eeee");
}

/// Changing the scroll region while a band is rotated materializes
/// the active band first, or the two rotations would compose.
#[test]
fn region_change_materializes_the_previous_band() {
    let mut g = Grid::new(6, 4);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee\r\nffff",
    );
    drive(&mut p, &mut g, b"\x1b[2;5r\x1b[5;1H\n");
    assert_eq!(g.screen.band_rot, 1);
    drive(&mut p, &mut g, b"\x1b[3;1H\x1b[1L");
    assert_eq!(logical_row(&g, 0), "aaaa");
    assert_eq!(logical_row(&g, 1), "cccc");
    assert_eq!(logical_row(&g, 2), "    ", "IL's inserted blank");
    assert_eq!(logical_row(&g, 3), "dddd");
    assert_eq!(logical_row(&g, 4), "eeee", "the pre-IL blank fell off");
    assert_eq!(logical_row(&g, 5), "ffff");
}

/// A full-screen scroll into history after an interior band rotation
/// materializes first, so the evicted row is the logical top row.
#[test]
fn full_screen_scroll_after_interior_band_materializes_first() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    drive(&mut p, &mut g, b"\x1b[2;3r\x1b[3;1H\n");
    assert_ne!(g.screen.band_rot, 0);
    drive(&mut p, &mut g, b"\x1b[r\x1b[4;1H\n");
    assert_eq!(g.screen.band_len, 0, "the base bump must drop the band");
    assert_eq!(g.scrollback().len(), 1);
    let hist: Vec<Cell> = g.scrollback().row(0).unwrap().to_vec();
    let hist_text: String = hist
        .iter()
        .map(|c| match c.grapheme {
            Grapheme::Ascii(b) => char::from(b),
            _ => ' ',
        })
        .collect();
    assert_eq!(hist_text, "aaaa", "the logical top row was evicted");
    assert_eq!(logical_row(&g, 0), "cccc", "band content materialized");
    assert_eq!(logical_row(&g, 1), "    ");
    assert_eq!(logical_row(&g, 2), "dddd");
    assert_eq!(logical_row(&g, 3), "    ", "blank from the full scroll");
}

/// Full-screen RI with retained history takes the band path (a base
/// bump would resurrect history rows); history stays intact beneath.
#[test]
fn full_screen_ri_with_history_rotates_a_viewport_band() {
    let mut g = Grid::new(3, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    assert_eq!(g.scrollback().len(), 2);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1bM");
    assert_eq!(g.screen.band_len, 3, "history forces the band path");
    assert_eq!(logical_row(&g, 0), "    ");
    assert_eq!(logical_row(&g, 1), "cccc");
    assert_eq!(logical_row(&g, 2), "dddd");
    assert_eq!(g.scrollback().len(), 2, "history untouched by the band");
    drive(&mut p, &mut g, b"\x1b[3;1H\n");
    assert_eq!(g.scrollback().len(), 3);
    assert_eq!(logical_row(&g, 0), "cccc");
    assert_eq!(logical_row(&g, 1), "dddd");
}

/// Entering the alt screen with a rotated band materializes the
/// primary first, since the snapshot records `base` but no band.
#[test]
fn alt_screen_roundtrip_preserves_band_rotated_content() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    drive(&mut p, &mut g, b"\x1b[2;3r\x1b[3;1H\n");
    assert_ne!(g.screen.band_rot, 0);
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert_eq!(g.screen.band_len, 0, "the snapshot must not carry a band");
    drive(&mut p, &mut g, b"\x1b[2;3r\x1b[3;1HZZZZ\n\x1b[?1049l");
    assert_eq!(logical_row(&g, 0), "aaaa");
    assert_eq!(logical_row(&g, 1), "cccc", "rotation survived the trip");
    assert_eq!(logical_row(&g, 2), "    ");
    assert_eq!(logical_row(&g, 3), "dddd");
}

/// A resize with a rotated band keeps the logical content and drops
/// the band.
#[test]
fn resize_after_band_rotation_keeps_logical_content() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    drive(&mut p, &mut g, b"\x1b[2;3r\x1b[3;1H\n");
    assert_ne!(g.screen.band_rot, 0);
    g.resize(4, 6);
    assert_eq!(g.screen.band_len, 0, "resize must drop the band");
    assert_eq!(logical_row(&g, 0), "aaaa  ");
    assert_eq!(logical_row(&g, 1), "cccc  ");
    assert_eq!(logical_row(&g, 2), "      ");
    assert_eq!(logical_row(&g, 3), "dddd  ");
}

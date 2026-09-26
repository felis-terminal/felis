//! Per-CSI-handler snapshots: one per CSI group the
//! [support matrix](../../../docs/reference/protocols/support-matrix.md)
//! "VT / ANSI" tables mark supported (`docs/reference/testing.md`
//! "Snapshot tests").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use felis_grid::{Color, Grid};

mod common;
use common::{drive, drive_take, write_cells, write_cursor, write_dirty_rows, write_pen};

fn render(grid: &Grid) -> String {
    let mut out = String::new();
    write_cursor(&mut out, grid);
    write_pen(&mut out, grid);
    writeln!(
        out,
        "modes: alt={} bracket_paste={} focus={} sync={} mouse={:?}/{:?} kitty=0x{:02x}",
        grid.on_alternate_screen() as u8,
        grid.bracketed_paste() as u8,
        grid.focus_reporting() as u8,
        grid.synchronized_output() as u8,
        grid.mouse_protocol(),
        grid.mouse_encoding(),
        grid.kitty_kbd_flags().bits(),
    )
    .unwrap();
    write_cells(&mut out, grid);
    write_dirty_rows(&mut out, grid);
    out
}

fn debug_bytes(b: &[u8]) -> String {
    let mut s = String::new();
    for &byte in b {
        if byte == 0x1b {
            s.push_str("\\e");
        } else if (0x20..0x7f).contains(&byte) {
            s.push(byte as char);
        } else {
            write!(s, "\\x{byte:02x}").unwrap();
        }
    }
    s
}

#[test]
fn cursor_positioning_handlers_compose() {
    let g = drive(3, 8, b"\x1b[2;3H\x1b[1C\x1b[1B\x1b[1A\x1b[6G\x1b[2dX");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=6 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 3x8:
    |........|
    |.....X..|
    |........|
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn erase_handlers_blank_their_target_regions() {
    let g = drive(3, 8, b"row1aaaa\r\nrow2bbbb\r\nrow3cccc\x1b[2;5H\x1b[1K");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=4 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 3x8:
    |row1aaaa|
    |.....bbb|
    |row3cccc|
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn ech_blanks_n_cells_without_moving_cursor() {
    let g = drive(1, 8, b"abcdefgh\r\x1b[2C\x1b[3X");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 1x8:
    |ab...fgh|
    dirty_rows: [0]
    ");
}

#[test]
fn il_dl_shift_rows_inside_the_screen() {
    let g = drive(3, 4, b"AAAA\r\nBBBB\r\nCCCC\x1b[1;1H\x1b[L");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 3x4:
    |....|
    |AAAA|
    |BBBB|
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn su_sd_scroll_the_whole_screen() {
    let g = drive(3, 4, b"AAAA\r\nBBBB\r\nCCCC\x1b[S\x1b[2;1H");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 3x4:
    |BBBB|
    |CCCC|
    |....|
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn private_modes_compose_in_one_dispatch_and_take_effect() {
    let g = drive(1, 1, b"\x1b[?25;2004;1004;2026;1000;1006h");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=1 focus=1 sync=1 mouse=ButtonEvents/Sgr kitty=0x00
    grid 1x1:
    |.|
    dirty_rows: [0]
    ");
}

#[test]
fn decsc_decrc_save_and_restore_cursor_and_pen() {
    let g = drive(3, 8, b"\x1b[2;3H\x1b[1;31m\x1b7\x1b[1;1H\x1b[0m\x1b8X");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=3 visible=1 pending_wrap=0
    pen: fg=idx1 bg=default flags=B
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 3x8:
    |........|
    |..X.....|
    |........|
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn decstr_soft_resets_modes_and_pen_keeps_cells() {
    // The cursor stays where the producer left it rather than homing:
    // xterm's behavior, relied on by esctest's `test_*_Reset`.
    let g = drive(2, 4, b"\x1b[1;31mhi\x1b[?2004h\x1b[?1004h\x1b[!p");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 2x4:
    |hi..|
    |....|
    dirty_rows: [0, 1]
    ");
}

#[test]
fn kitty_kbd_push_set_pop_walks_the_stack() {
    let g = drive(1, 1, b"\x1b[>1u\x1b[=4;2u\x1b[>2u\x1b[<u");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x05
    grid 1x1:
    |.|
    dirty_rows: [0]
    ");
}

#[test]
fn queries_emit_replies_into_the_response_queue() {
    let (_g, replies) = drive_take(2, 4, b"\x1b[c\x1b[5n\x1b[6n\x1b[?u");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"
    \e[?64;1;2;6;9;15;16;17;18;21;22;28;29c
    \e[0n
    \e[1;1R
    \e[?0u
    ");
}

#[test]
fn osc_color_query_replies_in_xterm_byte_replicated_form() {
    let (_g, replies) = drive_take(
        1,
        1,
        b"\x1b]10;#11ee99\x07\x1b]11;#3355aa\x07\x1b]12;#ff7700\x07\
          \x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07",
    );
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"
    \e]10;rgb:1111/eeee/9999\e\
    \e]11;rgb:3333/5555/aaaa\e\
    \e]12;rgb:ffff/7777/0000\e\
    ");
}

#[test]
fn osc_color_query_replies_with_grid_default_when_no_override() {
    // xterm always replies to `OSC 10/11/12 ; ?`; silence times out
    // esctest's `test_ResetSpecialColor_Dynamic`.
    let (_g, replies) = drive_take(1, 1, b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"
    \e]10;rgb:0000/0000/0000\e\
    \e]11;rgb:ffff/ffff/ffff\e\
    \e]12;rgb:0000/0000/0000\e\
    ");
}

#[test]
fn decstbm_scrolls_only_inside_the_region() {
    // vttest "Test of cursor movements" §3 uses this pattern.
    let g = drive(
        5,
        4,
        b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\r\nEEEE\
          \x1b[2;4r\x1b[4;1H\n\n",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=3 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 5x4:
    |AAAA|
    |DDDD|
    |....|
    |....|
    |EEEE|
    dirty_rows: [0, 1, 2, 3, 4]
    ");
}

#[test]
fn decstbm_reset_returns_to_full_screen() {
    // DECSTBM homes the cursor per VT220.
    let g = drive(4, 4, b"\x1b[2;3r\x1b[r");
    let c = g.cursor();
    assert_eq!((c.row, c.col), (0, 0));
}

#[test]
fn decom_makes_cup_address_region_relative() {
    let g = drive(5, 4, b"\x1b[2;4r\x1b[?6h\x1b[1;1H*\x1b[9;9H*");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=3 col=3 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 5x4:
    |....|
    |*...|
    |....|
    |...*|
    |....|
    dirty_rows: [0, 1, 2, 3, 4]
    ");
}

#[test]
fn decawm_off_pins_writes_at_the_right_margin() {
    let g = drive(2, 4, b"\x1b[?7lABCDEFG");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=3 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 2x4:
    |ABCG|
    |....|
    dirty_rows: [0, 1]
    ");
}

#[test]
fn ri_at_top_of_region_scrolls_region_down() {
    let g = drive(
        5,
        4,
        b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\r\nEEEE\x1b[2;4r\x1b[2;1H\x1bM",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 5x4:
    |AAAA|
    |....|
    |BBBB|
    |CCCC|
    |EEEE|
    dirty_rows: [0, 1, 2, 3, 4]
    ");
}

#[test]
fn cht_and_cbt_navigate_implicit_tab_stops() {
    let g = drive(1, 30, b"\x1b[3G\x1b[2I\x1b[Z");
    let c = g.cursor();
    assert_eq!(c.col, 8);
}

#[test]
fn s8c1t_emits_responses_with_c1_bytes() {
    let (_g, replies) = drive_take(1, 1, b"\x1b G\x1b[6n");
    let bytes = replies.into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(bytes, vec![0x9B, b'1', b';', b'1', b'R']);

    let (_g, replies) = drive_take(1, 1, b"\x1b G\x1b F\x1b[6n");
    let bytes = replies.into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(bytes, b"\x1b[1;1R".to_vec());
}

#[test]
fn decrqss_reports_current_sgr_pen() {
    // xterm prefixes the "0" reset so the re-emitted payload restores
    // the pen without prior SGR accumulation (esctest `test_DECRQSS_SGR`).
    let (_g, replies) = drive_take(1, 1, b"\x1bP$qm\x1b\\");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\eP1$r0m\e\");

    let (_g, replies) = drive_take(1, 1, b"\x1b[1;31m\x1bP$qm\x1b\\");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\eP1$r0;1;31m\e\");

    // SGR 58 has no 16-color short form; xterm uses the colon sub-params.
    let (_g, replies) = drive_take(1, 1, b"\x1b[4:3;58:2::10:20:30m\x1bP$qm\x1b\\");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\eP1$r0;4:3;58:2::10:20:30m\e\");

    let (_g, replies) = drive_take(1, 1, b"\x1b[58:5:196m\x1bP$qm\x1b\\");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\eP1$r0;58:5:196m\e\");
}

#[test]
fn decrqss_reports_scroll_region_and_unknown_invalid() {
    let (_g, replies) = drive_take(20, 80, b"\x1b[5;10r\x1bP$qr\x1b\\");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\eP1$r5;10r\e\");

    let (_g, replies) = drive_take(1, 1, b"\x1bP$qxyz\x1b\\");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\eP0$r\e\");
}

#[test]
fn spa_epa_protect_unprotect_via_ecma48_aliases() {
    let g = drive(1, 6, b"\x1bVAB\x1bWCD\x1b[?2K");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 1x6:
    |AB....|
    dirty_rows: [0]
    ");
}

#[test]
fn title_push_pop_round_trips() {
    let g = drive(
        1,
        1,
        b"\x1b]0;first\x07\x1b[22;0t\x1b]0;second\x07\x1b[23;0t",
    );
    assert_eq!(g.title(), Some("first"));
}

#[test]
fn title_stack_ps2_restores_only_window() {
    let (g, queued) = drive_take(
        1,
        1,
        b"\x1b]0;shared\x07\x1b[22;0t\x1b]1;new-icon\x07\x1b]2;new-window\x07\x1b[23;2t\x1b[20t",
    );
    assert_eq!(g.title(), Some("shared"));
    assert_eq!(
        String::from_utf8(queued.into_iter().next().unwrap()).unwrap(),
        "\x1b]Lnew-icon\x1b\\",
        "Ps=2 pop must not restore the icon",
    );
}

#[test]
fn title_pop_on_empty_stack_is_a_noop() {
    let g = drive(1, 1, b"\x1b]0;abc\x07\x1b[23;0t");
    assert_eq!(g.title(), Some("abc"));
}

#[test]
fn bce_carries_pen_bg_through_ech_ed_el() {
    // vttest's BCE menu (11.6.4 / 11.6.5) exercises this path.
    let g = drive(1, 6, b"\x1b[44mABC\r\x1b[5X");
    let cells: Vec<_> = (0..5)
        .map(|c| {
            let cell = g.cell(0, c).unwrap();
            g.style(cell.style).bg
        })
        .collect();
    for bg in &cells {
        assert!(
            matches!(bg, Color::Indexed(4)),
            "ECH-blanked cell missed BCE: got {bg:?}"
        );
    }
}

#[test]
fn decsca_decsel_skip_protected_cells() {
    let g = drive(1, 8, b"\x1b[1\"qAB\x1b[0\"qCD\x1b[?2K");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 1x8:
    |AB......|
    dirty_rows: [0]
    ");
}

#[test]
fn decsed_zero_keeps_protected_cells_after_cursor() {
    let g = drive(
        2,
        4,
        b"\x1b[1\"qAB\x1b[0\"qCD\r\n\x1b[1\"qXY\x1b[0\"qzz\x1b[1;1H\x1b[?0J",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 2x4:
    |AB..|
    |XY..|
    dirty_rows: [0, 1]
    ");
}

#[test]
fn sl_sr_shift_region_columns() {
    let g = drive(2, 5, b"ABCDE\r\nABCDE\x1b[2 @");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=4 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 2x5:
    |CDE..|
    |CDE..|
    dirty_rows: [0, 1]
    ");

    let g = drive(2, 5, b"ABCDE\r\nABCDE\x1b[1 A");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=4 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 2x5:
    |.ABCD|
    |.ABCD|
    dirty_rows: [0, 1]
    ");
}

#[test]
fn rep_repeats_last_graphic_n_times() {
    let g = drive(1, 10, b"A\x1b[4b");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 1x10:
    |AAAAA.....|
    dirty_rows: [0]
    ");
}

#[test]
fn rep_after_a_control_dispatch_is_a_noop() {
    let g = drive(1, 10, b"A\x1b[H\x1b[3b");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 1x10:
    |A.........|
    dirty_rows: [0]
    ");
}

#[test]
fn cnl_cpl_move_to_column_zero_of_the_target_row() {
    let g = drive(5, 10, b"\x1b[3;5H\x1b[2E");
    let c = g.cursor();
    assert_eq!((c.row, c.col), (4, 0));

    let g = drive(5, 10, b"\x1b[3;5H\x1b[1F");
    let c = g.cursor();
    assert_eq!((c.row, c.col), (1, 0));
}

#[test]
fn hpa_hpr_vpr_alias_cursor_motions() {
    let g = drive(3, 10, b"\x1b[5`\x1b[2a\x1b[1e");
    let c = g.cursor();
    assert_eq!((c.row, c.col), (1, 6));
}

#[test]
fn da2_replies_with_secondary_device_attributes() {
    // VT420 family (41) per docs/reference/terminal-identity.md; firmware
    // 400 sits inside esctest's [314, 999] acceptance range.
    let (_g, replies) = drive_take(1, 1, b"\x1b[>c");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\e[>41;400;0c");
}

#[test]
fn irm_shifts_cells_right_on_print() {
    let g = drive(1, 8, b"ABCDE\x1b[2G\x1b[4hX");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 1x8:
    |AXBCDE..|
    dirty_rows: [0]
    ");
}

#[test]
fn decrqm_reports_current_mode_state() {
    let (_g, replies) = drive_take(1, 1, b"\x1b[?7$p\x1b[4$p");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"
    \e[?7;1$y
    \e[4;2$y
    ");
}

#[test]
fn decrqm_reports_modifiable_for_deccolm_permanent_reset_for_decarm() {
    // DECARM must report Ps=4: esctest's `decrqm.py` marks
    // `test_DECRQM_DEC_DECARM` `@knownBug`, so any other reply trips
    // "Should have failed".
    let (_g, replies) = drive_take(1, 1, b"\x1b[?3$p\x1b[?8$p");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"
    \e[?3;2$y
    \e[?8;4$y
    ");
}

#[test]
fn decrqm_unknown_mode_replies_zero() {
    let (_g, replies) = drive_take(1, 1, b"\x1b[?9999$p");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\e[?9999;0$y");
}

#[test]
fn hts_then_tab_walks_a_custom_stop() {
    let g = drive(1, 10, b"\x1b[3g\x1b[6G\x1bH\x1b[1G\t\t");
    let c = g.cursor();
    assert_eq!(c.col, 9);
}

#[test]
fn tbc_zero_clears_only_the_cursor_column() {
    let g = drive(1, 30, b"\x1b[9G\x1b[g\x1b[1G\t");
    let c = g.cursor();
    assert_eq!(c.col, 16);
}

#[test]
fn cbt_walks_backwards_through_set_stops() {
    let g = drive(1, 30, b"\x1b[21G\x1b[2Z");
    let c = g.cursor();
    assert_eq!(c.col, 8);
}

#[test]
fn decscnm_toggle_dirties_whole_screen() {
    let g = drive(2, 4, b"\x1b[?5h");
    assert!(g.reverse_video());
    assert_eq!(g.damage().dirty_rows().collect::<Vec<_>>(), vec![0, 1]);
    let g_off = drive(2, 4, b"\x1b[?5h\x1b[?5l");
    assert!(!g_off.reverse_video());
}

#[test]
fn irm_toggle_tracks_insert_mode() {
    let g_on = drive(1, 4, b"\x1b[4h");
    assert!(g_on.insert_mode());
    let g_off = drive(1, 4, b"\x1b[4h\x1b[4l");
    assert!(!g_off.insert_mode());
}

#[test]
fn s8c1t_s7c1t_toggle_tracks_c1_8bit() {
    let g_default = drive(1, 4, b"");
    assert!(!g_default.c1_8bit());
    let g_8bit = drive(1, 4, b"\x1b G");
    assert!(g_8bit.c1_8bit());
    let g_7bit = drive(1, 4, b"\x1b G\x1b F");
    assert!(!g_7bit.c1_8bit());
}

#[test]
fn deckpam_deckpnm_toggle_application_keypad() {
    let g_on = drive(1, 1, b"\x1b=");
    assert!(g_on.application_keypad());
    let g_off = drive(1, 1, b"\x1b=\x1b>");
    assert!(!g_off.application_keypad());
}

#[test]
fn decreqtparm_replies_with_vt100_defaults() {
    let (_g, replies) = drive_take(1, 1, b"\x1b[0x\x1b[1x");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"
    \e[2;1;1;120;120;1;0x
    \e[3;1;1;120;120;1;0x
    ");
}

#[test]
fn decaln_fills_screen_with_e_and_homes_cursor() {
    let g = drive(2, 4, b"\x1b[31m\x1b#8");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=0 visible=1 pending_wrap=0
    pen: fg=idx1 bg=default flags=-
    modes: alt=0 bracket_paste=0 focus=0 sync=0 mouse=Off/Default kitty=0x00
    grid 2x4:
    |EEEE|
    |EEEE|
    dirty_rows: [0, 1]
    ");
}

#[test]
fn decid_alias_replies_with_da1() {
    let (_g, replies) = drive_take(1, 1, b"\x1bZ");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\e[?64;1;2;6;9;15;16;17;18;21;22;28;29c");
}

#[test]
fn xterm_size_in_cells_reports_grid_dimensions() {
    let (_g, replies) = drive_take(24, 80, b"\x1b[18t");
    let pretty: Vec<String> = replies.iter().map(|r| debug_bytes(r)).collect();
    insta::assert_snapshot!(pretty.join("\n"), @r"\e[8;24;80t");
}

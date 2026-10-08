use super::*;
use felis_vt::Parser;
use proptest::prelude::*;

use crate::test_support::{drive, responses};

/// The daemon's `compose_diffs` / rehydrate paths borrow the row to skip
/// the per-row clone; the contents must match a per-cell walk.
#[test]
fn row_cells_returns_the_same_contents_as_per_cell_walk() {
    let mut g = Grid::new(3, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdefgh\r\nABCDEFGH\r\n12345678");
    for r in 0..g.rows() {
        let walked: Vec<_> = (0..g.cols())
            .map(|c| g.cell(r, c).copied().unwrap_or_default())
            .collect();
        let slice = g.row_cells(r).expect("in-range row");
        assert_eq!(slice.len(), walked.len());
        for (a, b) in slice.iter().zip(walked.iter()) {
            assert_eq!(a, b, "row {r}");
        }
    }
}

#[test]
fn row_cells_returns_none_for_out_of_range_rows() {
    let g = Grid::new(2, 4);
    assert!(g.row_cells(2).is_none());
    assert!(g.row_cells(u16::MAX).is_none());
}

#[test]
fn fresh_grid_has_default_cells_and_damage_marked() {
    let g = Grid::new(3, 5);
    assert_eq!(g.rows(), 3);
    assert_eq!(g.cols(), 5);
    for r in 0..3 {
        for c in 0..5 {
            assert!(g.cell(r, c).unwrap().is_blank());
        }
    }
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![0, 1, 2]);
}

#[test]
fn print_ascii_advances_cursor_and_marks_row() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 5);
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"hi");
    assert_eq!(
        g.cursor(),
        Cursor {
            row: 0,
            col: 2,
            visible: true,
            pending_wrap: false,
        }
    );
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'h'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'i'));
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![0]);
}

#[test]
fn cr_lf_moves_cursor_to_next_line() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    drive(&mut p, &mut g, b"a\r\nb");
    assert_eq!(
        g.cursor(),
        Cursor {
            row: 1,
            col: 1,
            visible: true,
            pending_wrap: false,
        }
    );
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'b'));
}

#[test]
fn backspace_moves_cursor_left() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 5);
    drive(&mut p, &mut g, b"abc\x08X");
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'X'));
    assert_eq!(g.cursor().col, 3);
}

#[test]
fn cup_jumps_cursor() {
    let mut p = Parser::new();
    let mut g = Grid::new(5, 10);
    drive(&mut p, &mut g, b"\x1b[3;7HX");
    assert_eq!(g.cell(2, 6).unwrap().grapheme, Grapheme::Ascii(b'X'));
    assert_eq!(g.cursor().col, 7);
    assert_eq!(g.cursor().row, 2);
}

#[test]
fn el_clears_to_end_of_line() {
    let mut g = Grid::new(2, 6);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdef\r\x1b[3C"); // move forward 3
    drive(&mut p, &mut g, b"\x1b[K"); // EL 0
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'c'));
    assert!(g.cell(0, 3).unwrap().is_blank());
    assert!(g.cell(0, 5).unwrap().is_blank());
}

#[test]
fn bel_byte_sets_pending_flag_and_take_clears_it() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x07");
    assert!(g.take_bell_pending(), "BEL should set the flag");
    assert!(
        !g.take_bell_pending(),
        "second take must return false (drained)"
    );
}

#[test]
fn multiple_bels_in_one_chunk_coalesce_to_one_pending_event() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x07\x07\x07");
    assert!(g.take_bell_pending());
    assert!(!g.take_bell_pending());
}

#[test]
fn bel_does_not_alter_cells_or_cursor() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"a\x07b");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn kitty_kbd_default_flags_zero_when_no_program_pushed() {
    let g = Grid::new(1, 1);
    assert_eq!(g.kitty_kbd_flags(), KittyKbdFlags::empty());
    assert_eq!(g.kitty_kbd_stack_depth(), 0);
}

#[test]
fn kitty_kbd_push_promotes_top_of_stack_and_masks_unknown_bits() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>255u");
    assert_eq!(g.kitty_kbd_flags().bits(), 0x1F);
    assert_eq!(g.kitty_kbd_stack_depth(), 1);
    drive(&mut p, &mut g, b"\x1b[>1u");
    assert_eq!(g.kitty_kbd_flags().bits(), 0b1);
    assert_eq!(g.kitty_kbd_stack_depth(), 2);
}

#[test]
fn kitty_kbd_pop_unwinds_the_stack_and_caps_at_zero() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>1u\x1b[>3u\x1b[>5u");
    assert_eq!(g.kitty_kbd_flags().bits(), 5);
    drive(&mut p, &mut g, b"\x1b[<u");
    assert_eq!(g.kitty_kbd_flags().bits(), 3);
    drive(&mut p, &mut g, b"\x1b[<99u");
    assert_eq!(g.kitty_kbd_flags(), KittyKbdFlags::empty());
    assert_eq!(g.kitty_kbd_stack_depth(), 0);
}

#[test]
fn kitty_kbd_apply_modes_replace_or_clear() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>5u");
    drive(&mut p, &mut g, b"\x1b[=2;1u");
    assert_eq!(g.kitty_kbd_flags().bits(), 2);
    drive(&mut p, &mut g, b"\x1b[=1;2u");
    assert_eq!(g.kitty_kbd_flags().bits(), 3);
    drive(&mut p, &mut g, b"\x1b[=1;3u");
    assert_eq!(g.kitty_kbd_flags().bits(), 2);
}

#[test]
fn kitty_kbd_apply_with_empty_stack_pushes_a_fresh_entry() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[=5;1u");
    assert_eq!(g.kitty_kbd_flags().bits(), 5);
    assert_eq!(g.kitty_kbd_stack_depth(), 1);
}

#[test]
fn kitty_kbd_apply_clear_against_empty_stack_is_a_noop() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[=1;3u");
    assert_eq!(g.kitty_kbd_stack_depth(), 0);
}

#[test]
fn da1_query_responds_with_xterm_vt420_default_feature_mask() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    // `CSI c` and `CSI 0 c` are equivalent.
    drive(&mut p, &mut g, b"\x1b[c");
    assert_eq!(responses(&mut g), vec![DA1_REPLY.to_vec()]);
    drive(&mut p, &mut g, b"\x1b[0c");
    assert_eq!(responses(&mut g), vec![DA1_REPLY.to_vec()]);
}

/// DECID is the legacy alias for DA1; terminfo probes expect the reply
/// byte-for-byte identical.
#[test]
fn decid_esc_z_replies_identically_to_da1() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1bZ");
    assert_eq!(responses(&mut g), vec![DA1_REPLY.to_vec()]);
}

/// Pp=41 per `docs/explanation/protocols/vt-compliance.md` "Reply
/// identity"; Pv=400 sits inside esctest's `[314, 999]` acceptance
/// range.
#[test]
fn da2_query_responds_with_vt420_model_and_in_range_version() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>c");
    assert_eq!(responses(&mut g), vec![b"\x1b[>41;400;0c".to_vec()]);
    // `CSI > 0 c` is the form esccmd's `DA2(0)` emits.
    drive(&mut p, &mut g, b"\x1b[>0c");
    assert_eq!(responses(&mut g), vec![b"\x1b[>41;400;0c".to_vec()]);
}

/// `docs/reference/protocols/vt-compliance.md` "Reporting and queries":
/// the hex of "felis-1", not xterm's all-zeroes placeholder.
#[test]
fn da3_query_responds_with_the_felis_unit_id() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[=0c");
    assert_eq!(
        responses(&mut g),
        vec![b"\x1bP!|66656c69732d31\x1b\\".to_vec()]
    );
    drive(&mut p, &mut g, b"\x1b[=c");
    assert_eq!(
        responses(&mut g),
        vec![b"\x1bP!|66656c69732d31\x1b\\".to_vec()]
    );
}

#[test]
fn dsr_5n_replies_ok() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[5n");
    assert_eq!(responses(&mut g), vec![b"\x1b[0n".to_vec()]);
}

#[test]
fn dsr_6n_replies_with_the_one_based_cursor_position() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"\x1b[3;5H\x1b[6n");
    assert_eq!(responses(&mut g), vec![b"\x1b[3;5R".to_vec()]);
}

#[test]
fn csi_gt_4_2_m_sets_modify_other_keys_level_2() {
    // esctest and Claude Code emit `CSI > 4 ; 2 m`; the key encoder
    // reads the level to disambiguate Shift+Enter.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>4;2m");
    assert_eq!(g.mode_snapshot().modify_other_keys, ModifyOtherKeys::Level2);
}

#[test]
fn csi_gt_4_1_m_sets_modify_other_keys_level_1() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>4;1m");
    assert_eq!(g.mode_snapshot().modify_other_keys, ModifyOtherKeys::Level1);
}

#[test]
fn csi_gt_4_0_m_disables_modify_other_keys() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>4;2m\x1b[>4;0m");
    assert_eq!(g.mode_snapshot().modify_other_keys, ModifyOtherKeys::Off);
}

#[test]
fn csi_gt_4_m_without_level_resets_to_zero() {
    // `CSI > 4 m` with no Pv is xterm's "reset this resource to default".
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>4;2m\x1b[>4m");
    assert_eq!(g.mode_snapshot().modify_other_keys, ModifyOtherKeys::Off);
}

#[test]
fn csi_gt_m_bare_resets_modify_other_keys() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>4;2m\x1b[>m");
    assert_eq!(g.mode_snapshot().modify_other_keys, ModifyOtherKeys::Off);
}

#[test]
fn modify_other_keys_level_clamps_to_two() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>4;9m");
    assert_eq!(g.mode_snapshot().modify_other_keys, ModifyOtherKeys::Level2);
}

#[test]
fn csi_gt_non_other_keys_resource_leaves_modify_other_keys_untouched() {
    // `CSI > 2 ; 2 m` is modifyFunctionKeys, a no-op for felis.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>2;2m");
    assert_eq!(g.mode_snapshot().modify_other_keys, ModifyOtherKeys::Off);
}

#[test]
fn plain_sgr_m_does_not_touch_modify_other_keys() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>4;2m\x1b[1;31m");
    assert_eq!(
        g.mode_snapshot().modify_other_keys,
        ModifyOtherKeys::Level2,
        "an SGR write must not reset the modifyOtherKeys level"
    );
}

#[test]
fn ris_resets_modify_other_keys() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 2);
    drive(&mut p, &mut g, b"\x1b[>4;2m\x1bc");
    assert_eq!(g.mode_snapshot().modify_other_keys, ModifyOtherKeys::Off);
}

#[test]
fn decset_9001_toggles_win32_input_mode() {
    // ConPTY / PSReadLine enable win32-input-mode with `CSI ? 9001 h`.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[?9001h");
    assert!(g.mode_snapshot().win32_input_mode);
    drive(&mut p, &mut g, b"\x1b[?9001l");
    assert!(!g.mode_snapshot().win32_input_mode);
}

#[test]
fn win32_input_mode_survives_decstr_but_not_ris() {
    // A ConPTY-terminal negotiation, not app display state: a TUI's
    // soft reset must not silently sever input.
    let mut p = Parser::new();
    let mut g = Grid::new(2, 2);
    drive(&mut p, &mut g, b"\x1b[?9001h\x1b[!p");
    assert!(
        g.mode_snapshot().win32_input_mode,
        "DECSTR must not drop win32-input-mode"
    );
    drive(&mut p, &mut g, b"\x1bc");
    assert!(
        !g.mode_snapshot().win32_input_mode,
        "RIS must reset win32-input-mode"
    );
}

#[test]
fn csi_query_u_responds_with_active_kitty_kbd_flags() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[?u");
    assert_eq!(responses(&mut g), vec![b"\x1b[?0u".to_vec()]);
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    drive(&mut p, &mut g, b"\x1b[>13u\x1b[?u");
    let responses = responses(&mut g);
    assert_eq!(responses, vec![b"\x1b[?13u".to_vec()]);
}

#[test]
fn multiple_queries_in_one_chunk_each_get_their_own_response() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>1u\x1b[?u\x1b[>2u\x1b[?u");
    let responses = responses(&mut g);
    assert_eq!(responses, vec![b"\x1b[?1u".to_vec(), b"\x1b[?2u".to_vec()]);
}

#[test]
fn take_kitty_kbd_dirty_fires_only_when_top_changed() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    assert_eq!(g.take_kitty_kbd_dirty(), None);
    drive(&mut p, &mut g, b"\x1b[>1u");
    assert_eq!(g.take_kitty_kbd_dirty(), Some(KittyKbdFlags::DISAMBIGUATE));
    assert_eq!(g.take_kitty_kbd_dirty(), None);
    drive(&mut p, &mut g, b"\x1b[<u");
    assert_eq!(g.take_kitty_kbd_dirty(), Some(KittyKbdFlags::empty()));
    drive(&mut p, &mut g, b"\x1b[>0u");
    assert_eq!(g.take_kitty_kbd_dirty(), None);
}

#[test]
fn kitty_kbd_push_evicts_oldest_when_stack_is_full() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    for i in 1..=KITTY_KBD_STACK_LIMIT + 1 {
        let cmd = format!("\x1b[>{}u", i & 0x1F);
        drive(&mut p, &mut g, cmd.as_bytes());
    }
    assert_eq!(g.kitty_kbd_stack_depth(), KITTY_KBD_STACK_LIMIT);
    assert_eq!(
        g.kitty_kbd_flags().bits(),
        ((KITTY_KBD_STACK_LIMIT + 1) & 0x1F) as u8
    );
}

#[test]
fn resize_keeps_prompt_marks_with_absolute_lines_intact() {
    // docs/explanation/data-model/scrollback.md: marks carry absolute
    // lines a resize cannot invalidate.
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(5, 4);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b]133;A\x07"); // line 0
    drive(&mut p, &mut g, b"\x1b[5;1H\x1b]133;A\x07"); // line 4
    assert_eq!(g.prompt_marks().len(), 2);
    g.resize(3, 4);
    let marks = g.prompt_marks();
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[0].line, 0);
    assert_eq!(marks[1].line, 4);
    assert_eq!(marks[0].kind, PromptKind::PromptStart);
}

#[test]
fn resize_preserves_top_left_quadrant() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 6);
    drive(&mut p, &mut g, b"abcdef\r\nghijkl\r\nmnopqr");
    g.resize(2, 4);
    assert_eq!(g.rows(), 2);
    assert_eq!(g.cols(), 4);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'd'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'g'));
}

/// A narrowing resize must trim the preserved `?47` alt-screen snapshot
/// too: an untrimmed wide buffer reinstalled into the narrow grid reads
/// every row after the first from the wrong column offset.
#[test]
fn resize_trims_the_preserved_alt_snapshot_not_just_the_primary() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 16);
    drive(&mut p, &mut g, b"\x1b[?47h");
    drive(&mut p, &mut g, b"\x1b[1;1HA\x1b[2;1HB");
    drive(&mut p, &mut g, b"\x1b[?47l");
    g.resize(4, 8);
    drive(&mut p, &mut g, b"\x1b[?47h");
    assert_eq!(g.rows(), 4);
    assert_eq!(g.cols(), 8);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'A'));
    assert_eq!(
        g.cell(1, 0).unwrap().grapheme,
        Grapheme::Ascii(b'B'),
        "row 1 must show the alt buffer's row 1, not a mis-strided read"
    );
}

/// A grow-from-default resize must extend the implicit full-screen
/// scroll region, or `/bin/sh` (which never emits DECSTBM) keeps
/// line-feeding within the initial region and the grown bottom stays blank.
#[test]
fn resize_grow_extends_implicit_full_screen_scroll_region() {
    let mut p = Parser::new();
    // The daemon's cold-start shape: pool spawns 24×80, the client ships
    // Resize{82, 137} after the window appears.
    let mut g = Grid::new(24, 80);
    g.resize(82, 137);
    drive(&mut p, &mut g, b"\x1b[H");
    for _ in 0..30 {
        drive(&mut p, &mut g, b"\n");
    }
    drive(&mut p, &mut g, b"X");
    assert_eq!(
        g.cursor().row,
        30,
        "line-feed must advance into rows beyond the old bound",
    );
    assert_eq!(
        g.cell(30, 0).unwrap().grapheme,
        Grapheme::Ascii(b'X'),
        "stamp must land in the post-grow scroll region",
    );
}

/// An explicitly pinned narrow scroll region survives a resize that
/// still fits it; pagers in a resized window would otherwise mis-scroll.
#[test]
fn resize_preserves_explicit_narrow_scroll_region_when_it_still_fits() {
    let mut p = Parser::new();
    let mut g = Grid::new(24, 80);
    drive(&mut p, &mut g, b"\x1b[6;11r");
    g.resize(40, 120);
    drive(&mut p, &mut g, b"\x1b[11;1H"); // row 10 (0-based) col 0
    drive(&mut p, &mut g, b"keep\n");
    assert_eq!(
        g.cursor().row,
        10,
        "explicit DECSTBM band must survive a resize that still fits it",
    );
}

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

/// `append_combining_mark` lends its scratch buffer out via `mem::take`;
/// every return path must move it back, or the reuse degrades to a
/// fresh alloc per mark (visible only as benchmark noise).
#[test]
fn combining_mark_scratch_buffer_survives_every_return_path() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, "e\u{0301}".as_bytes());
    let cap = g.screen.cluster_table.scratch.capacity();
    assert!(cap > 0, "intern path must hand the scratch buffer back");
    drive(&mut p, &mut g, "\u{0327}".as_bytes());
    assert!(g.screen.cluster_table.scratch.capacity() >= cap);
    // Park the cursor past a never-printed cell so the mark's owner
    // resolves to `Grapheme::Empty`.
    drive(&mut p, &mut g, b"\x1b[2;3H");
    drive(&mut p, &mut g, "\u{0301}".as_bytes());
    assert!(
        g.screen.cluster_table.scratch.capacity() >= cap,
        "early-return path dropped the scratch buffer"
    );
}

#[test]
fn decscusr_maps_each_param_to_the_right_cursor_style() {
    // xterm: 0/1/2 → Block, 3/4 → Underline, 5/6 → Bar; anything else is
    // ignored.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    assert_eq!(g.cursor_style(), CursorStyle::Block, "default is Block");

    for ps in [3u8, 4] {
        drive(&mut p, &mut g, format!("\x1b[{ps} q").as_bytes());
        assert_eq!(g.cursor_style(), CursorStyle::Underline, "ps={ps}");
    }
    for ps in [5u8, 6] {
        drive(&mut p, &mut g, format!("\x1b[{ps} q").as_bytes());
        assert_eq!(g.cursor_style(), CursorStyle::Bar, "ps={ps}");
    }
    for ps in [0u8, 1, 2] {
        drive(&mut p, &mut g, b"\x1b[3 q"); // first set to Underline
        drive(&mut p, &mut g, format!("\x1b[{ps} q").as_bytes());
        assert_eq!(g.cursor_style(), CursorStyle::Block, "ps={ps}");
    }
    drive(&mut p, &mut g, b"\x1b[5 q");
    drive(&mut p, &mut g, b"\x1b[99 q");
    assert_eq!(g.cursor_style(), CursorStyle::Bar, "unknown ps ignored");
}

#[test]
fn decstr_resets_cursor_style_to_block() {
    // VT510 / xterm: soft reset restores the cursor shape, or a vim crash
    // + `tput reset` would leave vim's bar cursor permanently.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[5 q\x1b[!p");
    assert_eq!(g.cursor_style(), CursorStyle::Block);
}

/// `Sink::print_utf8_run` must be byte-for-byte equivalent to a
/// per-byte decode regardless of chunking; one byte per `advance`
/// forces the pending-drain path at every sequence boundary.
#[test]
fn bulk_utf8_run_is_chunking_invariant() {
    let input = "\u{202E}你好😀aé e\u{0301}f\r\n\u{2066}ab".as_bytes();

    let mut whole_p = Parser::new();
    let mut whole_g = Grid::new(3, 16);
    drive(&mut whole_p, &mut whole_g, input);

    let mut split_p = Parser::new();
    let mut split_g = Grid::new(3, 16);
    for &b in input {
        split_p.advance(&mut split_g, &[b]);
    }

    for r in 0..whole_g.rows() {
        for c in 0..whole_g.cols() {
            assert_eq!(
                whole_g.cell(r, c),
                split_g.cell(r, c),
                "cell ({r}, {c}) diverged between whole and per-byte chunking"
            );
        }
    }
}

/// A recycled alt-screen row keeps its previous tenant's cells past the
/// watermark; an ASCII run landing there must not read them as live.
#[test]
fn ascii_run_past_the_watermark_ignores_a_stale_spacer() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, "\x1b[?1049hx中\n\réé\x1b[mb".as_bytes());
    let row: Vec<_> = (0..4).map(|c| g.cell(0, c).unwrap().grapheme).collect();
    assert_eq!(
        row,
        [
            Grapheme::Char('é'),
            Grapheme::Char('é'),
            Grapheme::Ascii(b'b'),
            Grapheme::Empty,
        ]
    );
}

// docs/explanation/data-model/grid-and-cells.md pins `Cell` at 16 bytes
// (four per cache line); residency and the ascii store floor ride it, so
// a field that regrows it must justify the cost here.
const _: () = {
    const fn assert_copy<T: Copy>() {}
    assert_copy::<Cell>();
    assert!(size_of::<Cell>() == 16);
};

//
// The unified effect queue's one contract: effects come out in the
// order the program wrote them (cross-kind order inside one PTY burst is
// the recurring Kitty-graphics bug class).

/// `ED` → APC → DA1 → bottom-LF scroll → `?1049h`, in byte-stream order.
#[test]
fn pty_effects_drain_in_byte_stream_order() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    let mut burst = Vec::new();
    burst.extend_from_slice(b"\x1b[2J"); // ED 2 → Erased
    burst.extend_from_slice(b"\x1b_Ga=q,i=1;\x1b\\"); // → Apc
    burst.extend_from_slice(b"\x1b[c"); // DA1 → Response
    burst.extend_from_slice(b"a\r\nb\r\n"); // bottom LF → ScrolledIntoScrollback
    burst.extend_from_slice(b"\x1b[?1049h"); // → ScreenSwitch
    drive(&mut p, &mut g, &burst);
    let effects = g.take_pty_effects();
    let kinds: Vec<&str> = effects
        .iter()
        .map(|e| match e {
            PtyEffect::Response(_) => "response",
            PtyEffect::Apc(_) => "apc",
            PtyEffect::Erased(_) => "erased",
            PtyEffect::ScrolledIntoScrollback(_) => "scrolled",
            PtyEffect::AltScreenScrolled(_) => "alt_scrolled",
            PtyEffect::HardReset => "hard_reset",
            PtyEffect::Scrolled { .. } => "scroll_op",
            PtyEffect::ScreenSwitch(_) => "switch",
            PtyEffect::PrimaryReflowed(_) => "reflowed",
            PtyEffect::BracketedPasteDisabled => "paste_guard_off",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "erased",
            "apc",
            "response",
            "scrolled",
            "scroll_op",
            "switch"
        ],
        "effects={effects:?}"
    );
    assert_eq!(g.take_pty_effects(), Vec::<PtyEffect>::new());
}

/// REQ-902: edge-triggered, so a program spamming resets cannot flood
/// the daemon's log.
#[test]
fn disabling_bracketed_paste_queues_one_edge_triggered_warning() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"\x1b[?2004h");
    drop(g.take_pty_effects());
    drive(&mut p, &mut g, b"\x1b[?2004l\x1b[?2004l");
    let warnings = g
        .take_pty_effects()
        .iter()
        .filter(|e| matches!(e, PtyEffect::BracketedPasteDisabled))
        .count();
    assert_eq!(warnings, 1, "edge-triggered: on→off fires once");
    assert!(!g.bracketed_paste());
}

/// The split is the ordering information the queue exists to keep: a
/// scroll before an `a=T` must not shift the placement `a=T` creates.
#[test]
fn adjacent_scrollback_pushes_coalesce_and_interleaved_effects_split_the_run() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"a\r\nb\r\nc\r\nd\r\n");
    let effects = g.take_pty_effects();
    assert!(
        matches!(
            effects.as_slice(),
            [
                PtyEffect::ScrolledIntoScrollback(3),
                PtyEffect::Scrolled { .. },
            ]
        ),
        "the directive between the pushes must not split them: effects={effects:?}"
    );
    drive(&mut p, &mut g, b"e\r\n\x1b[2Kf\r\n");
    let effects = g.take_pty_effects();
    assert!(
        matches!(
            effects.as_slice(),
            [
                PtyEffect::ScrolledIntoScrollback(1),
                PtyEffect::Scrolled { .. },
                PtyEffect::Erased(_),
                PtyEffect::ScrolledIntoScrollback(1),
                PtyEffect::Scrolled { .. },
            ]
        ),
        "effects={effects:?}"
    );
}

/// A probe running under S8C1T reads C1-led responses.
#[test]
fn pty_effects_drain_applies_c1_transform_to_responses() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"\x1b G\x1b[c"); // S8C1T, then DA1
    let effects = g.take_pty_effects();
    let [PtyEffect::Response(bytes)] = effects.as_slice() else {
        panic!("expected one response, got {effects:?}");
    };
    assert_eq!(bytes[0], 0x9B, "CSI must collapse to its 8-bit C1 form");
}

#[test]
fn autowrap_sets_soft_wrap_on_the_landing_row() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    drive(&mut p, &mut g, b"abcdefg");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(g.row_soft_wrap_continued(1));
    assert!(!g.row_soft_wrap_continued(2));
}

#[test]
fn explicit_newline_does_not_set_soft_wrap() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    drive(&mut p, &mut g, b"ab\r\ncd");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(!g.row_soft_wrap_continued(1));
}

/// The unfillable last cell stays `Empty`, the trailing-Empty shape
/// search's stitcher must trim.
#[test]
fn wide_char_wrap_sets_soft_wrap_and_leaves_trailing_empty() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    drive(&mut p, &mut g, "abc中".as_bytes());
    assert!(g.row_soft_wrap_continued(1));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Char('中'));
}

#[test]
fn autowrap_off_does_not_set_soft_wrap() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    drive(&mut p, &mut g, b"\x1b[?7labcdefg");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(!g.row_soft_wrap_continued(1));
}

/// A later search stitches the logical line back across the seam.
#[test]
fn scroll_pushes_soft_wrap_bit_into_scrollback() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 3);
    drive(&mut p, &mut g, b"abcdef\r\n\r\n");
    assert_eq!(g.scrollback().soft_wrap_continued(0), Some(false));
    assert_eq!(g.scrollback().soft_wrap_continued(1), Some(true));
}

#[test]
fn full_row_erase_clears_soft_wrap_partial_keeps_it() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 3);
    drive(&mut p, &mut g, b"abcdef");
    assert!(g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[1;2H");
    drive(&mut p, &mut g, b"\x1b[2;2H\x1b[K");
    assert!(g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[2K");
    assert!(!g.row_soft_wrap_continued(1));
}

#[test]
fn ed2_clears_all_soft_wrap_bits() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 3);
    drive(&mut p, &mut g, b"abcdef\x1b[2J");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(!g.row_soft_wrap_continued(1));
}

/// Already-blank rows change nothing, so shipping them as `RowDelta`s
/// would be pure wire noise (the vim / lazygit CUP + ED 2 repaint loop
/// clears a mostly-blank screen every frame).
#[test]
fn ed2_default_pen_marks_only_rows_with_content() {
    let mut p = Parser::new();
    let mut g = Grid::new(5, 10);
    drive(&mut p, &mut g, b"top\x1b[3;1Hmid");
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"\x1b[2J");
    for r in 0..5 {
        for c in 0..10 {
            assert!(g.cell(r, c).unwrap().is_blank(), "cell ({r},{c})");
        }
    }
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![0, 2]);
}

#[test]
fn ed2_on_a_blank_screen_marks_nothing() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"\x1b[2J");
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert!(dirty.is_empty(), "unexpected dirty rows: {dirty:?}");
}

/// A BCE pen paints every cell, so the prefix shortcut must not apply.
#[test]
fn ed2_bce_pen_marks_every_row_and_colors_every_cell() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"\x1b[44m\x1b[2J");
    for r in 0..3 {
        for c in 0..4 {
            let cell = *g.cell(r, c).unwrap();
            assert_eq!(cell.grapheme, Grapheme::Empty);
            assert_eq!(g.style(cell.style).bg, Color::Indexed(4), "cell ({r},{c})");
        }
    }
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![0, 1, 2]);
}

#[test]
fn alt_screen_isolates_soft_wrap_from_the_primary() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 3);
    drive(&mut p, &mut g, b"abcdef");
    assert!(g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert!(!g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[HXYZuvw");
    assert!(g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[?1049l");
    assert!(g.row_soft_wrap_continued(1));
    assert!(!g.row_soft_wrap_continued(0));
}

/// The bit must stay attached to its content row, not its physical
/// slot.
#[test]
fn resize_keeps_soft_wrap_attached_to_content_after_rotation() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 3);
    drive(&mut p, &mut g, b"abcdefgh\r\n");
    assert!(g.row_soft_wrap_continued(0), "def row still a continuation");
    assert!(g.row_soft_wrap_continued(1), "gh row still a continuation");
    g.resize(4, 3);
    assert!(g.row_soft_wrap_continued(0));
    assert!(g.row_soft_wrap_continued(1));
    assert!(!g.row_soft_wrap_continued(2));
    assert!(!g.row_soft_wrap_continued(3));
}

//
// Called directly the way `ShadowScreen::apply` does, pinning the
// admission rule and the blank-band math a parser-only test never
// reaches.

/// Row rotation, blanking with the default cell rather than the pen.
/// `None` for a directive the grid refuses.
fn scroll_directive_model(
    rows: &[Vec<Option<u8>>],
    region_top: u16,
    region_bottom: u16,
    n_rows: u16,
    direction: ScrollDirection,
) -> Option<Vec<Vec<Option<u8>>>> {
    let mut out = rows.to_vec();
    let top = usize::from(region_top);
    let bottom = usize::from(region_bottom);
    if top > bottom || bottom >= rows.len() {
        return None;
    }
    let height = bottom - top + 1;
    let n = usize::from(n_rows);
    if n == 0 || n > height {
        return None;
    }
    let blank = vec![None; rows[0].len()];
    for r in top..=bottom {
        out[r] = match direction {
            ScrollDirection::Up if r + n <= bottom => rows[r + n].clone(),
            ScrollDirection::Down if r >= top + n => rows[r - n].clone(),
            _ => blank.clone(),
        };
    }
    Some(out)
}

proptest! {
    #[test]
    fn apply_scroll_directive_matches_a_row_rotation_model(
        rows in 1u16..=6,
        cols in 1u16..=3,
        region_top in 0u16..=7,
        region_bottom in 0u16..=7,
        n_rows in 0u16..=8,
        down in any::<bool>(),
    ) {
        let filled: Vec<Vec<Option<u8>>> = (0..rows)
            .map(|r| {
                (0..cols)
                    .map(|c| Some(b'a' + ((u32::from(r) * u32::from(cols) + u32::from(c)) % 26) as u8))
                    .collect()
            })
            .collect();
        let mut bytes = Vec::new();
        for (r, row) in filled.iter().enumerate() {
            bytes.extend_from_slice(format!("\x1b[{};1H", r + 1).as_bytes());
            bytes.extend(row.iter().map(|b| b.expect("filled")));
        }
        let mut p = Parser::new();
        let mut g = Grid::new(rows, cols);
        drive(&mut p, &mut g, &bytes);

        let direction = if down { ScrollDirection::Down } else { ScrollDirection::Up };
        let applied = g.apply_scroll_directive(region_top, region_bottom, n_rows, direction);

        let expected = scroll_directive_model(&filled, region_top, region_bottom, n_rows, direction);
        prop_assert_eq!(
            applied,
            expected.is_some(),
            "a directive outside the grid must be refused, not normalized"
        );
        let expected = expected.unwrap_or_else(|| filled.clone());
        for (r, row) in expected.iter().enumerate() {
            for (c, want) in row.iter().enumerate() {
                let cell = g.cell(r as u16, c as u16).expect("in-range cell");
                match want {
                    Some(b) => prop_assert_eq!(
                        cell.grapheme, Grapheme::Ascii(*b),
                        "row {} col {} should hold {}", r, c, *b as char
                    ),
                    None => prop_assert!(cell.is_blank(), "row {} col {} should be blank", r, c),
                }
            }
        }
    }
}

//
// A 3-row grid with 3 rows pushed into scrollback: live screen occupies
// absolute lines [3, 6), retained scrollback [0, 3), anything below 0
// is evicted.

fn grid_three_pushed() -> Grid {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    drive(&mut p, &mut g, b"\r\n\r\n\r\n\r\n\r\n");
    assert_eq!(g.scrollback_total_pushed(), 3);
    g
}

/// `line == base` is the first live row, not the last scrollback row.
#[test]
fn locate_line_maps_screen_lines_to_rows() {
    let g = grid_three_pushed();
    assert_eq!(g.locate_line(3), MarkLocation::Screen(0));
    assert_eq!(g.locate_line(4), MarkLocation::Screen(1));
    assert_eq!(g.locate_line(5), MarkLocation::Screen(2));
}

/// `base - sb_len` is the oldest retained row; `base - 1` the youngest.
#[test]
fn locate_line_maps_scrollback_lines_to_indices() {
    let g = grid_three_pushed();
    assert_eq!(g.locate_line(0), MarkLocation::Scrollback(0));
    assert_eq!(g.locate_line(1), MarkLocation::Scrollback(1));
    assert_eq!(g.locate_line(2), MarkLocation::Scrollback(2));
}

/// `base - sb_len` is still retained, one below it is gone.
#[test]
fn locate_line_reports_evicted_below_the_retained_ring() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    for _ in 0..DEFAULT_SCROLLBACK_ROWS + 5 {
        drive(&mut p, &mut g, b"\r\n");
    }
    let base = g.scrollback_total_pushed();
    let sb_len = g.scrollback().len() as u64;
    assert_eq!(g.locate_line(base - sb_len), MarkLocation::Scrollback(0));
    assert_eq!(g.locate_line(base - sb_len - 1), MarkLocation::Evicted);
}

/// The pruned counter records the drop so the daemon's ordinal cursor
/// can translate past it.
#[test]
fn evicted_prompt_marks_are_pruned_from_the_front() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b]133;A\x07");
    assert_eq!(g.prompt_marks().len(), 1);
    assert_eq!(g.prompt_marks_pruned(), 0);
    for _ in 0..DEFAULT_SCROLLBACK_ROWS + 5 {
        drive(&mut p, &mut g, b"\r\n");
    }
    assert!(g.prompt_marks().is_empty(), "evicted mark must be dropped");
    assert_eq!(g.prompt_marks_pruned(), 1);
}

/// ED 3 prunes only the marks pointing into history; live-screen marks
/// keep their absolute lines.
#[test]
fn ed_3_prunes_marks_pointing_into_scrollback() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b]133;A\x07"); // line 0
    drive(&mut p, &mut g, b"\r\n\r\n"); // push rows into scrollback
    drive(&mut p, &mut g, b"\x1b]133;A\x07"); // a live-screen mark
    let live_line = g.scrollback_total_pushed() + u64::from(g.cursor().row);
    assert!(g.prompt_marks().len() >= 2);
    assert!(!g.scrollback().is_empty());
    drive(&mut p, &mut g, b"\x1b[3J");
    let base = g.scrollback_total_pushed();
    assert!(
        g.prompt_marks().iter().all(|m| m.line >= base),
        "no surviving mark may point into the cleared scrollback"
    );
    assert!(g.prompt_marks().iter().any(|m| m.line == live_line));
    assert!(g.prompt_marks_pruned() >= 1);
}

// These pin cursor / response effects the coverage-matrix snapshot
// probes cannot see: an arm whose only effect is a cursor move on an
// otherwise-empty grid leaves the rendered screen identical.

#[test]
fn cud_moves_the_cursor_down_n_rows() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 8);
    drive(&mut p, &mut g, b"\x1b[3;4H\x1b[2B");
    assert_eq!((g.cursor().row, g.cursor().col), (4, 3));
}

#[test]
fn dl_deletes_lines_pulling_lower_rows_up() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 4);
    drive(&mut p, &mut g, b"AAAA\r\nBBBB\r\nCCCC\x1b[1;1H\x1b[1M");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'B'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'C'));
}

#[test]
fn private_dsr_15_reports_no_printer() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[?15n");
    assert_eq!(responses(&mut g), vec![b"\x1b[?13n".to_vec()]);
}

#[test]
fn private_dsr_25_reports_udk_locked() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[?25n");
    assert_eq!(responses(&mut g), vec![b"\x1b[?21n".to_vec()]);
}

#[test]
fn private_dsr_26_reports_keyboard_status() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[?26n");
    assert_eq!(responses(&mut g), vec![b"\x1b[?27;1;0;0n".to_vec()]);
}

#[test]
fn esc_ind_advances_one_row() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"\x1b[1;3H\x1bD");
    assert_eq!((g.cursor().row, g.cursor().col), (1, 2));
}

#[test]
fn esc_hts_sets_a_tab_stop_at_the_cursor_column() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"\x1b[3g\x1b[1;4H\x1bH\x1b[1;1H\t");
    assert_eq!(g.cursor().col, 3, "HT should land on the HTS-set stop");
}

/// `0` is the field default, so a deleted assignment arm would survive
/// a `0` round trip; this sets a non-default value.
#[test]
fn decrqss_decsace_round_trips_a_non_default_value() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 16);
    drive(&mut p, &mut g, b"\x1b[2*x\x1bP$q*x\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP1$r2*x\x1b\\");
}

fn visible_snapshot(g: &Grid) -> (Vec<Vec<Grapheme>>, Vec<bool>) {
    let mut cells = Vec::new();
    let mut wraps = Vec::new();
    for r in 0..g.rows() {
        let row: Vec<Grapheme> = (0..g.cols())
            .map(|c| g.cell(r, c).unwrap().grapheme)
            .collect();
        cells.push(row);
        wraps.push(g.row_soft_wrap_continued(r));
    }
    (cells, wraps)
}

/// REQ-604 round-trip below the scrollback cap.
#[test]
fn reflow_soft_wrap_narrow_then_widen_round_trips() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 10);
    drive(&mut p, &mut g, b"abcdefghijklmnopqr");
    let before = visible_snapshot(&g);
    assert!(g.row_soft_wrap_continued(1));

    g.reflow(6, 4);
    g.reflow(6, 10);

    let after = visible_snapshot(&g);
    assert_eq!(before, after, "cells + soft-wrap bits must survive W→W'→W");
}

/// The remap points at the row where the same cell landed, not the
/// start of its logical line (REQ-604).
#[test]
fn reflow_remap_translates_live_anchor_across_rewrap() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 10);
    drive(&mut p, &mut g, b"abcdefghijklmnopqr");

    let remap = g
        .reflow(6, 4)
        .expect("primary-screen reflow returns a remap");

    assert_eq!(remap.remap_row(2), Some(3));
    assert_eq!(remap.remap_row(1), Some(1));
}

/// Anchors pushed past the live window remap into scrollback
/// coordinates (row ≤ 0), as `shift_up` keeps scrolled-out placements
/// addressable.
#[test]
fn reflow_remap_moves_anchor_into_scrollback() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    // Wrapped line (rows 1-2) plus two hard lines fill the screen.
    drive(&mut p, &mut g, b"abcdefghijklmnopqr\r\nsecond\r\nthird");

    let remap = g
        .reflow(4, 4)
        .expect("primary-screen reflow returns a remap");

    // 9 physical rows now exist; the last 4 are live, 5 spill into
    // scrollback. Row 1 ("abcd…") becomes the oldest retained line.
    assert_eq!(remap.remap_row(1), Some(-4));
    assert_eq!(remap.remap_row(2), Some(-2));
    // "third" (old row 4) opens at new physical row 7 → live row 3.
    assert_eq!(remap.remap_row(4), Some(3));
}

/// Blank rows below the content are screen padding the reflow
/// regenerates, so an anchor there keeps its distance rather than
/// reading as evicted.
#[test]
fn reflow_remap_preserves_padding_distance_below_content() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 10);
    drive(&mut p, &mut g, b"hello");

    let remap = g
        .reflow(6, 4)
        .expect("primary-screen reflow returns a remap");

    assert_eq!(remap.remap_row(3), Some(4));
}

#[test]
fn reflow_keeps_hard_wrapped_lines_separate() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 8);
    drive(&mut p, &mut g, b"aaaa\r\nbbbb");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(!g.row_soft_wrap_continued(1));

    g.reflow(6, 3);

    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert!(g.row_soft_wrap_continued(1), "wrap of the first line");
    assert_eq!(g.cell(2, 0).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert!(
        !g.row_soft_wrap_continued(2),
        "the LF boundary is not a soft wrap"
    );
}

/// D1: the cursor keeps pointing at the same grapheme.
#[test]
fn reflow_preserves_cursor_logical_position() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 5);
    drive(&mut p, &mut g, b"0123456789");
    drive(&mut p, &mut g, b"\x1b[2;3H");
    assert_eq!(
        g.cell(g.cursor().row, g.cursor().col).unwrap().grapheme,
        Grapheme::Ascii(b'7')
    );

    g.reflow(6, 3);
    assert_eq!(
        g.cell(g.cursor().row, g.cursor().col).unwrap().grapheme,
        Grapheme::Ascii(b'7'),
        "cursor tracks '7' at the narrow width"
    );

    g.reflow(6, 5);
    assert_eq!(
        g.cell(g.cursor().row, g.cursor().col).unwrap().grapheme,
        Grapheme::Ascii(b'7'),
        "cursor returns to '7' at the original width"
    );
}

/// D1: the mark lands on the first physical row of its logical line.
#[test]
fn reflow_remaps_prompt_mark_to_its_logical_line_start() {
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(6, 5);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b]133;A\x07");
    drive(&mut p, &mut g, b"promptline");
    assert_eq!(g.prompt_marks().len(), 1);
    assert_eq!(g.prompt_marks()[0].kind, PromptKind::PromptStart);

    g.reflow(6, 3);

    let mark_line = g.prompt_marks()[0].line;
    assert_eq!(g.locate_line(mark_line), MarkLocation::Screen(0));
}

fn screen_ascii(g: &Grid) -> Vec<String> {
    (0..g.rows())
        .map(|r| {
            let line: String = (0..g.cols())
                .map(|c| match g.cell(r, c).map(|cell| cell.grapheme) {
                    Some(Grapheme::Ascii(b)) => char::from(b),
                    _ => ' ',
                })
                .collect();
            line.trim_end().to_owned()
        })
        .collect()
}

/// What zsh does on SIGWINCH: climb the rows its prompt took at the old
/// width (here one above the cursor), erase below, repaint.
const ZSH_REPAINT_AT_6: &[u8] = b"\x1b[1A\r\x1b[J\x1b]133;A\x07------\r\n% ";

/// One finished command, so the shell has shown it marks command starts.
const PRIOR_COMMAND: &[u8] = b"\x1b]133;C\x07out\r\n\x1b]133;D;0\x07";

fn grid_after(bytes: &[&[u8]]) -> (Parser, Grid) {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 10);
    for b in bytes {
        drive(&mut p, &mut g, b);
    }
    (p, g)
}

#[test]
fn reflow_blanks_the_prompt_the_shell_repaints() {
    let (mut p, mut g) = grid_after(&[PRIOR_COMMAND, b"\x1b]133;A\x07----------\r\n% "]);

    g.reflow(6, 6);
    drive(&mut p, &mut g, ZSH_REPAINT_AT_6);

    assert_eq!(screen_ascii(&g), ["out", "------", "%", "", "", ""]);
}

#[test]
fn reflow_rewraps_the_prompt_under_redraw_0() {
    let (_, mut g) = grid_after(&[PRIOR_COMMAND, b"\x1b]133;A;redraw=0\x07----------\r\n% "]);

    g.reflow(6, 6);

    assert_eq!(screen_ascii(&g), ["out", "------", "----", "%", "", ""]);
}

#[test]
fn reflow_keeps_the_redraw_0_of_a_prompt_with_a_continuation_line() {
    let (_, mut g) = grid_after(&[
        PRIOR_COMMAND,
        b"\x1b]133;A;redraw=0\x07----------\r\n\x1b]133;A;k=s\x07% ",
    ]);

    g.reflow(6, 6);

    assert_eq!(screen_ascii(&g), ["out", "------", "----", "%", "", ""]);
}

#[test]
fn reflow_blanks_only_the_cursor_line_under_redraw_last() {
    let (_, mut g) = grid_after(&[PRIOR_COMMAND, b"\x1b]133;A;redraw=last\x07----------\r\n% "]);

    g.reflow(6, 6);

    assert_eq!(screen_ascii(&g), ["out", "------", "----", "", "", ""]);
    assert_eq!((g.cursor().row, g.cursor().col), (3, 2));
}

#[test]
fn reflow_keeps_command_output_once_the_command_started() {
    let (_, mut g) = grid_after(&[b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07abcdefghij"]);

    g.reflow(6, 6);

    assert_eq!(screen_ascii(&g), ["$ ls", "abcdef", "ghij", "", "", ""]);
}

/// Without `C`, a running command's output is indistinguishable from
/// the prompt it was typed at.
#[test]
fn reflow_keeps_output_from_a_shell_that_never_marks_command_starts() {
    let (_, mut g) = grid_after(&[b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\nabcdefghij"]);

    g.reflow(6, 6);

    assert_eq!(screen_ascii(&g), ["$ ls", "abcdef", "ghij", "", "", ""]);
}

/// A `k=s` continuation must not become the clear's start, or the
/// prompt's first line is re-wrapped and left behind.
#[test]
fn reflow_blanks_from_the_primary_prompt_not_a_continuation() {
    let (_, mut g) = grid_after(&[
        PRIOR_COMMAND,
        b"\x1b]133;A\x07----------\r\n\x1b]133;A;k=s\x07% ",
    ]);

    g.reflow(6, 6);

    assert_eq!(screen_ascii(&g), ["out", "", "", "", "", ""]);
    assert_eq!((g.cursor().row, g.cursor().col), (2, 2));
}

/// `unsetopt prompt_cr prompt_sp; printf foo` leaves the next prompt on
/// `foo`'s row, which the shell will not repaint.
#[test]
fn reflow_keeps_output_on_the_row_a_prompt_starts_mid_line() {
    let (_, mut g) = grid_after(&[
        PRIOR_COMMAND,
        b"\x1b]133;C\x07foo\x1b]133;D;0\x07\x1b]133;A\x07% ",
    ]);

    g.reflow(6, 6);

    assert_eq!(screen_ascii(&g), ["out", "foo%", "", "", "", ""]);
}

/// A shell inside a TUI marks prompts on the alternate screen; they say
/// nothing about the primary screen's prompt.
#[test]
fn reflow_ignores_prompt_marks_from_the_alternate_screen() {
    let (mut p, mut g) = grid_after(&[
        PRIOR_COMMAND,
        b"\x1b]133;A\x07$ \x1b]133;B\x07tui\r\n\x1b]133;C\x07\x1b[?1049h",
        b"\x1b]133;A\x07inner\x1b]133;B\x07",
    ]);

    g.reflow(6, 6);
    drive(&mut p, &mut g, b"\x1b[?1049l");

    assert_eq!(screen_ascii(&g), ["out", "$ tui", "", "", "", ""]);
}

/// bash under `redraw=last` repaints only the line its cursor is on;
/// the rows below are its to keep.
#[test]
fn reflow_keeps_rows_below_the_cursor_under_redraw_last() {
    let (_, mut g) = grid_after(&[
        PRIOR_COMMAND,
        b"\x1b]133;A;redraw=last\x07$ \r\nbelowbelow\x1b[2;3H",
    ]);

    let remap = g.reflow(6, 6).expect("geometry changed");

    assert_eq!(screen_ascii(&g), ["out", "", "belowb", "elow", "", ""]);
    assert_eq!((g.cursor().row, g.cursor().col), (1, 2));
    assert_eq!(remap.remap_row(2), None, "the cursor's row");
    assert_eq!(
        remap.remap_row(3),
        Some(3),
        "the row below keeps its anchor"
    );
}

/// A prompt that arrives while a marked command still runs belongs to
/// what the command started, which may not mark its own commands.
#[test]
fn reflow_keeps_output_under_a_prompt_from_inside_a_running_command() {
    let (_, mut g) = grid_after(&[
        PRIOR_COMMAND,
        b"\x1b]133;A\x07$ \x1b]133;B\x07bash\r\n\x1b]133;C\x07",
        b"\x1b]133;A\x07# \x1b]133;B\x07ls\r\nabcdefghij",
    ]);

    g.reflow(6, 6);

    assert_eq!(
        screen_ascii(&g),
        ["out", "$ bash", "# ls", "abcdef", "ghij", ""]
    );
}

/// Once a nested shell marks its own command, its next prompt is
/// blanked like any other.
#[test]
fn reflow_blanks_a_nested_prompt_once_it_ended_a_marked_command() {
    let (mut p, mut g) = grid_after(&[
        b"\x1b]133;A\x07$ \x1b]133;B\x07zsh\r\n\x1b]133;C\x07",
        b"\x1b]133;A\x07# \x1b]133;B\x07true\r\n\x1b]133;C\x07\x1b]133;D;0\x07",
        b"\x1b]133;A\x07----------\r\n% ",
    ]);

    g.reflow(6, 6);
    drive(&mut p, &mut g, ZSH_REPAINT_AT_6);

    assert_eq!(screen_ascii(&g), ["$ zsh", "# true", "------", "%", "", ""]);
}

#[test]
fn reflow_remap_drops_anchors_on_a_blanked_prompt() {
    let (_, mut g) = grid_after(&[PRIOR_COMMAND, b"\x1b]133;A\x07----------\r\n% "]);

    let remap = g.reflow(6, 6).expect("geometry changed");

    assert_eq!(
        remap.remap_row(1),
        Some(1),
        "the output row keeps its anchor"
    );
    assert_eq!(remap.remap_row(2), None, "the prompt's first row");
    assert_eq!(remap.remap_row(3), None, "the cursor's row");
}

#[test]
fn reflow_never_splits_a_wide_glyph() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 2);
    drive(&mut p, &mut g, "aあ".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Char('あ'));
    assert_eq!(g.cell(1, 1).unwrap().grapheme, Grapheme::Spacer);

    g.reflow(6, 3);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Char('あ'));
    assert_eq!(
        g.cell(0, 2).unwrap().grapheme,
        Grapheme::Spacer,
        "the wide glyph keeps its spacer partner"
    );
}

/// D3: the alt buffer trim/pads and no scrollback is disturbed.
#[test]
fn reflow_on_alternate_screen_delegates_to_resize() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"\x1b[?1049h"); // enter alt screen
    drive(&mut p, &mut g, b"alt-text");
    assert!(g.on_alternate_screen());

    g.reflow(4, 4);
    assert_eq!(g.cols(), 4);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert!(g.scrollback().is_empty());
}

/// The preserved `?47` snapshot is trimmed at its capture-time column
/// count, not the post-reflow one: the alt buffer trim/pads (D3), it
/// does not reflow.
#[test]
fn reflow_trims_the_preserved_alt_snapshot_at_capture_dimensions() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 16);
    drive(&mut p, &mut g, b"\x1b[?47h");
    drive(&mut p, &mut g, b"\x1b[1;1HA\x1b[2;1HB");
    drive(&mut p, &mut g, b"\x1b[?47l");
    g.reflow(4, 8);
    drive(&mut p, &mut g, b"\x1b[?47h");
    assert_eq!(g.rows(), 4);
    assert_eq!(g.cols(), 8);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'A'));
    assert_eq!(
        g.cell(1, 0).unwrap().grapheme,
        Grapheme::Ascii(b'B'),
        "the alt snapshot must trim from 8x16, not a stale stride"
    );
}

// A default-pen scroll recycles a physical row by dropping its
// watermark and leaving unreferenced bytes in place
// (docs/explanation/data-model/grid-and-cells.md "Occupancy
// watermark"); no reader or writer may expose that stale tail.

/// Rows `AAAA`, `BBBB`, `CCCC`, then one bottom line feed: the recycled
/// bottom row's physical slot still holds `AAAA`.
fn scrolled_grid_with_stale_tail() -> (Parser, Grid) {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    drive(
        &mut p,
        &mut g,
        b"\x1b[1;1HAAAAAAAAAA\x1b[2;1HBBBBBBBBBB\x1b[3;1HCCCCCCCCCC",
    );
    drive(&mut p, &mut g, b"\x1b[3;1H\n");
    (p, g)
}

#[test]
fn recycled_row_reads_blank_past_the_watermark() {
    let (_p, g) = scrolled_grid_with_stale_tail();
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'B'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'C'));
    for c in 0..10 {
        assert!(
            g.cell(2, c).unwrap().is_blank(),
            "recycled row col {c} must read blank, not the stale 'A'"
        );
    }
    assert_eq!(g.row_content(2).unwrap().len(), 0, "watermark is 0");
}

/// `print_str`'s cold gap fork blanks the leading gap before writing.
#[test]
fn bulk_run_past_watermark_blanks_the_leading_gap() {
    let (mut p, mut g) = scrolled_grid_with_stale_tail();
    drive(&mut p, &mut g, b"\x1b[3;6HZZZ");
    for c in 0..5 {
        assert!(
            g.cell(2, c).unwrap().is_blank(),
            "gap col {c} must be blanked, not stale 'A'"
        );
    }
    for c in 5..8 {
        assert_eq!(g.cell(2, c).unwrap().grapheme, Grapheme::Ascii(b'Z'));
    }
    for c in 8..10 {
        assert!(g.cell(2, c).unwrap().is_blank(), "past-run col {c} clipped");
    }
    assert_eq!(g.row_content(2).unwrap().len(), 8);
}

/// The per-glyph path (`put_grapheme`) blanks the same leading gap.
#[test]
fn single_glyph_past_watermark_blanks_the_leading_gap() {
    let (mut p, mut g) = scrolled_grid_with_stale_tail();
    // "é" routes through the per-byte decoder, not `print_str`.
    drive(&mut p, &mut g, "\x1b[3;6Hé".as_bytes());
    for c in 0..5 {
        assert!(
            g.cell(2, c).unwrap().is_blank(),
            "gap col {c} must be blanked, not stale 'A'"
        );
    }
    assert_eq!(g.cell(2, 5).unwrap().grapheme, Grapheme::Char('é'));
    for c in 6..10 {
        assert!(g.cell(2, c).unwrap().is_blank());
    }
}

/// The watermark bumps only to the run's end; `cell` clips the rest.
#[test]
fn fast_path_write_clips_the_stale_tail_past_the_run() {
    let (mut p, mut g) = scrolled_grid_with_stale_tail();
    drive(&mut p, &mut g, b"HI");
    assert_eq!(g.cell(2, 0).unwrap().grapheme, Grapheme::Ascii(b'H'));
    assert_eq!(g.cell(2, 1).unwrap().grapheme, Grapheme::Ascii(b'I'));
    for c in 2..10 {
        assert!(
            g.cell(2, c).unwrap().is_blank(),
            "col {c} past the run must clip the stale 'A'"
        );
    }
}

/// BCE is live content: the recycled row materializes every cell with
/// the pen background and pins the watermark at `cols`.
#[test]
fn bce_scroll_materializes_the_full_row_with_pen_background() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    drive(
        &mut p,
        &mut g,
        b"\x1b[44m\x1b[1;1HAAAAAAAAAA\x1b[2;1HBBBBBBBBBB\x1b[3;1HCCCCCCCCCC",
    );
    drive(&mut p, &mut g, b"\x1b[3;1H\n");
    for c in 0..10 {
        let cell = *g.cell(2, c).unwrap();
        assert_eq!(
            g.style(cell.style).bg,
            Color::Indexed(4),
            "BCE row col {c} carries the pen background"
        );
        assert_eq!(cell.grapheme, Grapheme::Empty);
    }
    assert_eq!(
        g.row_content(2).unwrap().len(),
        10,
        "BCE pins the watermark at cols"
    );
}

/// `xtgettcap_reply` echoes each cap's hex verbatim, so a reply carrying
/// the trailing `co` proves the last byte before the limit survived.
#[test]
fn xtgettcap_body_exactly_at_the_dcs_buffer_limit_is_answered_intact() {
    let mut p = Parser::new();
    let mut g = Grid::new(24, 80);
    // The filler is one unknown (odd-length, so un-decodable) cap.
    let mut body = vec![b'0'; DCS_BUFFER_LIMIT - 5];
    body.extend_from_slice(b";636f");
    assert_eq!(body.len(), DCS_BUFFER_LIMIT);

    let mut bytes = b"\x1bP+q".to_vec();
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(b"\x1b\\");
    drive(&mut p, &mut g, &bytes);

    let reply = responses(&mut g)
        .into_iter()
        .next()
        .expect("XTGETTCAP reply");
    assert!(
        reply.ends_with(b";636f=3830\x1b\\"),
        "reply lost the cap at the limit: {:?}",
        String::from_utf8_lossy(&reply)
    );
}

/// One byte past the limit the overflow is dropped: the query's final
/// `f` is gone, so `636` cannot decode and the cap goes unanswered.
#[test]
fn xtgettcap_body_one_byte_past_the_dcs_buffer_limit_drops_the_overflow() {
    let mut p = Parser::new();
    let mut g = Grid::new(24, 80);
    let mut body = vec![b'0'; DCS_BUFFER_LIMIT - 4];
    body.extend_from_slice(b";636f");
    assert_eq!(body.len(), DCS_BUFFER_LIMIT + 1);

    let mut bytes = b"\x1bP+q".to_vec();
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(b"\x1b\\");
    drive(&mut p, &mut g, &bytes);

    let reply = responses(&mut g)
        .into_iter()
        .next()
        .expect("XTGETTCAP reply");
    assert!(
        reply.ends_with(b";636\x1b\\"),
        "expected the last body byte dropped, got: {:?}",
        String::from_utf8_lossy(&reply)
    );
    assert!(
        !reply.contains(&b'='),
        "a truncated cap name must not be answered"
    );
}

/// REQ-903: the flooding-producer shape leaves the grid untouched and
/// the parser back in Ground.
#[test]
fn input_after_an_oversized_dcs_body_parses_normally() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    let mut bytes = b"\x1bP+q".to_vec();
    bytes.extend(std::iter::repeat_n(b'6', DCS_BUFFER_LIMIT * 8));
    bytes.extend_from_slice(b"\x1b\\ok");
    drive(&mut p, &mut g, &bytes);

    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'o'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'k'));
    assert_eq!(g.cursor().col, 2);
    assert_eq!(g.cursor().row, 0);
}

/// The REQ-903 discard that mirrors `csi_dispatch`: without the `ignore`
/// bail the `$ q` tail would still read as DECRQSS and answer a
/// malformed request.
#[test]
fn dcs_with_overflowed_intermediates_is_dropped_without_a_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    // Four intermediates against `MAX_INTERMEDIATES = 3`.
    drive(&mut p, &mut g, b"\x1bP$$$$qm\x1b\\ok");

    assert!(
        responses(&mut g).is_empty(),
        "an overflowed DCS opener must not be answered"
    );
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'o'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'k'));
}

/// SCS is consumed so the selector bytes never reach the screen, and
/// the selected set stays inert: `ESC ( 0` does not translate `q` into
/// a line-drawing glyph (`docs/reference/protocols/vt-compliance.md`
/// "Consumed without effect").
#[test]
fn scs_charset_selection_leaves_printed_text_unchanged() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 6);
    drive(&mut p, &mut g, b"\x1b(0q\x1b)Bx");

    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'q'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

/// A scroll directive recycles the rotated-out storage as the vacated
/// row without clearing it, so the row written there next may equal
/// what the storage still holds; it must still land.
#[test]
fn a_row_written_over_a_vacated_row_lands_even_when_it_matches_the_rotated_out_row() {
    let mut screen = ScreenBuffer::with_scrollback(4, 3, 0);
    let row = [Cell {
        grapheme: Grapheme::Ascii(b'y'),
        ..Cell::default()
    }; 3];
    for r in 0..4 {
        screen.write_row_cells(r, &row);
    }
    assert!(screen.apply_scroll_directive(0, 3, 1, ScrollDirection::Up));
    assert_eq!(screen.cell(3, 0).map(|c| c.grapheme), Some(Grapheme::Empty));
    screen.write_row_cells(3, &row);
    assert_eq!(
        screen.cell(3, 0).map(|c| c.grapheme),
        Some(Grapheme::Ascii(b'y')),
    );
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

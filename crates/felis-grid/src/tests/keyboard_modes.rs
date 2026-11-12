use super::*;

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

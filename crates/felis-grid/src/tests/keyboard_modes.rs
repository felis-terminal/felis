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

/// esctest and Claude Code emit `CSI > 4 ; 2 m`; the key encoder reads
/// the level to disambiguate Shift+Enter.
#[test]
fn csi_gt_m_sets_and_resets_the_modify_other_keys_level() {
    for (case, bytes, level) in [
        (
            "4;2 sets level 2",
            &b"\x1b[>4;2m"[..],
            ModifyOtherKeys::Level2,
        ),
        ("4;1 sets level 1", b"\x1b[>4;1m", ModifyOtherKeys::Level1),
        (
            "4;0 disables",
            b"\x1b[>4;2m\x1b[>4;0m",
            ModifyOtherKeys::Off,
        ),
        (
            "4 with no level resets the resource to default",
            b"\x1b[>4;2m\x1b[>4m",
            ModifyOtherKeys::Off,
        ),
        (
            "bare > m resets",
            b"\x1b[>4;2m\x1b[>m",
            ModifyOtherKeys::Off,
        ),
        (
            "a level past 2 clamps to 2",
            b"\x1b[>4;9m",
            ModifyOtherKeys::Level2,
        ),
        (
            "2;2 is modifyFunctionKeys, a no-op",
            b"\x1b[>2;2m",
            ModifyOtherKeys::Off,
        ),
        (
            "a plain SGR leaves the level alone",
            b"\x1b[>4;2m\x1b[1;31m",
            ModifyOtherKeys::Level2,
        ),
        ("RIS resets", b"\x1b[>4;2m\x1bc", ModifyOtherKeys::Off),
    ] {
        let g = drive_new(2, 2, bytes);
        assert_eq!(g.mode_snapshot().modify_other_keys, level, "{case}");
    }
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

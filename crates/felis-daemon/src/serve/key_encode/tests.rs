use felis_protocol::messages::FKey;

use super::*;

fn lit(s: &str) -> Key {
    Key::Character(s.into())
}

/// Every argument of [`super::encode`], with the event kind as the
/// press flag the legacy encodings are specified in terms of.
#[allow(clippy::fn_params_excessive_bools)]
fn encode(
    key: &Key,
    text: Option<&str>,
    mods: Modifiers,
    is_press: bool,
    kitty_kbd_flags: KittyKbdFlags,
    modify_other_keys: ModifyOtherKeys,
    application_cursor: bool,
    application_keypad: bool,
    win32_input_mode: bool,
    key_location: KeyLocation,
) -> Option<Vec<u8>> {
    super::encode(
        key,
        text,
        mods,
        if is_press {
            KeyEventKind::Press
        } else {
            KeyEventKind::Release
        },
        kitty_kbd_flags,
        modify_other_keys,
        application_cursor,
        application_keypad,
        win32_input_mode,
        key_location,
    )
}

fn encode_no_app_cursor(
    key: &Key,
    text: Option<&str>,
    mods: Modifiers,
    is_press: bool,
    kitty_kbd_flags: KittyKbdFlags,
) -> Option<Vec<u8>> {
    encode(
        key,
        text,
        mods,
        is_press,
        kitty_kbd_flags,
        /* modify_other_keys = */ ModifyOtherKeys::Off,
        /* application_cursor = */ false,
        /* application_keypad = */ false,
        /* win32_input_mode = */ false,
        /* key_location = */ KeyLocation::Standard,
    )
}

fn encode_mok(
    key: &Key,
    text: Option<&str>,
    mods: Modifiers,
    modify_other_keys: ModifyOtherKeys,
) -> Option<Vec<u8>> {
    encode(
        key,
        text,
        mods,
        /* is_press = */ true,
        /* kitty_kbd_flags = */ KittyKbdFlags::empty(),
        modify_other_keys,
        /* application_cursor = */ false,
        /* application_keypad = */ false,
        /* win32_input_mode = */ false,
        /* key_location = */ KeyLocation::Standard,
    )
}

#[test]
fn ascii_letter_passes_text_through() {
    let bytes = encode_no_app_cursor(
        &lit("a"),
        Some("a"),
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(b"a".to_vec()));
}

#[test]
fn ctrl_a_maps_to_soh() {
    let bytes = encode_no_app_cursor(
        &lit("a"),
        Some("a"),
        Modifiers::CONTROL,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(vec![0x01]));
}

#[test]
fn ctrl_uppercase_a_also_maps_to_soh() {
    let bytes = encode_no_app_cursor(
        &lit("A"),
        Some("A"),
        Modifiers::CONTROL,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(vec![0x01]));
}

#[test]
fn ctrl_left_bracket_maps_to_esc() {
    let bytes = encode_no_app_cursor(
        &lit("["),
        Some("["),
        Modifiers::CONTROL,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(vec![0x1b]));
}

#[test]
fn alt_letter_emits_meta_prefixed_lowercase() {
    let bytes = encode_no_app_cursor(
        &lit("b"),
        Some("b"),
        Modifiers::ALT,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(vec![0x1b, b'b']));
}

#[test]
fn alt_uppercase_letter_keeps_its_case_after_meta_prefix() {
    let bytes = encode_no_app_cursor(
        &lit("B"),
        Some("B"),
        Modifiers::ALT | Modifiers::SHIFT,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(vec![0x1b, b'B']));
}

#[test]
fn ctrl_space_and_backspace_emit_xterm_c0() {
    for (key, mods, expected) in [
        (NamedKey::Space, Modifiers::CONTROL, &[0x00][..]),
        (
            NamedKey::Space,
            Modifiers::CONTROL | Modifiers::SHIFT,
            &[0x00][..],
        ),
        (NamedKey::Backspace, Modifiers::CONTROL, &[0x08][..]),
        (
            NamedKey::Backspace,
            Modifiers::CONTROL | Modifiers::SHIFT,
            &[0x08][..],
        ),
        (
            NamedKey::Space,
            Modifiers::CONTROL | Modifiers::ALT,
            &[0x1b, 0x00][..],
        ),
        (
            NamedKey::Backspace,
            Modifiers::CONTROL | Modifiers::ALT,
            &[0x1b, 0x08][..],
        ),
        (
            NamedKey::Space,
            Modifiers::CONTROL | Modifiers::SUPER,
            &b" "[..],
        ),
    ] {
        let bytes =
            encode_no_app_cursor(&Key::Named(key), None, mods, true, KittyKbdFlags::empty());
        assert_eq!(bytes.as_deref(), Some(expected), "{key:?} {mods:?}");
    }
}

#[test]
fn ctrl_digit_and_punctuation_follow_xterm_table() {
    for (s, expected) in [
        ("2", 0x00),
        ("3", 0x1b),
        ("4", 0x1c),
        ("5", 0x1d),
        ("6", 0x1e),
        ("7", 0x1f),
        ("8", 0x7f),
        ("/", 0x1f),
        ("?", 0x7f),
    ] {
        let bytes = encode_no_app_cursor(
            &lit(s),
            Some(s),
            Modifiers::CONTROL,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![expected]), "Ctrl+{s}");
        let bytes = encode_no_app_cursor(
            &lit(s),
            Some(s),
            Modifiers::CONTROL | Modifiers::ALT,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![0x1b, expected]), "Ctrl+Alt+{s}");
    }
}

#[test]
fn ctrl_digit_outside_xterm_table_passes_text_through() {
    for s in ["1", "9", "0"] {
        let bytes = encode_no_app_cursor(
            &lit(s),
            Some(s),
            Modifiers::CONTROL,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(s.as_bytes().to_vec()), "Ctrl+{s}");
    }
}

#[test]
fn modify_other_keys_level1_leaves_fixed_legacy_bytes_to_unescaped_keys() {
    for (key, mods, expected) in [
        (Key::Named(NamedKey::Space), Modifiers::CONTROL, &[0x00][..]),
        (
            Key::Named(NamedKey::Backspace),
            Modifiers::CONTROL,
            &[0x08][..],
        ),
        (
            lit("B"),
            Modifiers::ALT | Modifiers::SHIFT,
            &[0x1b, b'B'][..],
        ),
    ] {
        let bytes = encode_mok(&key, None, mods, ModifyOtherKeys::Level1);
        assert_eq!(bytes.as_deref(), Some(expected), "{key:?} {mods:?}");
    }
}

#[test]
fn kitty_flags_without_csi_u_leave_fixed_legacy_bytes() {
    for (key, mods, expected) in [
        (Key::Named(NamedKey::Space), Modifiers::CONTROL, &[0x00][..]),
        (
            lit("B"),
            Modifiers::ALT | Modifiers::SHIFT,
            &[0x1b, b'B'][..],
        ),
    ] {
        let bytes = encode_no_app_cursor(&key, None, mods, true, KittyKbdFlags::REPORT_EVENT_TYPES);
        assert_eq!(bytes.as_deref(), Some(expected), "{key:?} {mods:?}");
    }
}

#[test]
fn modify_other_keys_level1_still_escapes_ctrl_digit() {
    let bytes = encode_mok(
        &lit("2"),
        Some("2"),
        Modifiers::CONTROL,
        ModifyOtherKeys::Level1,
    );
    assert_eq!(bytes, Some(b"\x1b[50;5u".to_vec()));
}

#[test]
fn alt_period_emits_meta_prefixed_dot() {
    let bytes = encode_no_app_cursor(
        &lit("."),
        Some("."),
        Modifiers::ALT,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(vec![0x1b, b'.']));
}

#[test]
fn ctrl_alt_letter_emits_meta_prefixed_c0() {
    let mods = Modifiers::CONTROL | Modifiers::ALT;
    let bytes = encode_no_app_cursor(&lit("a"), Some("a"), mods, true, KittyKbdFlags::empty());
    assert_eq!(bytes, Some(vec![0x1b, 0x01]));
}

#[test]
fn alt_non_ascii_falls_through_to_text_passthrough() {
    let bytes = encode_no_app_cursor(
        &lit("α"),
        Some("α"),
        Modifiers::ALT,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(
        bytes,
        Some("α".as_bytes().to_vec()),
        "non-ASCII Alt+letter must pass through, no spurious ESC"
    );
}

#[test]
fn alt_named_keys_meta_prefix_their_bare_body() {
    let cases: [(NamedKey, &[u8]); 4] = [
        (NamedKey::Backspace, &[0x1b, 0x7f]),
        (NamedKey::Space, b"\x1b "),
        (NamedKey::Escape, &[0x1b, 0x1b]),
        (NamedKey::Tab, b"\x1b\t"),
    ];
    for (key, expected) in cases {
        let bytes = encode_no_app_cursor(
            &Key::Named(key),
            None,
            Modifiers::ALT,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(
            bytes.as_deref(),
            Some(expected),
            "{key:?} under Alt must meta-prefix"
        );
    }
}

#[test]
fn alt_named_keys_super_held_skips_meta_prefix() {
    let mods = Modifiers::ALT | Modifiers::SUPER;
    assert_eq!(
        encode_no_app_cursor(
            &Key::Named(NamedKey::Space),
            None,
            mods,
            true,
            KittyKbdFlags::empty()
        )
        .as_deref(),
        Some(&b" "[..]),
        "Super must short-circuit Space meta-prefix"
    );
}

#[test]
fn shift_tab_takes_priority_over_alt_meta_prefix() {
    let mods = Modifiers::ALT | Modifiers::SHIFT;
    assert_eq!(
        encode_no_app_cursor(
            &Key::Named(NamedKey::Tab),
            None,
            mods,
            true,
            KittyKbdFlags::empty()
        )
        .as_deref(),
        Some(&b"\x1b[Z"[..]),
        "Shift-Tab path must outrank the Alt meta-prefix"
    );
}

#[test]
fn unmodified_named_keys_keep_their_bare_byte() {
    for (key, expected) in [
        (NamedKey::Enter, &b"\r"[..]),
        (NamedKey::Backspace, &[0x7f][..]),
        (NamedKey::Tab, &b"\t"[..]),
        (NamedKey::Escape, &[0x1b][..]),
        (NamedKey::Space, &b" "[..]),
    ] {
        let bytes = encode_no_app_cursor(
            &Key::Named(key),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes.as_deref(), Some(expected), "{key:?} bare form");
    }
}

#[test]
fn alt_with_super_falls_through_to_text() {
    let mods = Modifiers::ALT | Modifiers::SUPER;
    let bytes = encode_no_app_cursor(&lit("a"), Some("a"), mods, true, KittyKbdFlags::empty());
    assert_eq!(
        bytes,
        Some(b"a".to_vec()),
        "Super present must short-circuit the meta-prefix path"
    );
}

#[test]
fn enter_yields_carriage_return() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(b"\r".to_vec()));
}

#[test]
fn alt_enter_prepends_escape() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::ALT,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(b"\x1b\r".to_vec()));
}

#[test]
fn backspace_yields_del() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Backspace),
        None,
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(vec![0x7f]));
}

#[test]
fn unmodified_cursor_and_editing_keys_keep_bare_legacy_form() {
    for (key, expected) in [
        (NamedKey::ArrowUp, &b"\x1b[A"[..]),
        (NamedKey::ArrowDown, &b"\x1b[B"[..]),
        (NamedKey::ArrowRight, &b"\x1b[C"[..]),
        (NamedKey::ArrowLeft, &b"\x1b[D"[..]),
        (NamedKey::Home, &b"\x1b[H"[..]),
        (NamedKey::End, &b"\x1b[F"[..]),
        (NamedKey::PageUp, &b"\x1b[5~"[..]),
        (NamedKey::Delete, &b"\x1b[3~"[..]),
    ] {
        let bytes = encode_no_app_cursor(
            &Key::Named(key),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes.as_deref(), Some(expected), "{key:?}");
    }
}

#[test]
fn f1_through_f4_emit_ss3_bare_form() {
    let cases: [(NamedKey, &[u8]); 4] = [
        (NamedKey::F(FKey::lit(1)), b"\x1bOP"),
        (NamedKey::F(FKey::lit(2)), b"\x1bOQ"),
        (NamedKey::F(FKey::lit(3)), b"\x1bOR"),
        (NamedKey::F(FKey::lit(4)), b"\x1bOS"),
    ];
    for (key, expected) in cases {
        let bytes = encode_no_app_cursor(
            &Key::Named(key),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes.as_deref(), Some(expected), "{key:?}");
    }
}

#[test]
fn f5_through_f12_emit_csi_tilde_with_xterm_numbers() {
    let cases: [(NamedKey, &[u8]); 8] = [
        (NamedKey::F(FKey::lit(5)), b"\x1b[15~"),
        (NamedKey::F(FKey::lit(6)), b"\x1b[17~"),
        (NamedKey::F(FKey::lit(7)), b"\x1b[18~"),
        (NamedKey::F(FKey::lit(8)), b"\x1b[19~"),
        (NamedKey::F(FKey::lit(9)), b"\x1b[20~"),
        (NamedKey::F(FKey::lit(10)), b"\x1b[21~"),
        (NamedKey::F(FKey::lit(11)), b"\x1b[23~"),
        (NamedKey::F(FKey::lit(12)), b"\x1b[24~"),
    ];
    for (key, expected) in cases {
        let bytes = encode_no_app_cursor(
            &Key::Named(key),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes.as_deref(), Some(expected), "{key:?}");
    }
}

#[test]
fn decckm_swaps_bare_arrows_to_ss3_form() {
    let cases: [(NamedKey, &[u8]); 4] = [
        (NamedKey::ArrowUp, b"\x1bOA"),
        (NamedKey::ArrowDown, b"\x1bOB"),
        (NamedKey::ArrowRight, b"\x1bOC"),
        (NamedKey::ArrowLeft, b"\x1bOD"),
    ];
    for (key, expected) in cases {
        let bytes = encode(
            &Key::Named(key),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            /* modify_other_keys = */ ModifyOtherKeys::Off,
            /* application_cursor = */ true,
            /* application_keypad = */ false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        );
        assert_eq!(bytes.as_deref(), Some(expected), "{key:?}");
    }
}

#[test]
fn decckm_swaps_bare_home_end_to_ss3_form() {
    assert_eq!(
        encode(
            &Key::Named(NamedKey::Home),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            true,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        )
        .as_deref(),
        Some(&b"\x1bOH"[..])
    );
    assert_eq!(
        encode(
            &Key::Named(NamedKey::End),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            true,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        )
        .as_deref(),
        Some(&b"\x1bOF"[..])
    );
}

#[test]
fn decckm_does_not_affect_tilde_or_function_keys() {
    let pgup = encode(
        &Key::Named(NamedKey::PageUp),
        None,
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
        ModifyOtherKeys::Off,
        true,
        false,
        /* win32_input_mode = */ false,
        KeyLocation::Standard,
    );
    assert_eq!(
        pgup.as_deref(),
        Some(&b"\x1b[5~"[..]),
        "tilde-final key unchanged by DECCKM"
    );
    let f1 = encode(
        &Key::Named(NamedKey::F(FKey::lit(1))),
        None,
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
        ModifyOtherKeys::Off,
        true,
        false,
        /* win32_input_mode = */ false,
        KeyLocation::Standard,
    );
    assert_eq!(
        f1.as_deref(),
        Some(&b"\x1bOP"[..]),
        "F1 stays on its native SS3 form regardless of DECCKM"
    );
}

#[test]
fn application_keypad_numpad_digits_emit_ss3() {
    let digits: [(&str, &[u8]); 11] = [
        ("0", b"\x1bOp"),
        ("1", b"\x1bOq"),
        ("2", b"\x1bOr"),
        ("3", b"\x1bOs"),
        ("4", b"\x1bOt"),
        ("5", b"\x1bOu"),
        ("6", b"\x1bOv"),
        ("7", b"\x1bOw"),
        ("8", b"\x1bOx"),
        ("9", b"\x1bOy"),
        (".", b"\x1bOn"),
    ];
    for (ch, expected) in digits {
        let out = encode(
            &Key::Character(ch.into()),
            Some(ch),
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            /* application_cursor = */ false,
            /* application_keypad = */ true,
            /* win32_input_mode = */ false,
            KeyLocation::Numpad,
        );
        assert_eq!(out.as_deref(), Some(expected), "numpad {ch} under DECKPAM");
    }
    let enter = encode(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
        ModifyOtherKeys::Off,
        /* application_cursor = */ false,
        /* application_keypad = */ true,
        /* win32_input_mode = */ false,
        KeyLocation::Numpad,
    );
    assert_eq!(
        enter.as_deref(),
        Some(&b"\x1bOM"[..]),
        "numpad Enter under DECKPAM"
    );
}

#[test]
fn application_keypad_off_numpad_digit_passes_through() {
    let digit = encode(
        &Key::Character("0".into()),
        Some("0"),
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
        ModifyOtherKeys::Off,
        /* application_cursor = */ false,
        /* application_keypad = */ false,
        /* win32_input_mode = */ false,
        KeyLocation::Numpad,
    );
    assert_eq!(digit.as_deref(), Some(&b"0"[..]));
    let enter = encode(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
        ModifyOtherKeys::Off,
        /* application_cursor = */ false,
        /* application_keypad = */ false,
        /* win32_input_mode = */ false,
        KeyLocation::Numpad,
    );
    assert_eq!(enter.as_deref(), Some(&b"\r"[..]));
}

#[test]
fn application_keypad_ignored_without_numpad_location() {
    let out = encode(
        &Key::Character("1".into()),
        Some("1"),
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
        ModifyOtherKeys::Off,
        /* application_cursor = */ false,
        /* application_keypad = */ true,
        /* win32_input_mode = */ false,
        KeyLocation::Standard,
    );
    assert_eq!(out.as_deref(), Some(&b"1"[..]));
}

#[test]
fn kitty_flags_supersede_application_keypad() {
    let out = encode(
        &Key::Character("1".into()),
        Some("1"),
        Modifiers::empty(),
        true,
        KittyKbdFlags::DISAMBIGUATE,
        ModifyOtherKeys::Off,
        /* application_cursor = */ false,
        /* application_keypad = */ true,
        /* win32_input_mode = */ false,
        KeyLocation::Numpad,
    );
    assert_eq!(out.as_deref(), Some(&b"1"[..]));
}

#[test]
fn xterm_mod_param_truth_table() {
    assert_eq!(xterm_mod_param(Modifiers::empty()), None);
    assert_eq!(xterm_mod_param(Modifiers::SHIFT), Some(2));
    assert_eq!(xterm_mod_param(Modifiers::ALT), Some(3));
    assert_eq!(xterm_mod_param(Modifiers::SHIFT | Modifiers::ALT), Some(4));
    assert_eq!(xterm_mod_param(Modifiers::CONTROL), Some(5));
    assert_eq!(
        xterm_mod_param(Modifiers::CONTROL | Modifiers::SHIFT),
        Some(6)
    );
    assert_eq!(
        xterm_mod_param(Modifiers::CONTROL | Modifiers::ALT),
        Some(7)
    );
    assert_eq!(
        xterm_mod_param(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT),
        Some(8)
    );
    assert_eq!(xterm_mod_param(Modifiers::SUPER), Some(9));
}

#[test]
fn shift_tab_emits_csi_z() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Tab),
        None,
        Modifiers::SHIFT,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
}

#[test]
fn modifier_only_press_returns_none() {
    let bytes = encode_no_app_cursor(
        &Key::Other,
        None,
        Modifiers::SHIFT,
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, None);
}

#[test]
fn esc_legacy_when_disambiguate_flag_is_off() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::empty(),
        true,
        KittyKbdFlags::empty(),
    );
    assert_eq!(bytes, Some(vec![0x1b]));
}

#[test]
fn esc_kitty_csi_27_u_when_disambiguate_flag_is_on() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::empty(),
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[27u".to_vec()));
}

#[test]
fn esc_kitty_with_alt_modifier_appends_modifier_param() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::ALT,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[27;3u".to_vec()));
}

#[test]
fn esc_kitty_modifier_code_composes_shift_alt_ctrl() {
    let mods = Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL;
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        None,
        mods,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[27;8u".to_vec()));
}

#[test]
fn ctrl_letter_under_bit0_uses_csi_u_with_lowercase_codepoint() {
    let bytes = encode_no_app_cursor(
        &lit("I"),
        Some("I"),
        Modifiers::CONTROL,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[105;5u".to_vec()));
    let bytes = encode_no_app_cursor(
        &lit("i"),
        Some("i"),
        Modifiers::CONTROL,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[105;5u".to_vec()));
}

#[test]
fn alt_letter_under_bit0_uses_csi_u_instead_of_esc_prefix() {
    let bytes = encode_no_app_cursor(
        &lit("a"),
        Some("a"),
        Modifiers::ALT,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[97;3u".to_vec()));
}

#[test]
fn ctrl_alt_letter_under_bit0_combines_modifier_bits() {
    let mods = Modifiers::CONTROL | Modifiers::ALT;
    let bytes = encode_no_app_cursor(
        &lit("a"),
        Some("a"),
        mods,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[97;7u".to_vec()));
}

#[test]
fn shift_only_letter_under_bit0_keeps_text_path() {
    let bytes = encode_no_app_cursor(
        &lit("A"),
        Some("A"),
        Modifiers::SHIFT,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"A".to_vec()));
}

#[test]
fn unmodified_tab_enter_backspace_under_bit0_keep_legacy_bytes() {
    let cases: [(NamedKey, &[u8]); 3] = [
        (NamedKey::Tab, b"\t"),
        (NamedKey::Enter, b"\r"),
        (NamedKey::Backspace, &[0x7f]),
    ];
    for (k, expected) in cases {
        let bytes = encode_no_app_cursor(
            &Key::Named(k),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes.as_deref(), Some(expected), "{k:?}");
    }
}

#[test]
fn modified_tab_enter_backspace_under_bit0_use_csi_u() {
    let cases: [(NamedKey, &[u8]); 3] = [
        (NamedKey::Tab, b"\x1b[9;5u"),
        (NamedKey::Enter, b"\x1b[13;5u"),
        (NamedKey::Backspace, b"\x1b[127;5u"),
    ];
    for (k, expected) in cases {
        let bytes = encode_no_app_cursor(
            &Key::Named(k),
            None,
            Modifiers::CONTROL,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes.as_deref(), Some(expected), "{k:?}");
    }
}

#[test]
fn shift_enter_under_bit0_emits_csi_13_2_u() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::SHIFT,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[13;2u".to_vec()));
}

#[test]
fn shift_backspace_under_bit0_emits_csi_127_2_u() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Backspace),
        None,
        Modifiers::SHIFT,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[127;2u".to_vec()));
}

#[test]
fn shift_tab_under_bit0_keeps_csi_z_legacy_form() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Tab),
        None,
        Modifiers::SHIFT,
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
}

#[test]
fn shift_tab_under_bit3_does_route_through_csi_u() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Tab),
        None,
        Modifiers::SHIFT,
        true,
        KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
    );
    assert_eq!(bytes, Some(b"\x1b[9;2u".to_vec()));
}

#[test]
fn release_drops_when_event_types_flag_is_off() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::empty(),
        false,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, None);
}

#[test]
fn release_drops_when_disambiguate_is_off_even_with_event_types() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::empty(),
        false,
        KittyKbdFlags::REPORT_EVENT_TYPES,
    );
    assert_eq!(bytes, None);
}

#[test]
fn esc_release_under_bits_0_and_1_emits_csi_with_event_subparam() {
    let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::empty(),
        false,
        flags,
    );
    assert_eq!(bytes, Some(b"\x1b[27;1:3u".to_vec()));
}

#[test]
fn esc_release_with_modifier_carries_mod_and_event() {
    let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::ALT,
        false,
        flags,
    );
    assert_eq!(bytes, Some(b"\x1b[27;3:3u".to_vec()));
}

#[test]
fn ctrl_letter_release_uses_csi_u_with_event_subparam() {
    let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
    let bytes = encode_no_app_cursor(&lit("i"), Some("i"), Modifiers::CONTROL, false, flags);
    assert_eq!(bytes, Some(b"\x1b[105;5:3u".to_vec()));
}

#[test]
fn release_of_text_path_key_is_dropped() {
    let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
    let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::empty(), false, flags);
    assert_eq!(bytes, None);
}

#[test]
fn ctrl_letter_with_associated_text_appends_codepoint() {
    let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
    let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::CONTROL, true, flags);
    assert_eq!(bytes, Some(b"\x1b[97;5;97u".to_vec()));
}

#[test]
fn esc_press_with_associated_text_off_omits_codepoints() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        Some("\x1b"),
        Modifiers::empty(),
        true,
        KittyKbdFlags::DISAMBIGUATE,
    );
    assert_eq!(bytes, Some(b"\x1b[27u".to_vec()));
}

#[test]
fn esc_press_with_associated_text_forces_mod_field() {
    let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        Some("X"),
        Modifiers::empty(),
        true,
        flags,
    );
    assert_eq!(bytes, Some(b"\x1b[27;1;88u".to_vec()));
}

#[test]
fn associated_text_release_does_not_emit_text() {
    let flags = KittyKbdFlags::DISAMBIGUATE
        | KittyKbdFlags::REPORT_EVENT_TYPES
        | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
    let bytes = encode_no_app_cursor(&lit("i"), Some("i"), Modifiers::CONTROL, false, flags);
    assert_eq!(bytes, Some(b"\x1b[105;5:3u".to_vec()));
}

#[test]
fn bit3_routes_unmodified_letter_through_csi_u() {
    let bytes = encode_no_app_cursor(
        &lit("a"),
        Some("a"),
        Modifiers::empty(),
        true,
        KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
    );
    assert_eq!(bytes, Some(b"\x1b[97u".to_vec()));
}

#[test]
fn bit3_implies_bit0_so_unmodified_esc_csi_u() {
    let bytes = encode_no_app_cursor(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::empty(),
        true,
        KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
    );
    assert_eq!(bytes, Some(b"\x1b[27u".to_vec()));
}

#[test]
fn bit3_unmodified_enter_tab_backspace_use_csi_u() {
    let cases: [(NamedKey, &[u8]); 3] = [
        (NamedKey::Tab, b"\x1b[9u"),
        (NamedKey::Enter, b"\x1b[13u"),
        (NamedKey::Backspace, b"\x1b[127u"),
    ];
    for (k, expected) in cases {
        let bytes = encode_no_app_cursor(
            &Key::Named(k),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
        );
        assert_eq!(bytes.as_deref(), Some(expected), "{k:?}");
    }
}

#[test]
fn bit3_with_shift_letter_keeps_lowercase_keycode_with_shift_mod() {
    let bytes = encode_no_app_cursor(
        &lit("A"),
        Some("A"),
        Modifiers::SHIFT,
        true,
        KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
    );
    assert_eq!(bytes, Some(b"\x1b[97;2u".to_vec()));
}

#[test]
fn bit3_with_associated_text_appends_codepoints() {
    let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
    let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::empty(), true, flags);
    assert_eq!(bytes, Some(b"\x1b[97;1;97u".to_vec()));
}

#[test]
fn bit2_shift_letter_appends_uppercase_alt_keycode() {
    let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_ALTERNATE_KEYS;
    let bytes = encode_no_app_cursor(&lit("A"), Some("A"), Modifiers::SHIFT, true, flags);
    assert_eq!(bytes, Some(b"\x1b[97:65;2u".to_vec()));
}

#[test]
fn bit2_unshifted_letter_omits_alt() {
    let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_ALTERNATE_KEYS;
    let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::empty(), true, flags);
    assert_eq!(bytes, Some(b"\x1b[97u".to_vec()));
}

#[test]
fn bit2_non_letter_shift_omits_alt() {
    let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_ALTERNATE_KEYS;
    let bytes = encode_no_app_cursor(&lit("!"), Some("!"), Modifiers::SHIFT, true, flags);
    assert_eq!(bytes, Some(b"\x1b[33;2u".to_vec()));
}

#[test]
fn bit2_release_drops_alt_subparam() {
    let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES
        | KittyKbdFlags::REPORT_ALTERNATE_KEYS
        | KittyKbdFlags::REPORT_EVENT_TYPES;
    let bytes = encode_no_app_cursor(&lit("A"), Some("A"), Modifiers::SHIFT, false, flags);
    assert_eq!(bytes, Some(b"\x1b[97;2:3u".to_vec()));
}

#[test]
fn bit3_release_under_event_types_emits_csi_u_with_event_subparam() {
    let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_EVENT_TYPES;
    let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::empty(), false, flags);
    assert_eq!(bytes, Some(b"\x1b[97;1:3u".to_vec()));
}

#[test]
fn option_composed_characters_arrive_as_typed_text() {
    let none = Modifiers::empty();
    for flags in [KittyKbdFlags::empty(), KittyKbdFlags::DISAMBIGUATE] {
        for typed in ["@", "{", "\u{a0}"] {
            let bytes = encode_no_app_cursor(&lit(typed), Some(typed), none, true, flags);
            assert_eq!(
                bytes,
                Some(typed.as_bytes().to_vec()),
                "{typed:?} {flags:?}"
            );
        }
    }
    let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_EVENT_TYPES;
    let press = encode_no_app_cursor(&lit("@"), Some("@"), none, true, flags);
    let release = encode_no_app_cursor(&lit("@"), None, none, false, flags);
    assert_eq!(press, Some(b"\x1b[64u".to_vec()));
    assert_eq!(release, Some(b"\x1b[64;1:3u".to_vec()));
}

#[test]
fn multi_codepoint_associated_text_uses_colon_separator() {
    let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
    let bytes = encode_no_app_cursor(&lit("a"), Some("AB"), Modifiers::CONTROL, true, flags);
    assert_eq!(bytes, Some(b"\x1b[97;5;65:66u".to_vec()));
}

// modifyOtherKeys: a program that could not enable the Kitty protocol
// (Claude Code gates it on a TERM_PROGRAM allowlist) falls back to
// level 2 and expects Shift+Enter as a distinct sequence (REQ-506).

#[test]
fn shift_enter_under_mok_level_2_emits_csi_13_2_u() {
    let bytes = encode_mok(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::SHIFT,
        ModifyOtherKeys::Level2,
    );
    assert_eq!(bytes, Some(b"\x1b[13;2u".to_vec()));
}

#[test]
fn shift_enter_under_mok_level_1_stays_carriage_return() {
    let bytes = encode_mok(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::SHIFT,
        ModifyOtherKeys::Level1,
    );
    assert_eq!(bytes, Some(b"\r".to_vec()));
}

#[test]
fn shift_enter_under_mok_level_0_stays_carriage_return() {
    let bytes = encode_mok(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::SHIFT,
        ModifyOtherKeys::Off,
    );
    assert_eq!(bytes, Some(b"\r".to_vec()));
}

#[test]
fn bare_enter_under_mok_level_2_stays_carriage_return() {
    let bytes = encode_mok(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::empty(),
        ModifyOtherKeys::Level2,
    );
    assert_eq!(bytes, Some(b"\r".to_vec()));
}

#[test]
fn bare_escape_under_mok_level_2_stays_esc() {
    let bytes = encode_mok(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::empty(),
        ModifyOtherKeys::Level2,
    );
    assert_eq!(bytes, Some(vec![0x1b]));
}

#[test]
fn modified_escape_under_mok_level_2_emits_csi_27_u() {
    let bytes = encode_mok(
        &Key::Named(NamedKey::Escape),
        None,
        Modifiers::CONTROL,
        ModifyOtherKeys::Level2,
    );
    assert_eq!(bytes, Some(b"\x1b[27;5u".to_vec()));
}

#[test]
fn ctrl_c_under_mok_level_1_keeps_its_control_byte() {
    let bytes = encode_mok(
        &lit("c"),
        Some("c"),
        Modifiers::CONTROL,
        ModifyOtherKeys::Level1,
    );
    assert_eq!(bytes, Some(vec![0x03]));
}

#[test]
fn ctrl_c_under_mok_level_2_routes_csi_u() {
    let bytes = encode_mok(
        &lit("c"),
        Some("c"),
        Modifiers::CONTROL,
        ModifyOtherKeys::Level2,
    );
    assert_eq!(bytes, Some(b"\x1b[99;5u".to_vec()));
}

#[test]
fn ctrl_digit_under_mok_level_1_routes_csi_u() {
    let bytes = encode_mok(
        &lit("1"),
        Some("1"),
        Modifiers::CONTROL,
        ModifyOtherKeys::Level1,
    );
    assert_eq!(bytes, Some(b"\x1b[49;5u".to_vec()));
}

#[test]
fn alt_letter_under_mok_level_1_keeps_meta_prefix() {
    let bytes = encode_mok(
        &lit("b"),
        Some("b"),
        Modifiers::ALT,
        ModifyOtherKeys::Level1,
    );
    assert_eq!(bytes, Some(vec![0x1b, b'b']));
}

#[test]
fn alt_letter_under_mok_level_2_routes_csi_u() {
    let bytes = encode_mok(
        &lit("b"),
        Some("b"),
        Modifiers::ALT,
        ModifyOtherKeys::Level2,
    );
    assert_eq!(bytes, Some(b"\x1b[98;3u".to_vec()));
}

#[test]
fn shift_only_letter_under_mok_level_2_stays_shifted_char() {
    let bytes = encode_mok(
        &lit("A"),
        Some("A"),
        Modifiers::SHIFT,
        ModifyOtherKeys::Level2,
    );
    assert_eq!(bytes, Some(b"A".to_vec()));
}

#[test]
fn ctrl_tab_backspace_space_under_mok_level_2_route_csi_u() {
    let cases: [(NamedKey, &[u8]); 3] = [
        (NamedKey::Tab, b"\x1b[9;5u"),
        (NamedKey::Backspace, b"\x1b[127;5u"),
        (NamedKey::Space, b"\x1b[32;5u"),
    ];
    for (k, expected) in cases {
        let bytes = encode_mok(
            &Key::Named(k),
            None,
            Modifiers::CONTROL,
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes.as_deref(), Some(expected), "{k:?}");
    }
}

#[test]
fn shift_tab_under_mok_level_2_keeps_csi_z() {
    let bytes = encode_mok(
        &Key::Named(NamedKey::Tab),
        None,
        Modifiers::SHIFT,
        ModifyOtherKeys::Level2,
    );
    assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
}

#[test]
fn modified_arrow_under_mok_stays_xterm_csi_form() {
    let bytes = encode_mok(
        &Key::Named(NamedKey::ArrowRight),
        None,
        Modifiers::CONTROL,
        ModifyOtherKeys::Level2,
    );
    assert_eq!(bytes, Some(b"\x1b[1;5C".to_vec()));
}

#[test]
fn kitty_flags_supersede_modify_other_keys() {
    let bytes = encode(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::SHIFT,
        /* is_press = */ true,
        KittyKbdFlags::REPORT_EVENT_TYPES,
        /* modify_other_keys = */ ModifyOtherKeys::Level2,
        /* application_cursor = */ false,
        /* application_keypad = */ false,
        /* win32_input_mode = */ false,
        /* key_location = */ KeyLocation::Standard,
    );
    assert_eq!(bytes, Some(b"\r".to_vec()));
}

#[test]
fn mok_release_events_produce_nothing() {
    let bytes = encode(
        &Key::Named(NamedKey::Enter),
        None,
        Modifiers::SHIFT,
        false,
        KittyKbdFlags::empty(),
        ModifyOtherKeys::Level2,
        false,
        false,
        false,
        KeyLocation::Standard,
    );
    assert_eq!(bytes, None);
}

/// A string of at most `cap` UTF-8 bytes, which is what admission
/// lets through for a character or its composed text.
fn capped_utf8(cap: usize) -> impl proptest::strategy::Strategy<Value = String> {
    use proptest::strategy::Strategy as _;
    proptest::collection::vec(proptest::char::any(), 0..=cap).prop_map(move |chars| {
        let mut out = String::new();
        for c in chars {
            if out.len() + c.len_utf8() > cap {
                break;
            }
            out.push(c);
        }
        out
    })
}

fn every_named_key() -> Vec<NamedKey> {
    let mut keys = vec![
        NamedKey::Enter,
        NamedKey::Tab,
        NamedKey::Escape,
        NamedKey::Space,
        NamedKey::Backspace,
        NamedKey::Insert,
        NamedKey::Delete,
        NamedKey::Home,
        NamedKey::End,
        NamedKey::PageUp,
        NamedKey::PageDown,
        NamedKey::ArrowUp,
        NamedKey::ArrowDown,
        NamedKey::ArrowLeft,
        NamedKey::ArrowRight,
    ];
    keys.extend((1..=35).map(|n| NamedKey::F(FKey::lit(n))));
    keys
}

proptest::proptest! {

    /// `MAX_KEY_REPORT_BYTES` is what admission reserves before the
    /// mode is known, so no mode may encode past it.
    #[test]
    fn no_mode_encodes_a_capped_key_past_the_reserved_report_size(
        key_idx in 0_usize..52,
        character in capped_utf8(felis_protocol::limits::MAX_KEY_CHARACTER_BYTES),
        text in proptest::option::of(capped_utf8(
            felis_protocol::limits::MAX_KEY_TEXT_BYTES,
        )),
        mods_bits in 0_u8..16,
        kind_idx in 0_usize..3,
        kbd in proptest::num::u8::ANY,
        mok_idx in 0_usize..3,
        application_cursor in proptest::bool::ANY,
        application_keypad in proptest::bool::ANY,
        win32_input_mode in proptest::bool::ANY,
        location_idx in 0_usize..4,
    ) {
        let named = every_named_key();
        let key = match key_idx {
            i if i < named.len() => Key::Named(named[i]),
            i if i == named.len() => Key::Character(character),
            _ => Key::Other,
        };
        let kind = match kind_idx {
            0 => KeyEventKind::Press,
            1 => KeyEventKind::Repeat,
            _ => KeyEventKind::Release,
        };
        let location = match location_idx {
            0 => KeyLocation::Standard,
            1 => KeyLocation::Left,
            2 => KeyLocation::Right,
            _ => KeyLocation::Numpad,
        };
        let modify_other_keys = match mok_idx {
            0 => ModifyOtherKeys::Off,
            1 => ModifyOtherKeys::Level1,
            _ => ModifyOtherKeys::Level2,
        };
        let encoded = super::encode(
            &key,
            text.as_deref(),
            Modifiers::from_bits_truncate(mods_bits),
            kind,
            KittyKbdFlags::from_bits_truncate(kbd),
            modify_other_keys,
            application_cursor,
            application_keypad,
            win32_input_mode,
            location,
        );
        let len = encoded.as_ref().map_or(0, Vec::len);
        proptest::prop_assert!(
            len <= felis_protocol::limits::MAX_KEY_REPORT_BYTES,
            "{key:?} under kbd={kbd} mok={modify_other_keys:?} win32={win32_input_mode} \
                 encoded to {len} bytes, past the reserved \
                 {}",
            felis_protocol::limits::MAX_KEY_REPORT_BYTES,
        );
    }

    #[test]
    fn encode_named_total_across_modifiers_and_modes(
        key_idx in 0_usize..15,
        shift in proptest::bool::ANY,
        alt in proptest::bool::ANY,
        ctrl in proptest::bool::ANY,
        sup in proptest::bool::ANY,
        kbd in proptest::num::u8::ANY,
        mok_idx in 0_usize..3,
        app_cursor in proptest::bool::ANY,
        is_press in proptest::bool::ANY,
    ) {
        let key = match key_idx {
            0 => NamedKey::ArrowUp,
            1 => NamedKey::ArrowDown,
            2 => NamedKey::ArrowLeft,
            3 => NamedKey::ArrowRight,
            4 => NamedKey::Home,
            5 => NamedKey::End,
            6 => NamedKey::PageUp,
            7 => NamedKey::PageDown,
            8 => NamedKey::Insert,
            9 => NamedKey::Delete,
            10 => NamedKey::F(FKey::lit(1)),
            11 => NamedKey::F(FKey::lit(4)),
            12 => NamedKey::F(FKey::lit(5)),
            13 => NamedKey::F(FKey::lit(12)),
            _ => NamedKey::Escape,
        };
        let mut mods = Modifiers::empty();
        if shift { mods |= Modifiers::SHIFT; }
        if alt { mods |= Modifiers::ALT; }
        if ctrl { mods |= Modifiers::CONTROL; }
        if sup { mods |= Modifiers::SUPER; }
        let mok = match mok_idx {
            0 => ModifyOtherKeys::Off,
            1 => ModifyOtherKeys::Level1,
            _ => ModifyOtherKeys::Level2,
        };
        drop(encode(
            &Key::Named(key),
            None,
            mods,
            is_press,
            KittyKbdFlags::from_bits_truncate(kbd),
            mok,
            app_cursor,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        ));
    }

    #[test]
    fn bare_letter_cursor_keys_pick_intro_byte_by_decckm(
        key_idx in 0_usize..6,
        app_cursor in proptest::bool::ANY,
    ) {
        let key = match key_idx {
            0 => NamedKey::ArrowUp,
            1 => NamedKey::ArrowDown,
            2 => NamedKey::ArrowLeft,
            3 => NamedKey::ArrowRight,
            4 => NamedKey::Home,
            _ => NamedKey::End,
        };
        let bytes = encode(
            &Key::Named(key),
            None,
            Modifiers::empty(),
            /* is_press = */ true,
            /* kitty_kbd_flags = */ KittyKbdFlags::empty(),
            /* modify_other_keys = */ ModifyOtherKeys::Off,
            app_cursor,
            /* application_keypad = */ false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        )
        .expect("named cursor key always encodes");
        let intro = if app_cursor { 0x4f } else { 0x5b }; // 'O' or '['
        proptest::prop_assert_eq!(bytes[0], 0x1b);
        proptest::prop_assert_eq!(bytes[1], intro, "intro byte under DECCKM={}", app_cursor);
        proptest::prop_assert_eq!(bytes.len(), 3, "bare form is exactly 3 bytes");
    }

    /// xterm's modified form is the bare form with the modifier
    /// parameter spliced in: the family and the final byte come from
    /// the bare tables, so only the parameter is new. DECCKM swings
    /// the bare form alone.
    #[test]
    fn modified_named_keys_splice_the_modifier_into_the_bare_form(
        key_idx in 0_usize..14,
        shift in proptest::bool::ANY,
        alt in proptest::bool::ANY,
        ctrl in proptest::bool::ANY,
        sup in proptest::bool::ANY,
        app_cursor in proptest::bool::ANY,
    ) {
        proptest::prop_assume!(shift || alt || ctrl || sup);
        let named = match key_idx {
            0 => NamedKey::ArrowUp,
            1 => NamedKey::ArrowDown,
            2 => NamedKey::ArrowLeft,
            3 => NamedKey::ArrowRight,
            4 => NamedKey::Home,
            5 => NamedKey::End,
            6 => NamedKey::PageUp,
            7 => NamedKey::PageDown,
            8 => NamedKey::Insert,
            9 => NamedKey::Delete,
            10 => NamedKey::F(FKey::lit(1)),
            11 => NamedKey::F(FKey::lit(4)),
            12 => NamedKey::F(FKey::lit(5)),
            _ => NamedKey::F(FKey::lit(12)),
        };
        let key = Key::Named(named);
        let mut mods = Modifiers::empty();
        if shift { mods |= Modifiers::SHIFT; }
        if alt { mods |= Modifiers::ALT; }
        if ctrl { mods |= Modifiers::CONTROL; }
        if sup { mods |= Modifiers::SUPER; }

        let bare = encode_no_app_cursor(&key, None, Modifiers::empty(), true, KittyKbdFlags::empty())
            .expect("every key in the xterm tables encodes bare");
        let modified = encode_no_app_cursor(&key, None, mods, true, KittyKbdFlags::empty())
            .expect("every key in the xterm tables encodes modified");
        let under_decckm = encode(
            &key,
            None,
            mods,
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            app_cursor,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        );
        proptest::prop_assert_eq!(under_decckm.as_deref(), Some(&modified[..]));

        let m = xterm_mod_param(mods).expect("a held modifier has an xterm parameter");
        let final_byte = *bare.last().expect("a non-empty encoding");
        let leading = if final_byte == b'~' {
            String::from_utf8(bare[2..bare.len() - 1].to_vec()).expect("digits")
        } else {
            "1".to_owned()
        };
        let expected = format!("\x1b[{leading};{m}{}", char::from(final_byte));
        proptest::prop_assert_eq!(
            String::from_utf8_lossy(&modified),
            expected,
        );
    }

    #[test]
    fn modified_letter_cursor_keys_stay_csi_under_decckm(
        key_idx in 0_usize..6,
        shift in proptest::bool::ANY,
        alt in proptest::bool::ANY,
        ctrl in proptest::bool::ANY,
        app_cursor in proptest::bool::ANY,
    ) {
        proptest::prop_assume!(shift || alt || ctrl);
        let key = match key_idx {
            0 => NamedKey::ArrowUp,
            1 => NamedKey::ArrowDown,
            2 => NamedKey::ArrowLeft,
            3 => NamedKey::ArrowRight,
            4 => NamedKey::Home,
            _ => NamedKey::End,
        };
        let mut mods = Modifiers::empty();
        if shift { mods |= Modifiers::SHIFT; }
        if alt { mods |= Modifiers::ALT; }
        if ctrl { mods |= Modifiers::CONTROL; }
        let bytes = encode(
            &Key::Named(key),
            None,
            mods,
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            app_cursor,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        )
        .expect("named cursor key always encodes");
        proptest::prop_assert!(
            bytes.starts_with(b"\x1b[1;"),
            "modified letter-final must stay CSI even under DECCKM, got {:?}",
            bytes
        );
    }

    #[test]
    fn non_letter_cursor_keys_unaffected_by_decckm(
        key_idx in 0_usize..10,
    ) {
        let key = match key_idx {
            0 => NamedKey::PageUp,
            1 => NamedKey::PageDown,
            2 => NamedKey::Insert,
            3 => NamedKey::Delete,
            4 => NamedKey::F(FKey::lit(1)),
            5 => NamedKey::F(FKey::lit(4)),
            6 => NamedKey::F(FKey::lit(5)),
            7 => NamedKey::F(FKey::lit(8)),
            8 => NamedKey::F(FKey::lit(11)),
            _ => NamedKey::F(FKey::lit(12)),
        };
        let off = encode(
            &Key::Named(key),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            false,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        )
        .expect("encode never None for these named keys");
        let on = encode(
            &Key::Named(key),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            true,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        )
        .expect("encode never None for these named keys");
        proptest::prop_assert_eq!(
            off, on,
            "DECCKM must not change tilde-final / F1-F4 byte shape"
        );
    }

    #[test]
    fn alt_ascii_press_meta_prefixes_when_super_off(
        byte in 0x21_u8..=0x7e_u8,
        shift in proptest::bool::ANY,
        ctrl in proptest::bool::ANY,
    ) {
        let s = std::str::from_utf8(&[byte]).unwrap().to_owned();
        let mut mods = Modifiers::ALT;
        if shift { mods |= Modifiers::SHIFT; }
        if ctrl { mods |= Modifiers::CONTROL; }
        let key = Key::Character(s.clone());
        let bytes = encode_no_app_cursor(&key, Some(s.as_str()), mods, true, KittyKbdFlags::empty())
            .expect("printable ASCII always encodes under Alt");
        let body_resolves = if ctrl {
            legacy_ctrl_byte(&s).is_some()
        } else {
            single_ascii(&s).is_some()
        };
        if body_resolves {
            proptest::prop_assert_eq!(
                bytes[0], 0x1b,
                "Alt+ASCII byte={} mods={:?} must meta-prefix", byte, mods
            );
            proptest::prop_assert_eq!(bytes.len(), 2, "meta-prefixed shape is exactly 2 bytes");
        }
    }

    #[test]
    fn alt_with_super_never_meta_prefixes(
        byte in 0x61_u8..=0x7a_u8, // 'a'..='z'
    ) {
        let s = std::str::from_utf8(&[byte]).unwrap().to_owned();
        let mods = Modifiers::ALT | Modifiers::SUPER;
        let key = Key::Character(s.clone());
        let bytes = encode_no_app_cursor(&key, Some(s.as_str()), mods, true, KittyKbdFlags::empty())
            .expect("ASCII letter always encodes");
        proptest::prop_assert_ne!(
            bytes[0], 0x1b,
            "Super+Alt+{} must not meta-prefix", char::from(byte)
        );
    }

    #[test]
    fn alt_named_press_meta_prefixes_when_super_off(
        key_idx in 0_usize..4,
        shift in proptest::bool::ANY,
        ctrl in proptest::bool::ANY,
    ) {
        let key = match key_idx {
            0 => NamedKey::Backspace,
            1 => NamedKey::Space,
            2 => NamedKey::Escape,
            _ => NamedKey::Tab,
        };
        let mut mods = Modifiers::ALT;
        if shift { mods |= Modifiers::SHIFT; }
        if ctrl { mods |= Modifiers::CONTROL; }
        if matches!(key, NamedKey::Tab) && shift {
            return Ok(());
        }
        let bytes = encode_no_app_cursor(&Key::Named(key), None, mods, true, KittyKbdFlags::empty())
            .expect("named-key arms always encode");
        proptest::prop_assert_eq!(
            bytes[0], 0x1b,
            "Alt+{:?} mods={:?} must meta-prefix", key, mods
        );
        proptest::prop_assert_eq!(
            bytes.len(), 2,
            "Alt+named bare-body shape is exactly 2 bytes (ESC + body)"
        );
    }

    #[test]
    fn mok_bare_keys_match_legacy_across_levels(
        key_idx in 0_usize..16,
        level_idx in 0_usize..3,
    ) {
        let level = match level_idx {
            0 => ModifyOtherKeys::Off,
            1 => ModifyOtherKeys::Level1,
            _ => ModifyOtherKeys::Level2,
        };
        let key = match key_idx {
            0 => Key::Named(NamedKey::Enter),
            1 => Key::Named(NamedKey::Tab),
            2 => Key::Named(NamedKey::Backspace),
            3 => Key::Named(NamedKey::Space),
            4 => Key::Named(NamedKey::Escape),
            5 => Key::Named(NamedKey::ArrowUp),
            6 => Key::Named(NamedKey::Home),
            7 => Key::Named(NamedKey::PageUp),
            8 => Key::Named(NamedKey::F(FKey::lit(1))),
            9 => Key::Named(NamedKey::F(FKey::lit(5))),
            10 => lit("a"),
            11 => lit("Z"),
            12 => lit("1"),
            13 => lit("."),
            14 => lit(" "),
            _ => lit("/"),
        };
        let text = if let Key::Character(s) = &key { Some(s.as_str()) } else { None };
        let legacy = encode(
            &key,
            text,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            false,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        );
        let with_mok = encode(
            &key,
            text,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            level,
            false,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        );
        proptest::prop_assert_eq!(
            legacy, with_mok,
            "bare {:?} must be identical under modifyOtherKeys level {:?}", key, level
        );
    }

    #[test]
    fn mok_level_2_enter_modifier_param_matches_formula(
        shift in proptest::bool::ANY,
        alt in proptest::bool::ANY,
        ctrl in proptest::bool::ANY,
        sup in proptest::bool::ANY,
    ) {
        proptest::prop_assume!(shift || alt || ctrl || sup);
        let mut mods = Modifiers::empty();
        let mut bits = 0u32;
        if shift { mods |= Modifiers::SHIFT; bits |= 1; }
        if alt { mods |= Modifiers::ALT; bits |= 2; }
        if ctrl { mods |= Modifiers::CONTROL; bits |= 4; }
        if sup { mods |= Modifiers::SUPER; bits |= 8; }
        let bytes = encode_mok(&Key::Named(NamedKey::Enter), None, mods, ModifyOtherKeys::Level2)
            .expect("modified Enter always encodes under level 2");
        let expected = format!("\x1b[13;{}u", bits + 1).into_bytes();
        proptest::prop_assert_eq!(bytes, expected);
    }

    #[test]
    fn nonzero_kitty_flags_make_mok_a_noop(
        key_idx in 0_usize..6,
        kbd in 1_u8..=KittyKbdFlags::all().bits(),
        shift in proptest::bool::ANY,
        ctrl in proptest::bool::ANY,
        is_press in proptest::bool::ANY,
    ) {
        let key = match key_idx {
            0 => Key::Named(NamedKey::Enter),
            1 => Key::Named(NamedKey::Tab),
            2 => Key::Named(NamedKey::Escape),
            3 => lit("c"),
            4 => lit("1"),
            _ => Key::Named(NamedKey::Space),
        };
        let mut mods = Modifiers::empty();
        if shift { mods |= Modifiers::SHIFT; }
        if ctrl { mods |= Modifiers::CONTROL; }
        let text = if let Key::Character(s) = &key { Some(s.as_str()) } else { None };
        let without = encode(
            &key,
            text,
            mods,
            is_press,
            KittyKbdFlags::from_bits_truncate(kbd),
            ModifyOtherKeys::Off,
            false,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        );
        let with = encode(
            &key,
            text,
            mods,
            is_press,
            KittyKbdFlags::from_bits_truncate(kbd),
            ModifyOtherKeys::Level2,
            false,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        );
        proptest::prop_assert_eq!(
            without, with,
            "non-zero Kitty flags {} must make modifyOtherKeys a no-op", kbd
        );
    }
}

fn win32(key: &Key, text: Option<&str>, mods: Modifiers, is_press: bool) -> Option<Vec<u8>> {
    encode(
        key,
        text,
        mods,
        is_press,
        /* kitty_kbd_flags = */ KittyKbdFlags::empty(),
        /* modify_other_keys = */ ModifyOtherKeys::Off,
        /* application_cursor = */ false,
        /* application_keypad = */ false,
        /* win32_input_mode = */ true,
        KeyLocation::Standard,
    )
}

#[test]
fn win32_input_letter_reports_vk_char_and_keydown() {
    let out = win32(
        &Key::Character("a".into()),
        Some("a"),
        Modifiers::empty(),
        true,
    );
    assert_eq!(out.as_deref(), Some(&b"\x1b[65;0;97;1;0;1_"[..]));
}

#[test]
fn win32_input_reports_release_with_keydown_zero() {
    let out = win32(
        &Key::Character("a".into()),
        Some("a"),
        Modifiers::empty(),
        false,
    );
    assert_eq!(out.as_deref(), Some(&b"\x1b[65;0;97;0;0;1_"[..]));
}

#[test]
fn win32_input_enter_carries_vk_return_and_cr() {
    let out = win32(&Key::Named(NamedKey::Enter), None, Modifiers::empty(), true);
    assert_eq!(out.as_deref(), Some(&b"\x1b[13;0;13;1;0;1_"[..]));
}

#[test]
fn win32_input_ctrl_c_sets_control_key_state() {
    let out = win32(
        &Key::Character("c".into()),
        Some("c"),
        Modifiers::CONTROL,
        true,
    );
    assert_eq!(out.as_deref(), Some(&b"\x1b[67;0;99;1;8;1_"[..]));
}

#[test]
fn win32_input_shift_sets_shift_pressed_bit() {
    let out = win32(
        &Key::Character("A".into()),
        Some("A"),
        Modifiers::SHIFT,
        true,
    );
    assert_eq!(out.as_deref(), Some(&b"\x1b[65;0;65;1;16;1_"[..]));
}

#[test]
fn win32_input_arrow_reports_vk_with_no_char() {
    let out = win32(
        &Key::Named(NamedKey::ArrowUp),
        None,
        Modifiers::empty(),
        true,
    );
    assert_eq!(out.as_deref(), Some(&b"\x1b[38;0;0;1;0;1_"[..]));
}

#[test]
fn win32_input_bare_modifier_emits_nothing() {
    assert_eq!(win32(&Key::Other, None, Modifiers::SHIFT, true), None);
}

#[test]
fn win32_input_supersedes_kitty_flags() {
    let out = encode(
        &Key::Character("a".into()),
        Some("a"),
        Modifiers::empty(),
        true,
        /* kitty_kbd_flags = */ KittyKbdFlags::DISAMBIGUATE,
        /* modify_other_keys = */ ModifyOtherKeys::Off,
        /* application_cursor = */ false,
        /* application_keypad = */ false,
        /* win32_input_mode = */ true,
        KeyLocation::Standard,
    );
    assert_eq!(out.as_deref(), Some(&b"\x1b[65;0;97;1;0;1_"[..]));
}

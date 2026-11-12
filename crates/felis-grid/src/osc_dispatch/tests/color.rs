use super::*;

#[test]
fn osc_11_query_reports_the_client_configured_background() {
    // A light/dark detector (neovim, modern CLIs) queries `OSC 11 ; ?`;
    // an xterm-white reply on felis's dark surface picks a light
    // colorscheme.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_theme_config_default(ThemeChannel::Background, Some((0x0D, 0x0D, 0x12)));
    drive(&mut p, &mut g, b"\x1b]11;?\x07");
    let replies = responses(&mut g);
    assert_eq!(replies, vec![b"\x1b]11;rgb:0d0d/0d0d/1212\x1b\\".to_vec()]);
}

#[test]
fn osc_10_and_12_queries_report_configured_fg_and_cursor() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_theme_config_default(ThemeChannel::Foreground, Some((0xE5, 0xE5, 0xE5)));
    g.set_theme_config_default(ThemeChannel::Cursor, Some((0xFF, 0xAA, 0x00)));
    drive(&mut p, &mut g, b"\x1b]10;?\x07\x1b]12;?\x07");
    let replies = responses(&mut g);
    assert_eq!(
        replies,
        vec![
            b"\x1b]10;rgb:e5e5/e5e5/e5e5\x1b\\".to_vec(),
            b"\x1b]12;rgb:ffff/aaaa/0000\x1b\\".to_vec(),
        ]
    );
}

#[test]
fn runtime_osc_set_wins_over_the_configured_default() {
    // A colorscheme-toggling shell function must still be able to
    // change the reported background.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_theme_config_default(ThemeChannel::Background, Some((0x0D, 0x0D, 0x12)));
    drive(&mut p, &mut g, b"\x1b]11;#abcdef\x07\x1b]11;?\x07");
    let replies = responses(&mut g);
    assert_eq!(replies, vec![b"\x1b]11;rgb:abab/cdcd/efef\x1b\\".to_vec()]);
}

#[test]
fn osc_111_reset_falls_back_to_the_configured_default_not_xterm_white() {
    // `OSC 111` reverts to this terminal's default: the client's
    // configured background once one is attached, not xterm's white.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_theme_config_default(ThemeChannel::Background, Some((0x0D, 0x0D, 0x12)));
    drive(
        &mut p,
        &mut g,
        b"\x1b]11;#ffffff\x07\x1b]111\x07\x1b]11;?\x07",
    );
    let replies = responses(&mut g);
    assert_eq!(
        replies.last().unwrap(),
        &b"\x1b]11;rgb:0d0d/0d0d/1212\x1b\\".to_vec()
    );
}

#[test]
fn osc_11_query_without_a_configured_default_keeps_the_xterm_baseline() {
    // The esctest path drives a bare grid; `test_ResetSpecialColor_Dynamic`
    // pins the xterm baseline (white bg).
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]11;?\x07");
    let replies = responses(&mut g);
    assert_eq!(replies, vec![b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\".to_vec()]);
}

#[test]
fn ris_keeps_the_client_configured_colors_and_os_preference() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_theme_config_default(ThemeChannel::Background, Some((0x0D, 0x0D, 0x12)));
    g.set_os_dark(true);
    drive(&mut p, &mut g, b"\x1bc\x1b]11;?\x07\x1b[?996n");
    let replies = responses(&mut g);
    assert_eq!(
        replies,
        vec![
            b"\x1b]11;rgb:0d0d/0d0d/1212\x1b\\".to_vec(),
            b"\x1b[?997;1n".to_vec(),
        ]
    );
}

/// The attached windows paint from these deltas; a RIS that dropped
/// the overrides silently would leave them painting what no query
/// answers any more.
#[test]
fn ris_ships_resets_for_the_color_overrides_it_drops() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]4;3;#aabbcc\x07\x1b]11;#102030\x07");
    drop(g.take_palette_dirty());
    let _ = g.take_theme_dirty(ThemeChannel::Background);
    drive(&mut p, &mut g, b"\x1bc");
    let palette = g.take_palette_dirty();
    assert!(palette.reset_all);
    assert_eq!(g.take_theme_dirty(ThemeChannel::Background), Some(None));
    assert_eq!(g.take_theme_dirty(ThemeChannel::Foreground), None);
}

#[test]
fn ris_without_color_overrides_ships_no_color_resets() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1bc");
    assert!(!g.take_palette_dirty().reset_all);
    assert_eq!(g.take_theme_dirty(ThemeChannel::Background), None);
}

#[test]
fn osc_4_query_reports_the_client_configured_palette_slot() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_palette_config_default(1, Some((0xFF, 0x55, 0x55)));
    g.set_palette_config_default(17, Some((0x5E, 0x81, 0xAC)));
    drive(&mut p, &mut g, b"\x1b]4;1;?;17;?;2;?\x07");
    let replies = responses(&mut g);
    assert_eq!(
        replies,
        vec![
            b"\x1b]4;1;rgb:ffff/5555/5555\x1b\\".to_vec(),
            b"\x1b]4;17;rgb:5e5e/8181/acac\x1b\\".to_vec(),
            b"\x1b]4;2;rgb:0000/cdcd/0000\x1b\\".to_vec(),
        ]
    );
}

#[test]
fn runtime_osc_4_set_wins_over_the_configured_palette_slot() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_palette_config_default(1, Some((0xFF, 0x55, 0x55)));
    drive(&mut p, &mut g, b"\x1b]4;1;#abcdef\x07\x1b]4;1;?\x07");
    let replies = responses(&mut g);
    assert_eq!(replies, vec![b"\x1b]4;1;rgb:abab/cdcd/efef\x1b\\".to_vec()]);
}

#[test]
fn osc_104_and_ris_fall_back_to_the_configured_palette_slot() {
    let configured = b"\x1b]4;1;rgb:ffff/5555/5555\x1b\\".to_vec();
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_palette_config_default(1, Some((0xFF, 0x55, 0x55)));
    for reset in [&b"\x1b]104;1\x07"[..], b"\x1b]104\x07", b"\x1bc"] {
        drive(&mut p, &mut g, b"\x1b]4;1;#abcdef\x07");
        drive(&mut p, &mut g, reset);
        drive(&mut p, &mut g, b"\x1b]4;1;?\x07");
        assert_eq!(
            responses(&mut g).last(),
            Some(&configured),
            "after {reset:?}"
        );
    }
}

#[test]
fn special_color_aliases_ignore_the_configured_palette() {
    // `OSC 4 ; 259` aliases special color 3, not palette slot 3.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    let baseline = {
        drive(&mut p, &mut g, b"\x1b]4;259;?\x07\x1b]5;3;?\x07");
        responses(&mut g)
    };
    g.set_palette_config_default(3, Some((0x11, 0x22, 0x33)));
    drive(&mut p, &mut g, b"\x1b]4;259;?\x07\x1b]5;3;?\x07");
    assert_eq!(responses(&mut g), baseline);
}

/// A refused color spec (colorimetric space or `rgbi:`) is dropped at
/// the dispatch layer: the addressed palette slot keeps the value it
/// already had and no reply goes out, even with a `?` query in the
/// same sequence
/// (`docs/reference/protocols/vt-compliance.md` "OSC").
#[test]
fn osc_4_and_5_refused_spec_keeps_the_slot_and_sends_no_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b]4;1;#aabbcc\x1b\\\x1b]5;0;#112233\x1b\\",
    );
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());

    for seq in [
        &b"\x1b]4;1;CIELab:1/1/1\x1b\\"[..],
        b"\x1b]4;1;rgbi:1.0/0.5/0.0\x1b\\",
        b"\x1b]4;1;TekHVC:0/0/0\x1b\\",
        b"\x1b]5;0;CIEXYZ:0.1/0.2/0.3\x1b\\",
    ] {
        drive(&mut p, &mut g, seq);
        assert!(responses(&mut g).is_empty(), "reply to {seq:?}");
    }

    assert_eq!(
        g.palette_overrides().collect::<Vec<_>>(),
        vec![(1, (0xaa, 0xbb, 0xcc))]
    );
    drive(&mut p, &mut g, b"\x1b]4;1;?\x1b\\\x1b]5;0;?\x1b\\");
    assert_eq!(
        responses(&mut g),
        vec![
            b"\x1b]4;1;rgb:aaaa/bbbb/cccc\x1b\\".to_vec(),
            b"\x1b]5;0;rgb:1111/2222/3333\x1b\\".to_vec(),
        ]
    );
}

/// The same refusal on the dynamic colors: OSC 10-12 keep the current
/// override and answer nothing when the spec is colorimetric.
#[test]
fn osc_10_refused_spec_keeps_the_override_and_sends_no_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]10;#aabbcc\x1b\\");

    drive(
        &mut p,
        &mut g,
        b"\x1b]10;CIELuv:1/1/1\x1b\\\x1b]11;rgbi:0/0/0\x1b\\",
    );
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    assert_eq!(
        g.theme_override(ThemeChannel::Foreground),
        Some((0xaa, 0xbb, 0xcc))
    );
    assert_eq!(g.theme_override(ThemeChannel::Background), None);
}

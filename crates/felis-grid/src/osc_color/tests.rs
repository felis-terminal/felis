use super::*;
use crate::test_support::{drive, responses};
use crate::*;
use felis_vt::Parser;

#[test]
fn parse_x_color_accepts_all_xterm_widths_per_channel() {
    // X11 left-justifies each channel into 16 bits and takes the top
    // byte (not the CSS `#abc` → `#aabbcc` digit-doubling): `a` →
    // 0xA0, `ab` → 0xAB, `abc` → 0xAB, `abcd` → 0xAB.
    assert_eq!(parse_x_color("rgb:a/b/c"), Some((0xA0, 0xB0, 0xC0)));
    assert_eq!(parse_x_color("rgb:ab/cd/ef"), Some((0xAB, 0xCD, 0xEF)));
    assert_eq!(parse_x_color("rgb:abc/def/123"), Some((0xAB, 0xDE, 0x12)));
    assert_eq!(
        parse_x_color("rgb:abcd/efab/cdef"),
        Some((0xAB, 0xEF, 0xCD))
    );
}

#[test]
fn parse_x_color_accepts_all_hash_widths() {
    // esctest's ChangeColor_Hash{3,6,9,12}. Hash3 is the case most
    // likely to be mis-implemented as CSS digit-doubling: X11 says
    // `#fff` is the 1-hex-digit form, so `0xF` → top byte `0xF0`.
    assert_eq!(parse_x_color("#fff"), Some((0xF0, 0xF0, 0xF0)));
    assert_eq!(parse_x_color("#abc"), Some((0xA0, 0xB0, 0xC0)));
    assert_eq!(parse_x_color("#f0f0f0"), Some((0xF0, 0xF0, 0xF0)));
    assert_eq!(parse_x_color("#abcdef"), Some((0xAB, 0xCD, 0xEF)));
    assert_eq!(parse_x_color("#f00f00f00"), Some((0xF0, 0xF0, 0xF0)));
    assert_eq!(parse_x_color("#abcdefabc"), Some((0xAB, 0xDE, 0xAB)));
    assert_eq!(parse_x_color("#f000f000f000"), Some((0xF0, 0xF0, 0xF0)));
    assert_eq!(parse_x_color("#aaaabbbbcccc"), Some((0xAA, 0xBB, 0xCC)));
}

#[test]
fn parse_x_color_rejects_alpha_named_and_malformed() {
    assert_eq!(parse_x_color("rgba:11/22/33/ff"), None, "alpha rejected");
    assert_eq!(parse_x_color("red"), None, "named color rejected");
    assert_eq!(parse_x_color("#abcdef00"), None, "8-digit alpha rejected");
    assert_eq!(parse_x_color("#a"), None, "1-digit total rejected");
    assert_eq!(parse_x_color("#abcd"), None, "4-digit not divisible by 3");
    assert_eq!(parse_x_color(""), None, "empty rejected");
    assert_eq!(parse_x_color("rgb:abcde/0/0"), None, "5 digits rejected");
    // A 5-digit channel that still fits in u16: the `s.len() > 4` guard,
    // not the radix parse, must reject it, or `(4 - len) * 4` underflows
    // (pins the `||` in `parse_hex_channel`).
    assert_eq!(
        parse_x_color("rgb:0000f/0/0"),
        None,
        "5 low digits rejected"
    );
    assert_eq!(parse_x_color("rgb:zz/00/00"), None, "non-hex rejected");
    assert_eq!(parse_x_color("rgb:+a/b/c"), None, "sign prefix rejected");
    // `from_str_radix` accepts a leading `+`, a syntax XParseColor does
    // not have; the hexdigit gate in `parse_hex_channel` rejects it.
    assert_eq!(parse_x_color("#+bcdef012345"), None);
    assert_eq!(parse_x_color("rgb:1/2"), None, "two channels rejected");
    assert_eq!(parse_x_color("rgb:1/2/3/4"), None, "four channels rejected");
    assert_eq!(
        parse_x_color("CIELab:1/1/1"),
        None,
        "colorimetric forms rejected"
    );
    assert_eq!(parse_x_color("rgbi:1/1/1"), None, "intensity form rejected");
}

/// esctest's `ChangeColorTests.test_ChangeColor_RGB`.
#[test]
fn osc_4_set_then_query_round_trips_via_rgb_form() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"\x1b]4;0;rgb:f0f0/f0f0/f0f0\x1b\\\x1b]4;0;?\x1b\\",
    );
    let responses = responses(&mut g);
    assert_eq!(responses.len(), 1);
    assert_eq!(
        std::str::from_utf8(&responses[0]).unwrap(),
        "\x1b]4;0;rgb:f0f0/f0f0/f0f0\x1b\\"
    );
}

/// esctest's `test_ChangeColor_Hash3`: X11 Hash3 sets `(0xF0, 0xF0,
/// 0xF0)`, echoed back as the doubled-byte rgb form.
#[test]
fn osc_4_set_via_hash3_form_then_query_echoes_top_byte_doubled() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b]4;0;#fff\x1b\\\x1b]4;0;?\x1b\\");
    let responses = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&responses[0]).unwrap(),
        "\x1b]4;0;rgb:f0f0/f0f0/f0f0\x1b\\"
    );
}

/// Each query echoes as its own response frame so the producer can
/// parse them one at a time.
#[test]
fn osc_4_multi_pair_set_then_multi_pair_query_emits_one_response_per_pair() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"\x1b]4;0;rgb:f0f0/f0f0/f0f0;1;rgb:f0f0/0000/0000\x1b\\\
          \x1b]4;0;?;1;?\x1b\\",
    );
    let responses = responses(&mut g);
    assert_eq!(responses.len(), 2);
    assert_eq!(
        std::str::from_utf8(&responses[0]).unwrap(),
        "\x1b]4;0;rgb:f0f0/f0f0/f0f0\x1b\\"
    );
    assert_eq!(
        std::str::from_utf8(&responses[1]).unwrap(),
        "\x1b]4;1;rgb:f0f0/0000/0000\x1b\\"
    );
}

/// esctest's `ResetColorTests.test_ResetColor_Standard`.
#[test]
fn osc_104_resets_to_default_palette_color() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    // xterm's default palette[3] is yellow `0xCD/0xCD/0x00`.
    drive(&mut p, &mut g, b"\x1b]4;3;?\x1b\\");
    let before = responses(&mut g).pop().unwrap();
    drive(
        &mut p,
        &mut g,
        b"\x1b]4;3;#aabbcc\x1b\\\x1b]4;3;?\x1b\\\
          \x1b]104;3\x1b\\\x1b]4;3;?\x1b\\",
    );
    let responses = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&responses[0]).unwrap(),
        "\x1b]4;3;rgb:aaaa/bbbb/cccc\x1b\\"
    );
    assert_eq!(responses[1], before);
}

/// The `OSC 104 ; ST` shape esccmd's `ResetColor()` emits.
#[test]
fn osc_104_with_no_index_resets_every_slot() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"\x1b]4;0;rgb:1010/2020/3030;1;rgb:4040/5050/6060\x1b\\\
          \x1b]104\x1b\\\
          \x1b]4;0;?;1;?\x1b\\",
    );
    let responses = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&responses[0]).unwrap(),
        "\x1b]4;0;rgb:0000/0000/0000\x1b\\"
    );
    let default_red_top = format_doubled_rgb(default_palette_color(1));
    let expected_1 = format!("\x1b]4;1;rgb:{default_red_top}\x1b\\");
    assert_eq!(std::str::from_utf8(&responses[1]).unwrap(), expected_1);
}

/// The client applies deltas one slot at a time, so a batch that
/// arrived as a single opaque message would be undeliverable.
#[test]
fn multi_pair_osc_4_queues_one_delta_per_index() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"\x1b]4;9;rgb:1010/2020/3030;2;rgb:4040/5050/6060\x1b\\",
    );
    let deltas = g.take_palette_dirty();
    assert!(!deltas.reset_all);
    assert_eq!(
        deltas.entries,
        vec![
            PaletteEntry {
                index: 2,
                rgb: Some((0x40, 0x50, 0x60)),
            },
            PaletteEntry {
                index: 9,
                rgb: Some((0x10, 0x20, 0x30)),
            },
        ],
    );
    // Replace semantics: a drained cycle owes nothing until the next
    // change.
    assert_eq!(g.take_palette_dirty(), PaletteDeltas::default());
}

/// The client has no other way to tell "this one slot" from "all of
/// them".
#[test]
fn osc_104_queues_per_index_and_whole_table_resets_apart() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b]4;5;#aabbcc;6;#112233\x1b\\");
    drop(g.take_palette_dirty());

    drive(&mut p, &mut g, b"\x1b]104;5\x1b\\");
    let per_index = g.take_palette_dirty();
    assert!(!per_index.reset_all);
    assert_eq!(
        per_index.entries,
        vec![PaletteEntry {
            index: 5,
            rgb: None,
        }],
    );

    drive(&mut p, &mut g, b"\x1b]104\x1b\\");
    let whole = g.take_palette_dirty();
    assert!(whole.reset_all, "a bare OSC 104 is the whole-table reset");
    assert!(
        whole.entries.is_empty(),
        "the whole-table reset subsumes the per-index entries it follows",
    );
}

/// `reset_all` is emitted first, so a set in the same cycle must
/// survive it, or a program that recolors by clearing and re-declaring
/// ends up with the cleared half.
#[test]
fn a_set_after_a_whole_table_reset_survives_the_same_cycle() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b]4;1;#ff0000\x1b\\");
    drop(g.take_palette_dirty());

    drive(&mut p, &mut g, b"\x1b]104\x1b\\\x1b]4;2;#00ff00\x1b\\");
    let deltas = g.take_palette_dirty();
    assert!(deltas.reset_all);
    assert_eq!(
        deltas.entries,
        vec![PaletteEntry {
            index: 2,
            rgb: Some((0x00, 0xFF, 0x00)),
        }],
    );
}

/// A reattaching client rebuilds the layer in a deterministic order.
#[test]
fn palette_overrides_replay_ascending_by_index() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"\x1b]4;200;#010203;7;#040506;33;#070809\x1b\\\x1b]104;33\x1b\\",
    );
    assert_eq!(
        g.palette_overrides().collect::<Vec<_>>(),
        vec![(7, (0x04, 0x05, 0x06)), (200, (0x01, 0x02, 0x03))],
    );
}

/// esctest's `test_ChangeDynamicColor_Multiple`.
#[test]
fn osc_10_multi_spec_advances_channel_per_spec() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"\x1b]10;rgb:f0f0/f0f0/f0f0;rgb:f0f0/0000/0000\x1b\\\
          \x1b]10;?;?\x1b\\",
    );
    let r = responses(&mut g);
    assert_eq!(r.len(), 2);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        "\x1b]10;rgb:f0f0/f0f0/f0f0\x1b\\"
    );
    assert_eq!(
        std::str::from_utf8(&r[1]).unwrap(),
        "\x1b]11;rgb:f0f0/0000/0000\x1b\\"
    );
}

/// esctest's `ChangeSpecialColor2` + `ResetSpecialColor_Single2`.
#[test]
fn osc_5_set_query_reset_round_trips_the_special_color_table() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"\x1b]5;0;rgb:8080/8080/8080\x1b\\\x1b]5;0;?\x1b\\\
          \x1b]105;0\x1b\\\x1b]5;0;?\x1b\\",
    );
    let responses = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&responses[0]).unwrap(),
        "\x1b]5;0;rgb:8080/8080/8080\x1b\\"
    );
    // The default is (0,0,0); pinned so a real renderer default would
    // surface here.
    assert_eq!(
        std::str::from_utf8(&responses[1]).unwrap(),
        "\x1b]5;0;rgb:0000/0000/0000\x1b\\"
    );
}

fn format_doubled_rgb((r, g, b): (u8, u8, u8)) -> String {
    format!("{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
}

/// esctest's `reset()` leads with DECSCL and expects each test to start
/// from a clean baseline.
#[test]
fn osc_10_query_after_decscl_returns_default() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
    drive(&mut p, &mut g, b"\x1b]10;?\x07");
    assert_eq!(
        responses(&mut g),
        vec![b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\".to_vec()]
    );
    drive(&mut p, &mut g, b"\x1b[64;1\"p");
    drive(&mut p, &mut g, b"\x1b]10;?\x07");
    assert_eq!(
        responses(&mut g),
        vec![b"\x1b]10;rgb:0000/0000/0000\x1b\\".to_vec()],
        "DECSCL must clear theme_overrides so post-DECSCL query returns the default"
    );
}

/// esctest's `reset()` issues DECSCL → DECSTR → DECRESETs → ED2; none
/// of the later steps may re-introduce an override DECSCL cleared.
#[test]
fn esctest_style_reset_chain_clears_theme_overrides() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
    drive(&mut p, &mut g, b"\x1b[64;1\"p"); // DECSCL
    drive(&mut p, &mut g, b"\x1b[!p"); // DECSTR
    drive(&mut p, &mut g, b"\x1b[?47l\x1b[?1047l\x1b[?1049l\x1b[?69l"); // various DECRESETs
    drive(&mut p, &mut g, b"\x1b[4l"); // RM IRM
    drive(&mut p, &mut g, b"\x1b[20l"); // RM LNM
    drive(&mut p, &mut g, b"\x1b[?7h"); // DECSET DECAWM
    drive(&mut p, &mut g, b"\x1b[?41l"); // DECRESET MoreFix
    drive(&mut p, &mut g, b"\x1b[2J"); // ED(2)
    drive(&mut p, &mut g, b"\x1b]10;?\x07");
    assert_eq!(
        responses(&mut g),
        vec![b"\x1b]10;rgb:0000/0000/0000\x1b\\".to_vec()],
        "After a full esctest-style reset, OSC 10 query must show the default"
    );
}

#[test]
fn osc_10_with_query_payload_is_ignored() {
    use felis_protocol::messages::ThemeChannel;
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]10;#123456\x07");
    assert_eq!(
        g.theme_override(ThemeChannel::Foreground),
        Some((0x12, 0x34, 0x56)),
    );
    // A query must not change the value.
    drive(&mut p, &mut g, b"\x1b]10;?\x07");
    assert_eq!(
        g.theme_override(ThemeChannel::Foreground),
        Some((0x12, 0x34, 0x56)),
    );
}

#[test]
fn osc_10_malformed_payload_keeps_the_prior_value() {
    use felis_protocol::messages::ThemeChannel;
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]10;#ABCDEF\x07");
    let prior = g.theme_override(ThemeChannel::Foreground);
    // Length 5 is not a recognized hex form.
    drive(&mut p, &mut g, b"\x1b]10;#ABCDE\x07");
    assert_eq!(g.theme_override(ThemeChannel::Foreground), prior);
}

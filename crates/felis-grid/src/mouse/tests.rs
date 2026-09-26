use crate::test_support::{drive, responses};
use crate::*;
use felis_vt::Parser;

use felis_protocol::messages::{InputMods, MouseAction, MouseButton, MouseEvent};

use super::*;

fn ev(
    button: Option<MouseButton>,
    action: MouseAction,
    mods: InputMods,
    x: u16,
    y: u16,
) -> MouseEvent {
    // The pixel pair mirrors the cell coords; only the `SgrPixels` test
    // below sets distinct pixels.
    MouseEvent {
        button,
        action,
        mods,
        x,
        y,
        px: x,
        py: y,
    }
}

#[test]
fn sgr_pixels_encoding_reports_the_pixel_pair_not_the_cell() {
    // Distinct values pin that the encoder reads px / py, not the cell.
    let e = MouseEvent {
        button: Some(MouseButton::Left),
        action: MouseAction::Press,
        mods: InputMods::empty(),
        x: 10,
        y: 5,
        px: 137,
        py: 88,
    };
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::SgrPixels).unwrap();
    assert_eq!(bytes, b"\x1b[<0;137;88M");
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<0;10;5M");
}

#[test]
fn sgr_left_release_uses_lowercase_m_with_button_zero() {
    // xterm SGR: press `\e[<0;X;YM`, release `\e[<0;X;Ym`; the same
    // button code, only the trailing letter changes.
    let e = ev(
        Some(MouseButton::Left),
        MouseAction::Release,
        InputMods::empty(),
        10,
        5,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<0;10;5m");
}

#[test]
fn sgr_wheel_up_with_ctrl_carries_wheel_bit_and_ctrl_bit() {
    let e = ev(
        Some(MouseButton::WheelUp),
        MouseAction::Press,
        InputMods::CTRL,
        40,
        12,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<80;40;12M");
}

/// xterm wheel button numbers 64/65/66/67: the contract
/// `felis_protocol::messages::MouseButton`'s docs state but cannot
/// enforce (protocol never sees the encoder).
#[test]
fn wheel_buttons_encode_as_xterm_64_through_67() {
    for (button, code) in [
        (MouseButton::WheelUp, 64),
        (MouseButton::WheelDown, 65),
        (MouseButton::WheelLeft, 66),
        (MouseButton::WheelRight, 67),
    ] {
        let e = ev(Some(button), MouseAction::Press, InputMods::empty(), 1, 1);
        let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).unwrap();
        assert_eq!(bytes, format!("\x1b[<{code};1;1M").into_bytes());
    }
}

#[test]
fn sgr_drag_under_button_only_protocol_drops() {
    let e = ev(
        Some(MouseButton::Left),
        MouseAction::Drag,
        InputMods::empty(),
        10,
        5,
    );
    assert!(
        encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).is_none(),
        "drag must require ?1002 or ?1003"
    );
}

#[test]
fn sgr_drag_under_button_and_drag_protocol_encodes_with_motion_bit() {
    let e = ev(
        Some(MouseButton::Left),
        MouseAction::Drag,
        InputMods::empty(),
        10,
        5,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonAndDrag, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<32;10;5M");
}

#[test]
fn sgr_motion_drops_under_button_only_or_drag_protocol() {
    let e = ev(None, MouseAction::Motion, InputMods::empty(), 1, 1);
    assert!(encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).is_none());
    assert!(encode_mouse(e, MouseProtocol::ButtonAndDrag, MouseEncoding::Sgr).is_none());
}

#[test]
fn sgr_motion_under_any_motion_encodes_with_motion_bit_and_no_button() {
    let e = ev(None, MouseAction::Motion, InputMods::empty(), 7, 3);
    let bytes = encode_mouse(e, MouseProtocol::AnyMotion, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<32;7;3M");
}

#[test]
fn x10_default_press_encodes_classic_three_byte_form() {
    let e = ev(
        Some(MouseButton::Left),
        MouseAction::Press,
        InputMods::empty(),
        1,
        1,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Default).unwrap();
    assert_eq!(bytes, b"\x1b[M \x21\x21");
}

#[test]
fn x10_default_clamps_oversized_coordinates_to_0xff() {
    // Past column 223 the legacy form runs out of byte room; saturate so
    // a stray legacy producer cannot tear down the daemon.
    let e = ev(
        Some(MouseButton::Left),
        MouseAction::Press,
        InputMods::empty(),
        500,
        500,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Default).unwrap();
    assert_eq!(bytes, b"\x1b[M \xff\xff");
}

#[test]
fn x10_left_release_uses_low_bit_overload() {
    // X10 sends button code 3 (low bits 11) for any regular release.
    let e = ev(
        Some(MouseButton::Left),
        MouseAction::Release,
        InputMods::empty(),
        1,
        1,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Default).unwrap();
    assert_eq!(bytes, b"\x1b[M\x23\x21\x21");
}

#[test]
fn shift_alt_ctrl_modifier_bits_compose() {
    let e = ev(
        Some(MouseButton::Left),
        MouseAction::Press,
        InputMods::SHIFT | InputMods::ALT | InputMods::CTRL,
        10,
        5,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<28;10;5M");
}

#[test]
fn mouse_protocol_starts_off_and_button_set_promotes_to_button_events() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    assert_eq!(g.mouse_protocol(), MouseProtocol::Off);
    drive(&mut p, &mut g, b"\x1b[?1000h");
    assert_eq!(g.mouse_protocol(), MouseProtocol::ButtonEvents);
}

#[test]
fn mouse_protocol_higher_levels_override_lower() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b[?1000h\x1b[?1003h");
    assert_eq!(g.mouse_protocol(), MouseProtocol::AnyMotion);
}

#[test]
fn mouse_reset_only_fires_when_the_active_level_matches() {
    // xterm calls this out and helix relies on it.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b[?1000h\x1b[?1003l");
    assert_eq!(g.mouse_protocol(), MouseProtocol::ButtonEvents);
    drive(&mut p, &mut g, b"\x1b[?1000l");
    assert_eq!(g.mouse_protocol(), MouseProtocol::Off);
}

#[test]
fn mouse_encoding_toggles_independently_of_protocol() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b[?1000h\x1b[?1006h");
    assert_eq!(g.mouse_protocol(), MouseProtocol::ButtonEvents);
    assert_eq!(g.mouse_encoding(), MouseEncoding::Sgr);
    drive(&mut p, &mut g, b"\x1b[?1006l");
    assert_eq!(g.mouse_encoding(), MouseEncoding::Default);
    assert_eq!(g.mouse_protocol(), MouseProtocol::ButtonEvents);
}

#[test]
fn mouse_sgr_pixels_encoding_sets_via_1016_and_reset_respects_active_encoding() {
    // Through the parser: the direct `encode_mouse(.., SgrPixels)` tests
    // bypass this dispatch arm.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b[?1000h\x1b[?1016h");
    assert_eq!(g.mouse_encoding(), MouseEncoding::SgrPixels);
    // `?1016l` after `?1006h` must leave Sgr active.
    drive(&mut p, &mut g, b"\x1b[?1006h\x1b[?1016l");
    assert_eq!(g.mouse_encoding(), MouseEncoding::Sgr);
}

/// `?9` (X10 tracking) is tolerated, not implemented: it must not
/// become the active protocol, and it draws no reply
/// (`docs/reference/protocols/vt-compliance.md` "Consumed without
/// effect").
#[test]
fn x10_mouse_mode_is_consumed_without_enabling_tracking() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b[?9h");
    assert_eq!(g.mouse_protocol(), MouseProtocol::Off);
    assert_eq!(g.mouse_encoding(), MouseEncoding::Default);
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    drive(&mut p, &mut g, b"\x1b[?9$p");
    let queried = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&queried[0]).unwrap(),
        "\x1b[?9;1$y",
        "?9 is soft-tracked, so DECRQM reports it set"
    );
    drive(&mut p, &mut g, b"\x1b[?9l");
    assert_eq!(g.mouse_protocol(), MouseProtocol::Off);
}

#[test]
fn mouse_legacy_encodings_1005_1015_are_tolerated() {
    // Modern programs emit them defensively.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b[?1005h\x1b[?1015h\x1b[?1005l\x1b[?1015l",
    );
    assert_eq!(g.mouse_encoding(), MouseEncoding::Default);
    assert_eq!(g.mouse_protocol(), MouseProtocol::Off);
    assert!(responses(&mut g).is_empty(), "neither mode draws a reply");
}

/// Distinguishes the base 128 and the per-button offset from a single
/// shared constant.
#[test]
fn sgr_high_button_press_encodes_128_plus_button_offset() {
    let e = ev(
        Some(MouseButton::Button9),
        MouseAction::Press,
        InputMods::empty(),
        10,
        5,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<129;10;5M");
}

/// The X10 release-overload applies only to regular buttons (`cb <
/// 64`), so wheel-up release stays 64, not 67.
#[test]
fn sgr_wheel_release_keeps_wheel_code_without_overload() {
    let e = ev(
        Some(MouseButton::WheelUp),
        MouseAction::Release,
        InputMods::empty(),
        10,
        5,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<64;10;5m");
}

/// `Button11` (code 131, low bits already 11) must keep all its bits:
/// pins both `&&` operands in the strip guard.
#[test]
fn sgr_high_button_release_keeps_low_bits() {
    let e = ev(
        Some(MouseButton::Button11),
        MouseAction::Release,
        InputMods::empty(),
        10,
        5,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<131;10;5m");
}

/// left+SHIFT release → code 7, stripped to 4: pins `cb & 0b11 == 0b11`
/// (an `|` there would skip the strip).
#[test]
fn sgr_shifted_release_strips_only_the_overload_bits() {
    let e = ev(
        Some(MouseButton::Left),
        MouseAction::Release,
        InputMods::SHIFT,
        10,
        5,
    );
    let bytes = encode_mouse(e, MouseProtocol::ButtonEvents, MouseEncoding::Sgr).unwrap();
    assert_eq!(bytes, b"\x1b[<4;10;5m");
}

/// The daemon reserves `MAX_MOUSE_REPORT_BYTES` per mouse event before
/// it knows which encoding the session will use, so no encoding may
/// exceed that reservation.
#[test]
fn every_encoding_fits_the_reserved_report_size() {
    use felis_protocol::limits::MAX_MOUSE_REPORT_BYTES;

    let widest = MouseEvent {
        button: Some(MouseButton::Button11),
        action: MouseAction::Motion,
        mods: InputMods::SHIFT | InputMods::ALT | InputMods::CTRL,
        x: u16::MAX,
        y: u16::MAX,
        px: u16::MAX,
        py: u16::MAX,
    };
    for encoding in [
        MouseEncoding::Sgr,
        MouseEncoding::SgrPixels,
        MouseEncoding::Default,
    ] {
        let bytes = encode_mouse(widest, MouseProtocol::AnyMotion, encoding).unwrap();
        assert!(
            bytes.len() <= MAX_MOUSE_REPORT_BYTES,
            "{encoding:?} encoded {} bytes, past the reservation",
            bytes.len(),
        );
    }
}

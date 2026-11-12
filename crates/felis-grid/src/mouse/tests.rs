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
fn mouse_events_encode_per_protocol_and_encoding() {
    use MouseAction::{Drag, Motion, Press, Release};
    use MouseButton::{Button9, Button11, Left, WheelUp};
    use MouseEncoding::{Default as X10, Sgr};
    use MouseProtocol::{AnyMotion, ButtonAndDrag, ButtonEvents};
    let none = InputMods::empty();
    for (case, button, action, mods, (x, y), protocol, encoding, expected) in [
        (
            "SGR release keeps the button code and ends in m",
            Some(Left),
            Release,
            none,
            (10, 5),
            ButtonEvents,
            Sgr,
            Some(&b"\x1b[<0;10;5m"[..]),
        ),
        (
            "SGR wheel up with Ctrl carries both bits",
            Some(WheelUp),
            Press,
            InputMods::CTRL,
            (40, 12),
            ButtonEvents,
            Sgr,
            Some(b"\x1b[<80;40;12M"),
        ),
        (
            "Shift, Alt and Ctrl bits compose",
            Some(Left),
            Press,
            InputMods::SHIFT | InputMods::ALT | InputMods::CTRL,
            (10, 5),
            ButtonEvents,
            Sgr,
            Some(b"\x1b[<28;10;5M"),
        ),
        (
            "a drag needs ?1002 or ?1003",
            Some(Left),
            Drag,
            none,
            (10, 5),
            ButtonEvents,
            Sgr,
            None,
        ),
        (
            "a drag under ?1002 sets the motion bit",
            Some(Left),
            Drag,
            none,
            (10, 5),
            ButtonAndDrag,
            Sgr,
            Some(b"\x1b[<32;10;5M"),
        ),
        (
            "motion is dropped under ?1000",
            None,
            Motion,
            none,
            (1, 1),
            ButtonEvents,
            Sgr,
            None,
        ),
        (
            "motion is dropped under ?1002",
            None,
            Motion,
            none,
            (1, 1),
            ButtonAndDrag,
            Sgr,
            None,
        ),
        (
            "motion under ?1003 sets the motion bit and no button",
            None,
            Motion,
            none,
            (7, 3),
            AnyMotion,
            Sgr,
            Some(b"\x1b[<32;7;3M"),
        ),
        (
            "a high button is 128 plus its offset",
            Some(Button9),
            Press,
            none,
            (10, 5),
            ButtonEvents,
            Sgr,
            Some(b"\x1b[<129;10;5M"),
        ),
        // The X10 release overload applies only to regular buttons (cb < 64).
        (
            "a wheel release keeps its code",
            Some(WheelUp),
            Release,
            none,
            (10, 5),
            ButtonEvents,
            Sgr,
            Some(b"\x1b[<64;10;5m"),
        ),
        (
            "Button11 release keeps the low bits it already has",
            Some(Button11),
            Release,
            none,
            (10, 5),
            ButtonEvents,
            Sgr,
            Some(b"\x1b[<131;10;5m"),
        ),
        (
            "Shift+Left release strips only the overload bits",
            Some(Left),
            Release,
            InputMods::SHIFT,
            (10, 5),
            ButtonEvents,
            Sgr,
            Some(b"\x1b[<4;10;5m"),
        ),
        (
            "X10 press is the classic three-byte form",
            Some(Left),
            Press,
            none,
            (1, 1),
            ButtonEvents,
            X10,
            Some(b"\x1b[M \x21\x21"),
        ),
        // Saturating rather than overflowing keeps a stray legacy
        // producer from tearing down the daemon.
        (
            "X10 coordinates past 223 saturate at 0xff",
            Some(Left),
            Press,
            none,
            (500, 500),
            ButtonEvents,
            X10,
            Some(b"\x1b[M \xff\xff"),
        ),
        (
            "X10 release overloads the button code to 3",
            Some(Left),
            Release,
            none,
            (1, 1),
            ButtonEvents,
            X10,
            Some(b"\x1b[M\x23\x21\x21"),
        ),
    ] {
        let e = ev(button, action, mods, x, y);
        assert_eq!(
            encode_mouse(e, protocol, encoding).as_deref(),
            expected,
            "{case}"
        );
    }
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

const ALL_PROTOCOLS: [MouseProtocol; 4] = [
    MouseProtocol::Off,
    MouseProtocol::ButtonEvents,
    MouseProtocol::ButtonAndDrag,
    MouseProtocol::AnyMotion,
];

#[test]
fn mouse_event_reported_is_exactly_the_filter_encode_mouse_applies() {
    let actions = [
        (Some(MouseButton::Left), MouseAction::Press),
        (Some(MouseButton::Left), MouseAction::Release),
        (Some(MouseButton::Left), MouseAction::Drag),
        (None, MouseAction::Motion),
    ];
    for protocol in ALL_PROTOCOLS {
        for (button, action) in actions {
            let e = ev(button, action, InputMods::empty(), 3, 4);
            assert_eq!(
                mouse_event_reported(protocol, action),
                encode_mouse(e, protocol, MouseEncoding::Sgr).is_some(),
                "{protocol:?} / {action:?}",
            );
        }
    }
}

fn unplaced(x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        px: 0,
        py: 0,
        ..ev(
            Some(MouseButton::Left),
            MouseAction::Press,
            InputMods::empty(),
            x,
            y,
        )
    }
}

#[test]
fn a_filled_center_maps_back_to_its_own_cell_at_the_full_ratio() {
    // Extents the cell counts do not divide: a center built from the
    // floored cell (809 / 80 = 10) puts column 80's click on column 79.
    for (extent, cells) in [
        (809_u16, 80_u16),
        (800, 80),
        (1001, 7),
        (17, 16),
        (32_768, 2048),
    ] {
        for cell in 1..=cells {
            let filled =
                fill_cell_center_pixels(unplaced(cell, cell), (cells, cells), (extent, extent));
            let back = |px: u16| u64::from(px - 1) * u64::from(cells) / u64::from(extent);
            assert_eq!(
                back(filled.px),
                u64::from(cell - 1),
                "{extent} px over {cells}, col {cell}"
            );
            assert_eq!(
                back(filled.py),
                u64::from(cell - 1),
                "{extent} px over {cells}, row {cell}"
            );
        }
    }
}

#[test]
fn a_filled_center_stays_within_an_extent_narrower_than_its_cells() {
    let filled = fill_cell_center_pixels(unplaced(80, 24), (80, 24), (1, 1));
    assert_eq!((filled.px, filled.py), (1, 1));
}

#[test]
fn a_supplied_pixel_axis_is_left_as_the_window_placed_it() {
    let placed = MouseEvent {
        px: 137,
        py: 0,
        ..unplaced(10, 5)
    };
    let filled = fill_cell_center_pixels(placed, (80, 24), (800, 384));
    assert_eq!((filled.px, filled.py), (137, 72));
}

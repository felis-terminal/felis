//! Property tests for the mouse encoder.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::{MouseEncoding, MouseProtocol, encode_mouse};
use felis_protocol::messages::{InputMods, MouseAction, MouseButton, MouseEvent};
use proptest::prelude::*;

fn any_button() -> impl Strategy<Value = Option<MouseButton>> {
    prop_oneof![
        Just(None),
        Just(Some(MouseButton::Left)),
        Just(Some(MouseButton::Middle)),
        Just(Some(MouseButton::Right)),
        Just(Some(MouseButton::WheelUp)),
        Just(Some(MouseButton::WheelDown)),
        Just(Some(MouseButton::WheelLeft)),
        Just(Some(MouseButton::WheelRight)),
        Just(Some(MouseButton::Button8)),
        Just(Some(MouseButton::Button9)),
        Just(Some(MouseButton::Button10)),
        Just(Some(MouseButton::Button11)),
    ]
}

fn any_action() -> impl Strategy<Value = MouseAction> {
    prop_oneof![
        Just(MouseAction::Press),
        Just(MouseAction::Release),
        Just(MouseAction::Drag),
        Just(MouseAction::Motion),
    ]
}

fn any_protocol() -> impl Strategy<Value = MouseProtocol> {
    prop_oneof![
        Just(MouseProtocol::Off),
        Just(MouseProtocol::ButtonEvents),
        Just(MouseProtocol::ButtonAndDrag),
        Just(MouseProtocol::AnyMotion),
    ]
}

fn any_encoding() -> impl Strategy<Value = MouseEncoding> {
    prop_oneof![
        Just(MouseEncoding::Default),
        Just(MouseEncoding::Sgr),
        Just(MouseEncoding::SgrPixels),
    ]
}

fn any_event() -> impl Strategy<Value = MouseEvent> {
    (
        any_button(),
        any_action(),
        any::<u8>().prop_map(InputMods::from_bits_truncate),
        any::<u16>(),
        any::<u16>(),
        any::<u16>(),
        any::<u16>(),
    )
        .prop_map(|(button, action, mods, x, y, px, py)| MouseEvent {
            button,
            action,
            mods,
            x,
            y,
            px,
            py,
        })
}

proptest! {
    #[test]
    fn encode_mouse_is_total(
        event in any_event(),
        protocol in any_protocol(),
        encoding in any_encoding(),
    ) {
        drop(encode_mouse(event, protocol, encoding));
    }

    #[test]
    fn off_protocol_always_drops(
        event in any_event(),
        encoding in any_encoding(),
    ) {
        prop_assert_eq!(encode_mouse(event, MouseProtocol::Off, encoding), None);
    }

    #[test]
    fn sgr_envelope_shape(
        event in any_event(),
        protocol in any_protocol().prop_filter(
            "Off drops everything; covered by its own property",
            |p| !matches!(p, MouseProtocol::Off),
        ),
        sgr in prop_oneof![Just(MouseEncoding::Sgr), Just(MouseEncoding::SgrPixels)],
    ) {
        let Some(bytes) = encode_mouse(event, protocol, sgr) else {
            return Ok(());
        };
        prop_assert!(bytes.starts_with(b"\x1b[<"), "SGR must begin \\x1b[<; got {bytes:?}");
        let last = *bytes.last().expect("non-empty SGR output");
        prop_assert!(
            last == b'M' || last == b'm',
            "SGR must end with M or m; got {last:#x}"
        );
    }

    /// xterm `ctlseqs` "Mouse Tracking": the X10 form is fixed-width.
    #[test]
    fn x10_envelope_is_exactly_six_bytes(
        event in any_event(),
        protocol in any_protocol().prop_filter(
            "Off drops everything; covered by its own property",
            |p| !matches!(p, MouseProtocol::Off),
        ),
    ) {
        let Some(bytes) = encode_mouse(event, protocol, MouseEncoding::Default) else {
            return Ok(());
        };
        prop_assert_eq!(bytes.len(), 6, "X10 must be 6 bytes; got {:?}", bytes);
        prop_assert_eq!(&bytes[..3], b"\x1b[M");
    }
}

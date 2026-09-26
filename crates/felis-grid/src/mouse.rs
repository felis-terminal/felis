//! Mouse-event byte encoders: xterm `ctlseqs` "Mouse Tracking" plus
//! the SGR extension (`?1006` / `?1016`). Cell coordinates ride the
//! wire one-based to match xterm; the encoder applies the X10 bias.

use felis_protocol::messages::{InputMods, MouseAction, MouseButton, MouseEvent};

use crate::{MouseEncoding, MouseProtocol};

/// `None` when the active protocol filters the event out.
#[must_use]
pub fn encode_mouse(
    event: MouseEvent,
    protocol: MouseProtocol,
    encoding: MouseEncoding,
) -> Option<Vec<u8>> {
    if protocol == MouseProtocol::Off {
        return None;
    }
    match event.action {
        MouseAction::Press | MouseAction::Release => {}
        MouseAction::Drag => {
            if matches!(protocol, MouseProtocol::ButtonEvents) {
                return None;
            }
        }
        MouseAction::Motion => {
            if !matches!(protocol, MouseProtocol::AnyMotion) {
                return None;
            }
        }
    }
    let cb = button_code(event.button, event.action, event.mods);
    Some(match encoding {
        MouseEncoding::Sgr => encode_sgr(cb, event.action, event.x, event.y),
        MouseEncoding::SgrPixels => encode_sgr(cb, event.action, event.px, event.py),
        MouseEncoding::Default => encode_x10(cb, event),
    })
}

/// xterm `ctlseqs` button-code byte, shared by both encodings before
/// framing.
const fn button_code(button: Option<MouseButton>, action: MouseAction, mods: InputMods) -> u32 {
    let mut cb: u32 = match button {
        // Bare motion: xterm uses button 0 plus the motion bit.
        None | Some(MouseButton::Left) => 0,
        Some(MouseButton::Middle) => 1,
        Some(MouseButton::Right) => 2,
        Some(MouseButton::WheelUp) => 64,
        Some(MouseButton::WheelDown) => 65,
        Some(MouseButton::WheelLeft) => 66,
        Some(MouseButton::WheelRight) => 67,
        Some(MouseButton::Button8) => 128,
        Some(MouseButton::Button9) => 129,
        Some(MouseButton::Button10) => 130,
        Some(MouseButton::Button11) => 131,
    };
    // X10 marks release as low bits 11; the SGR encoder strips them
    // again in favor of its trailing `m`. Wheel and high buttons omit
    // the overload, as xterm does.
    if matches!(action, MouseAction::Release) && cb < 64 {
        cb |= 0b11;
    }
    if matches!(action, MouseAction::Drag | MouseAction::Motion) {
        cb |= 32;
    }
    if mods.contains(InputMods::SHIFT) {
        cb |= 4;
    }
    if mods.contains(InputMods::ALT) {
        cb |= 8;
    }
    if mods.contains(InputMods::CTRL) {
        cb |= 16;
    }
    cb
}

fn encode_sgr(cb: u32, action: MouseAction, x: u16, y: u16) -> Vec<u8> {
    let cb = if matches!(action, MouseAction::Release) && cb & 0b11 == 0b11 && cb < 64 {
        cb & !0b11
    } else {
        cb
    };
    let trailing = if matches!(action, MouseAction::Release) {
        b'm'
    } else {
        b'M'
    };
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(b"\x1b[<");
    out.extend_from_slice(cb.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(x.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(y.to_string().as_bytes());
    out.push(trailing);
    out
}

fn encode_x10(cb: u32, event: MouseEvent) -> Vec<u8> {
    // Coordinates above 223 are unrepresentable in X10; clamped rather
    // than panicking on a stray legacy producer.
    let cb_byte = saturating_byte(cb + 0x20);
    let cx_byte = saturating_byte(u32::from(event.x) + 0x20);
    let cy_byte = saturating_byte(u32::from(event.y) + 0x20);
    vec![0x1b, b'[', b'M', cb_byte, cx_byte, cy_byte]
}

const fn saturating_byte(v: u32) -> u8 {
    if v > 0xff { 0xff } else { v as u8 }
}

#[cfg(test)]
mod tests;

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
    if !mouse_event_reported(protocol, event.action) {
        return None;
    }
    let cb = button_code(event.button, event.action, event.mods);
    Some(match encoding {
        MouseEncoding::Sgr => encode_sgr(cb, event.action, event.x, event.y),
        MouseEncoding::SgrPixels => encode_sgr(cb, event.action, event.px, event.py),
        MouseEncoding::Default => encode_x10(cb, event),
    })
}

/// Whether `protocol` reports an event of `action` at all: the filter
/// [`encode_mouse`] applies, so a sender can refuse an event before
/// writing it.
#[must_use]
pub const fn mouse_event_reported(protocol: MouseProtocol, action: MouseAction) -> bool {
    match action {
        MouseAction::Press | MouseAction::Release => !matches!(protocol, MouseProtocol::Off),
        MouseAction::Drag => matches!(
            protocol,
            MouseProtocol::ButtonAndDrag | MouseProtocol::AnyMotion
        ),
        MouseAction::Motion => matches!(protocol, MouseProtocol::AnyMotion),
    }
}

/// `event` with each pixel axis it left at `0` set to the middle of the
/// pixels a program maps back to its cell at the full ratio
/// `winsize_pixels / cells` (cols, rows); a floored per-cell size drifts
/// onto the neighboring column across a wide grid.
#[must_use]
pub fn fill_cell_center_pixels(
    event: MouseEvent,
    cells: (u16, u16),
    winsize_pixels: (u16, u16),
) -> MouseEvent {
    let center = |pixel: u16, cell: u16, cells: u16, extent: u16| {
        if pixel != 0 || cells == 0 || extent == 0 {
            return pixel;
        }
        let (cells, extent) = (u64::from(cells), u64::from(extent));
        let index = u64::from(cell.max(1) - 1);
        let first = (index * extent).div_ceil(cells);
        let past = ((index + 1) * extent).div_ceil(cells);
        let middle = if past > first {
            u64::midpoint(first, past - 1)
        } else {
            first.min(extent - 1)
        };
        u16::try_from(middle + 1).unwrap_or(u16::MAX)
    };
    MouseEvent {
        px: center(event.px, event.x, cells.0, winsize_pixels.0),
        py: center(event.py, event.y, cells.1, winsize_pixels.1),
        ..event
    }
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

//! X-style color-spec parsing and the OSC 10/11/12 + OSC 4/5 query
//! reply format.

/// xterm replies 16 bits per channel; the grid's 8-bit value is
/// replicated (`0xAB → 0xABAB`), as xterm does for its own palette.
pub(crate) fn format_osc_color_response(code: &[u8], r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(b"\x1b]");
    out.extend_from_slice(code);
    out.extend_from_slice(b";rgb:");
    out.extend_from_slice(format!("{r:02x}{r:02x}").as_bytes());
    out.push(b'/');
    out.extend_from_slice(format!("{g:02x}{g:02x}").as_bytes());
    out.push(b'/');
    out.extend_from_slice(format!("{b:02x}{b:02x}").as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Per X11 `XParseColor(3)`, each `#` channel is left-justified into
/// 16 bits and read through its top byte, not the CSS shorthand:
/// `#abc` is `(0xA0, 0xB0, 0xC0)`, not `(0xAA, 0xBB, 0xCC)`.
pub(crate) fn parse_x_color(text: &str) -> Option<(u8, u8, u8)> {
    parse_x_color_bytes(text.as_bytes())
}

/// The kernel the Kani proof targets. On bytes rather than `str`: the
/// UTF-8 validation and str-search primitives drown the solver.
fn parse_x_color_bytes(b: &[u8]) -> Option<(u8, u8, u8)> {
    if let Some(hex) = b.strip_prefix(b"#") {
        let per_channel = match hex.len() {
            3 | 6 | 9 | 12 => hex.len() / 3,
            _ => return None,
        };
        let r = parse_hex_channel(&hex[..per_channel])?;
        let g = parse_hex_channel(&hex[per_channel..per_channel * 2])?;
        let b = parse_hex_channel(&hex[per_channel * 2..])?;
        return Some((r, g, b));
    }
    if let Some(body) = b.strip_prefix(b"rgb:") {
        let mut parts = body.split(|&c| c == b'/');
        let r = parse_hex_channel(parts.next()?)?;
        let g = parse_hex_channel(parts.next()?)?;
        let b = parse_hex_channel(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        return Some((r, g, b));
    }
    None
}

/// Digits are matched by hand: `from_str_radix` would admit a leading
/// `+`, which `XParseColor` does not.
fn parse_hex_channel(s: &[u8]) -> Option<u8> {
    if s.is_empty() || s.len() > 4 {
        return None;
    }
    let mut raw: u16 = 0;
    for &c in s {
        raw = raw * 16
            + match c {
                b'0'..=b'9' => u16::from(c - b'0'),
                b'a'..=b'f' => u16::from(c - b'a') + 10,
                b'A'..=b'F' => u16::from(c - b'A') + 10,
                _ => return None,
            };
    }
    let shift = (4 - s.len()) * 4;
    let scaled = raw << shift;
    Some((scaled >> 8) as u8)
}

#[cfg(kani)]
mod kani_proofs {
    use super::parse_x_color_bytes;

    /// Independent reference for the X11 color-spec grammar.
    fn reference(b: &[u8]) -> Option<(u8, u8, u8)> {
        let top_byte = |digits: &[u8]| -> Option<u8> {
            if digits.is_empty() || digits.len() > 4 {
                return None;
            }
            let mut value: u32 = 0;
            for &d in digits {
                value = value * 16
                    + match d {
                        b'0'..=b'9' => u32::from(d - b'0'),
                        b'a'..=b'f' => u32::from(d - b'a') + 10,
                        b'A'..=b'F' => u32::from(d - b'A') + 10,
                        _ => return None,
                    };
            }
            Some(((value << ((4 - digits.len()) * 4)) >> 8) as u8)
        };

        if let Some(hex) = b.strip_prefix(b"#") {
            let per = match hex.len() {
                3 | 6 | 9 | 12 => hex.len() / 3,
                _ => return None,
            };
            return Some((
                top_byte(&hex[..per])?,
                top_byte(&hex[per..per * 2])?,
                top_byte(&hex[per * 2..])?,
            ));
        }
        if let Some(body) = b.strip_prefix(b"rgb:") {
            let mut parts = body.split(|&c| c == b'/');
            let r = top_byte(parts.next()?)?;
            let g = top_byte(parts.next()?)?;
            let bl = top_byte(parts.next()?)?;
            if parts.next().is_some() {
                return None;
            }
            return Some((r, g, bl));
        }
        None
    }

    /// 15 bytes covers the longest accepted form, `rgb:abcd/efab/cdef`.
    #[kani::proof]
    #[kani::unwind(20)]
    fn parse_matches_xparsecolor_reference_over_15_byte_window() {
        let bytes: [u8; 15] = kani::any();
        assert_eq!(parse_x_color_bytes(&bytes), reference(&bytes));
    }
}

#[cfg(test)]
mod tests;

//! OSC 52 clipboard codec: selector parsing plus response framing.

use crate::ClipboardSelection;

/// xterm allows `c p q s 0..7` in any combination; felis honors only
/// `c` and `p`, and an empty selector means the clipboard, as in
/// xterm. `None` when no honored selector is present.
pub(crate) fn parse_osc_52_selection(bytes: &[u8]) -> Option<ClipboardSelection> {
    let mut sel = ClipboardSelection::empty();
    for &b in bytes {
        match b {
            b'c' => sel |= ClipboardSelection::CLIPBOARD,
            b'p' => sel |= ClipboardSelection::PRIMARY,
            _ => {}
        }
    }
    if bytes.is_empty() {
        sel |= ClipboardSelection::CLIPBOARD;
    }
    if sel.is_empty() { None } else { Some(sel) }
}

/// Reply to an `OSC 52 ; <sel> ; ?` query. `None` yields an empty body,
/// read as "no clipboard data". BEL-terminated.
pub(crate) fn format_osc_52_response(selector: &[u8], data: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(selector.len() + data.map_or(0, <[u8]>::len) * 2 + 8);
    out.extend_from_slice(b"\x1b]52;");
    out.extend_from_slice(selector);
    out.push(b';');
    if let Some(bytes) = data {
        felis_protocol::base64::encode_into(bytes, &mut out);
    }
    out.push(0x07);
    out
}

#[cfg(test)]
mod tests;

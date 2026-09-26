//! XTGETTCAP (`DCS + q ... ST`) terminfo-cap support: the hex
//! name/value codec and the tiny cap-value lookup table.

/// `None` on odd length or a non-hex byte: xterm reports that slot as
/// unknown rather than poisoning the reply.
pub(crate) fn hex_to_ascii(hex: &[u8]) -> Option<Vec<u8>> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_chunks::<2>().0 {
        let high = decode_hex_nibble(pair[0])?;
        let low = decode_hex_nibble(pair[1])?;
        out.push((high << 4) | low);
    }
    Some(out)
}

const fn decode_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Lowercase, per xterm's convention.
pub(crate) fn ascii_to_hex(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(hex_nibble(b >> 4));
        out.push(hex_nibble(b & 0x0F));
    }
    out
}

const fn hex_nibble(n: u8) -> u8 {
    match n {
        0..=9 => b'0' + n,
        10..=15 => b'a' + n - 10,
        _ => b'?',
    }
}

/// Only the caps esctest and hosted programs actually query; the rest
/// fall through to the `0+r` "unknown" branch. `co` / `li` depend on
/// the live grid and `TN` on the host's `TERM`, so
/// [`crate::Grid::xtgettcap_reply`] answers those.
pub(crate) const fn xtgettcap_value(name: &[u8]) -> Option<&'static str> {
    match name {
        // esctest's `GetIndexedColors()` picks the OSC 4 alias range for
        // special colors from this.
        b"Co" | b"colors" => Some("256"),
        _ => None,
    }
}

#[cfg(test)]
mod tests;

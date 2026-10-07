//! UTF-8 boundary buffering above the byte-level DFA. Invalid sequences
//! emit the Unicode replacement character per WHATWG.

use std::str;

/// Held state is at most three continuation bytes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct Decoder {
    buf: [u8; 4],
    pub(crate) len: usize,
    pub(crate) needed: u8,
}

impl Decoder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; 4],
            len: 0,
            needed: 0,
        }
    }

    /// The bulk-decode print path drains an in-flight sequence byte-by-byte
    /// before it can `str::from_utf8` a run.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.len > 0
    }

    pub const fn reset(&mut self) {
        self.len = 0;
        self.needed = 0;
    }

    /// Emits `'\u{FFFD}'` on a malformed sequence.
    pub fn push<F: FnMut(char)>(&mut self, byte: u8, mut on_char: F) {
        if self.len == 0 && byte < 0x80 {
            on_char(byte as char);
            return;
        }
        if self.len == 0 {
            self.needed = match byte {
                0xC2..=0xDF => 1,
                0xE0..=0xEF => 2,
                0xF0..=0xF4 => 3,
                _ => {
                    on_char('\u{FFFD}');
                    return;
                }
            };
            self.buf[0] = byte;
            self.len = 1;
            return;
        }
        if byte & 0xC0 != 0x80 {
            on_char('\u{FFFD}');
            self.reset();
            self.push(byte, on_char);
            return;
        }
        self.buf[self.len] = byte;
        self.len += 1;
        if self.len as u8 == self.needed + 1 {
            match str::from_utf8(&self.buf[..self.len]) {
                Ok(s) => {
                    if let Some(c) = s.chars().next() {
                        on_char(c);
                    }
                }
                Err(_) => on_char('\u{FFFD}'),
            }
            self.reset();
        }
    }
}

// Kani proofs (`docs/reference/testing.md` "Kani proof inventory") live
// here rather than in the crate-root `kani_proofs` because they read
// `Decoder`'s private fields.
#[cfg(kani)]
mod kani_proofs {
    use super::Decoder;

    /// `push` writes `buf[len]`, so `len` must stay within `0..=4`. Five
    /// bytes cover a full four-byte sequence plus a resync byte, exercising
    /// the invalid-continuation re-feed (one recursion level).
    #[kani::proof]
    #[kani::unwind(6)]
    fn push_never_panics_and_keeps_buffer_bounded() {
        let bytes: [u8; 5] = kani::any();
        let mut dec = Decoder::new();
        for &b in &bytes {
            dec.push(b, |_| {});
            assert!(dec.len <= 4);
            assert!(dec.needed <= 3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(bytes: &[u8]) -> String {
        let mut dec = Decoder::new();
        let mut out = String::new();
        for b in bytes {
            dec.push(*b, |c| out.push(c));
        }
        out
    }

    #[test]
    fn invalid_lead_emits_replacement() {
        assert_eq!(collect(&[0xFF, b'a']), "\u{FFFD}a");
    }

    #[test]
    fn is_pending_tracks_in_flight_sequence() {
        // `is_pending` gates the bulk-decode path.
        let mut dec = Decoder::new();
        assert!(!dec.is_pending(), "fresh decoder is not pending");
        dec.push(0xE3, |_| {});
        assert!(dec.is_pending(), "pending after a lead byte");
        dec.push(0x81, |_| {});
        assert!(dec.is_pending(), "still pending mid-sequence");
        dec.push(0x82, |_| {});
        assert!(!dec.is_pending(), "drained after the final byte");
    }

    #[test]
    fn stray_continuation_byte_is_replacement_not_latin1() {
        // 0x80 is the boundary of the ASCII fast path: a stray continuation
        // byte must decode to U+FFFD, not be waved through as U+0080.
        assert_eq!(collect(&[0x80]), "\u{FFFD}");
    }

    #[test]
    fn invalid_continuation_resyncs() {
        // E3 expects two continuations; an ASCII byte instead yields U+FFFD then A.
        assert_eq!(collect(&[0xE3, 0x41]), "\u{FFFD}A");
    }
}

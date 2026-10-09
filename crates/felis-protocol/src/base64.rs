//! Standard-alphabet base64 codec for Kitty graphics and OSC 52.
//!
//! Accepts optional RFC 4648 §3.2 unpadded tails because kitten emits
//! unpadded payloads. Whitespace is stripped for column-wrapped input.

use std::borrow::Cow;

/// Decodes standard-alphabet base64 bytes, allowing optional padding and whitespace.
///
/// Returns `None` on invalid bytes, misplaced padding, or a 1-char tail.
/// Empty input yields an empty vector for zero-payload keepalives.
#[must_use]
pub fn decode(input: &[u8]) -> Option<Vec<u8>> {
    let bytes: Cow<'_, [u8]> = if input.iter().any(u8::is_ascii_whitespace) {
        Cow::Owned(
            input
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect(),
        )
    } else {
        Cow::Borrowed(input)
    };
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    // Only the last group may carry padding or be short, so every group
    // before it decodes to exactly three bytes.
    let tail_len = match bytes.len() % 4 {
        0 => 4,
        r => r,
    };
    let (body, tail) = bytes.split_at(bytes.len() - tail_len);
    let groups = body.as_chunks::<4>().0;
    let mut out = vec![0u8; groups.len() * 3 + 3];
    let mut invalid = 0u8;
    for (o, g) in out.as_chunks_mut::<3>().0.iter_mut().zip(groups) {
        let s = g.map(|c| SEXTET[c as usize]);
        invalid |= s[0] | s[1] | s[2] | s[3];
        // Every `|` here ORs disjoint bit ranges, so the `| -> ^`
        // mutants cargo-mutants reports on this codec are equivalent.
        let n = (u32::from(s[0]) << 18)
            | (u32::from(s[1]) << 12)
            | (u32::from(s[2]) << 6)
            | u32::from(s[3]);
        *o = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
    }
    if invalid & INVALID != 0 {
        return None;
    }
    let data = match tail {
        [a, b, b'=', b'='] | [a, b] => &[*a, *b][..],
        [a, b, c, b'='] | [a, b, c] => &[*a, *b, *c][..],
        [_] => return None,
        _ => tail,
    };
    let mut n = 0u32;
    for &c in data {
        let s = SEXTET[c as usize];
        if s & INVALID != 0 {
            return None;
        }
        n = (n << 6) | u32::from(s);
    }
    n <<= 6 * (4 - data.len());
    let len = groups.len() * 3 + data.len() - 1;
    out[groups.len() * 3..].copy_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    out.truncate(len);
    Some(out)
}

/// Encode bytes as standard-alphabet base64, appending to `out`.
/// Always padded with `=` to a multiple of four.
pub fn encode_into(input: &[u8], out: &mut Vec<u8>) {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHA[(b0 >> 2) as usize]);
        out.push(ALPHA[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize]);
        if chunk.len() == 1 {
            out.push(b'=');
            out.push(b'=');
        } else {
            out.push(ALPHA[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize]);
            if chunk.len() == 2 {
                out.push(b'=');
            } else {
                out.push(ALPHA[(b2 & 0x3F) as usize]);
            }
        }
    }
}

const INVALID: u8 = 0x80;

/// [`decode_char`] as a table, with [`INVALID`] for every rejected byte.
const SEXTET: [u8; 256] = {
    let mut table = [INVALID; 256];
    let mut c = 0;
    while c < 256 {
        if let Some(v) = decode_char(c as u8) {
            table[c] = v;
        }
        c += 1;
    }
    table
};

const fn decode_char(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{decode, decode_char};

    /// Every accepted byte maps to a sextet that fits six bits, the
    /// invariant the bit-packing in `decode` relies on.
    #[kani::proof]
    fn decode_char_yields_a_six_bit_sextet() {
        let c: u8 = kani::any();
        if let Some(v) = decode_char(c) {
            assert!(v < 64);
        }
    }

    /// `decode` never panics on any input up to eight bytes (two groups:
    /// enough to reach every padding and tail branch).
    #[kani::proof]
    #[kani::unwind(10)]
    fn decode_never_panics() {
        let bytes: [u8; 8] = kani::any();
        let _ = decode(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn empty_input_decodes_to_empty() {
        assert_eq!(decode(b""), Some(Vec::new()));
    }

    #[test]
    fn round_trips_short_payload_with_full_padding() {
        assert_eq!(decode(b"TQ=="), Some(vec![b'M']));
    }

    #[test]
    fn round_trips_two_byte_payload_with_one_pad() {
        assert_eq!(decode(b"TWE="), Some(vec![b'M', b'a']));
    }

    #[test]
    fn round_trips_three_byte_payload_with_no_padding() {
        assert_eq!(decode(b"TWFu"), Some(vec![b'M', b'a', b'n']));
    }

    #[test]
    fn round_trips_long_payload() {
        let input = b"VGhlIHF1aWNrIGJyb3duIGZveCBqdW1wcyBvdmVyIHRoZSBsYXp5IGRvZw==";
        let expected = b"The quick brown fox jumps over the lazy dog";
        assert_eq!(decode(input), Some(expected.to_vec()));
    }

    #[test]
    fn accepts_full_alphabet() {
        // The expected vector is packed by an independent bit loop,
        // not the production table.
        let encoded = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let decoded = decode(encoded).expect("alphabet decodes");
        let mut expected = Vec::with_capacity(48);
        let mut acc: u32 = 0;
        let mut bits = 0;
        for sextet in 0u32..64 {
            acc = (acc << 6) | sextet;
            bits += 6;
            while bits >= 8 {
                bits -= 8;
                expected.push((acc >> bits) as u8);
                acc &= (1 << bits) - 1;
            }
        }
        assert_eq!(decoded, expected);
    }

    #[test]
    fn rejects_url_safe_alphabet() {
        assert_eq!(decode(b"abcd-fgh"), None);
        assert_eq!(decode(b"abcd_fgh"), None);
    }

    #[test]
    fn rejects_pad_in_slot_3_without_pad_in_slot_4() {
        assert_eq!(decode(b"TQ=A"), None);
    }

    #[test]
    fn encode_into_appends_rather_than_replaces() {
        let mut out = b"prefix:".to_vec();
        encode_into(b"Man", &mut out);
        assert_eq!(out, b"prefix:TWFu");
    }

    proptest! {
        /// `decode(encode(x)) == x` for payloads of any length. proptest,
        /// not Kani: the property spans both heap-allocating halves,
        /// which bit-blasts past any solver's reach.
        #[test]
        fn encode_then_decode_round_trips(payload in proptest::collection::vec(any::<u8>(), 0..256)) {
            let mut encoded = Vec::new();
            encode_into(&payload, &mut encoded);
            let decoded = decode(&encoded);
            prop_assert_eq!(decoded.as_deref(), Some(payload.as_slice()));
        }

        /// kitten emits payloads with the `=` tail stripped (RFC 4648
        /// §3.2), so both spellings must decode alike.
        #[test]
        fn an_unpadded_tail_decodes_like_the_padded_form(
            payload in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let mut encoded = Vec::new();
            encode_into(&payload, &mut encoded);
            let unpadded = encoded.iter().copied().filter(|b| *b != b'=').collect::<Vec<_>>();
            let decoded = decode(&unpadded);
            prop_assert_eq!(decoded.as_deref(), Some(payload.as_slice()));
            prop_assert_eq!(decoded, decode(&encoded));
        }

        /// Column-wrapped input: whitespace anywhere is invisible to the
        /// decoder, including between the two bytes of a group.
        #[test]
        fn whitespace_anywhere_leaves_the_decoding_unchanged(
            payload in proptest::collection::vec(any::<u8>(), 0..64),
            gaps in proptest::collection::vec(
                prop::option::of(prop::sample::select(vec![b' ', b'\t', b'\r', b'\n'])),
                0..96,
            ),
        ) {
            let mut encoded = Vec::new();
            encode_into(&payload, &mut encoded);
            let mut spaced = Vec::new();
            for (i, byte) in encoded.iter().enumerate() {
                if let Some(Some(gap)) = gaps.get(i) {
                    spaced.push(*gap);
                }
                spaced.push(*byte);
            }
            if let Some(Some(gap)) = gaps.get(encoded.len()) {
                spaced.push(*gap);
            }
            let decoded = decode(&spaced);
            prop_assert_eq!(decoded.as_deref(), Some(payload.as_slice()));
        }

        /// A byte outside the alphabet is caught wherever it lands, in a
        /// whole group or in the tail.
        #[test]
        fn a_foreign_byte_anywhere_is_rejected(
            payload in proptest::collection::vec(any::<u8>(), 1..64),
            at in any::<prop::sample::Index>(),
            foreign in any::<u8>().prop_filter("outside the alphabet", |b| {
                !b.is_ascii_alphanumeric() && !matches!(b, b'+' | b'/' | b'=') && !b.is_ascii_whitespace()
            }),
        ) {
            let mut encoded = Vec::new();
            encode_into(&payload, &mut encoded);
            let i = at.index(encoded.len());
            encoded[i] = foreign;
            prop_assert_eq!(decode(&encoded), None);
        }

        /// Padding belongs to the last group only.
        #[test]
        fn padding_before_the_last_group_is_rejected(
            payload in proptest::collection::vec(any::<u8>(), 4..64),
            at in any::<prop::sample::Index>(),
            tail in proptest::collection::vec(prop::sample::select(&b"AQw+/9"[..]), 0..4),
        ) {
            let mut encoded = Vec::new();
            encode_into(&payload, &mut encoded);
            let i = at.index(encoded.len() - 4);
            encoded[i] = b'=';
            encoded.extend(tail);
            prop_assert_eq!(decode(&encoded), None);
        }

        /// One character cannot carry a whole byte.
        #[test]
        fn a_one_character_tail_is_rejected(
            payload in proptest::collection::vec(any::<u8>(), 0..64),
            extra in prop::sample::select(&b"AQw+/9"[..]),
        ) {
            let mut encoded = Vec::new();
            encode_into(&payload, &mut encoded);
            let mut unpadded: Vec<u8> = encoded.into_iter().filter(|b| *b != b'=').collect();
            unpadded.truncate(unpadded.len() - unpadded.len() % 4);
            unpadded.push(extra);
            prop_assert_eq!(decode(&unpadded), None);
        }

        /// `decode` is total on malformed input. The Kani proof
        /// (`decode_never_panics`) runs periodically and on x86_64-linux
        /// only; this is the every-platform tripwire.
        #[test]
        fn decode_is_total_on_arbitrary_bytes(input in proptest::collection::vec(any::<u8>(), 0..256)) {
            drop(decode(&input));
        }
    }
}

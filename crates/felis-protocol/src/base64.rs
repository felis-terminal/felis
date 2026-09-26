//! Standard-alphabet base64 codec for Kitty graphics and OSC 52.
//!
//! Accepts optional RFC 4648 §3.2 unpadded tails because kitten emits
//! unpadded payloads. Whitespace is stripped for column-wrapped input.

/// Decodes standard-alphabet base64 bytes, allowing optional padding and whitespace.
///
/// Returns `None` on invalid bytes, misplaced padding, or a 1-char tail.
/// Empty input yields an empty vector for zero-payload keepalives.
#[must_use]
pub fn decode(input: &[u8]) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = input
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let (chunks, rem) = bytes.as_chunks::<4>();
    let mut padded = false;
    for chunk in chunks {
        if padded {
            return None;
        }
        let d0 = decode_char(chunk[0])?;
        let d1 = decode_char(chunk[1])?;
        // Every `|` here ORs disjoint bit ranges, so the `| -> ^`
        // mutants cargo-mutants reports on this codec are equivalent.
        out.push((d0 << 2) | (d1 >> 4));
        if chunk[2] == b'=' {
            if chunk[3] != b'=' {
                return None;
            }
            padded = true;
            continue;
        }
        let d2 = decode_char(chunk[2])?;
        out.push(((d1 & 0x0F) << 4) | (d2 >> 2));
        if chunk[3] == b'=' {
            padded = true;
            continue;
        }
        let d3 = decode_char(chunk[3])?;
        out.push(((d2 & 0x03) << 6) | d3);
    }
    match rem.len() {
        0 => {}
        2 | 3 => {
            if padded {
                return None;
            }
            let d0 = decode_char(rem[0])?;
            let d1 = decode_char(rem[1])?;
            out.push((d0 << 2) | (d1 >> 4));
            if rem.len() == 3 {
                let d2 = decode_char(rem[2])?;
                out.push(((d1 & 0x0F) << 4) | (d2 >> 2));
            }
        }
        _ => return None,
    }
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
    fn rejects_non_alphabet_byte() {
        assert_eq!(decode(b"!@#$"), None);
    }

    #[test]
    fn rejects_one_char_tail() {
        assert_eq!(decode(b"TWFua"), None, "5 chars = 1 char of tail");
    }

    #[test]
    fn rejects_tail_after_padded_group() {
        assert_eq!(decode(b"TQ==TW"), None);
    }

    #[test]
    fn rejects_pad_in_slot_3_without_pad_in_slot_4() {
        assert_eq!(decode(b"TQ=A"), None);
    }

    #[test]
    fn rejects_pad_in_middle_of_input() {
        assert_eq!(decode(b"TQ==TWFu"), None);
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

        /// `decode` is total on malformed input. The Kani proof
        /// (`decode_never_panics`) runs periodically and on x86_64-linux
        /// only; this is the every-platform tripwire.
        #[test]
        fn decode_is_total_on_arbitrary_bytes(input in proptest::collection::vec(any::<u8>(), 0..256)) {
            drop(decode(&input));
        }
    }
}

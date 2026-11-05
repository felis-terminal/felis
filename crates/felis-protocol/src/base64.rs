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
    let mut decoder = Decoder::new();
    decoder.push(input);
    decoder.finish()
}

/// [`decode`] over input that arrives in pieces: pushing the pieces in
/// order and finishing gives what [`decode`] gives for their
/// concatenation, wherever the boundaries fall.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Decoder {
    out: Vec<u8>,
    /// The last group seen, undecoded: only the final group may carry
    /// padding or be short, and which group is final is known only at
    /// [`Decoder::finish`]. Never holds whitespace.
    held: [u8; 4],
    held_len: u8,
    invalid: bool,
    pushed: usize,
}

impl Decoder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            out: Vec::new(),
            held: [0; 4],
            held_len: 0,
            invalid: false,
            pushed: 0,
        }
    }

    pub fn push(&mut self, input: &[u8]) {
        self.pushed += input.len();
        if self.invalid {
            return;
        }
        let (out_len, held, held_len) = (self.out.len(), self.held, self.held_len);
        // Whitespace is outside the alphabet, so whitespace-free input
        // never pays for a scan, and input carrying some fails this pass.
        if self.push_compact(input) {
            return;
        }
        self.out.truncate(out_len);
        (self.held, self.held_len) = (held, held_len);
        if input.iter().any(u8::is_ascii_whitespace) {
            let stripped: Vec<u8> = input
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            if self.push_compact(&stripped) {
                return;
            }
        }
        self.invalid = true;
        self.out = Vec::new();
    }

    /// Bytes pushed so far, whitespace included.
    #[must_use]
    pub const fn pushed(&self) -> usize {
        self.pushed
    }

    /// `None` on invalid bytes, misplaced padding, or a 1-char tail.
    #[must_use]
    pub fn finish(self) -> Option<Vec<u8>> {
        if self.invalid {
            return None;
        }
        let mut out = self.out;
        let tail = &self.held[..usize::from(self.held_len)];
        if tail.is_empty() {
            return Some(out);
        }
        let data = match *tail {
            [a, b, b'=', b'='] | [a, b] => &[a, b][..],
            [a, b, c, b'='] | [a, b, c] => &[a, b, c][..],
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
        out.extend_from_slice(&n.to_be_bytes()[1..data.len()]);
        Some(out)
    }

    /// Input that, pushed into a fresh decoder, rebuilds this one: the
    /// decoded bytes re-encoded, the held group, then spaces up to
    /// [`Decoder::pushed`], since whitespace decodes to nothing.
    #[must_use]
    pub fn replay(&self) -> Vec<u8> {
        let mut text = Vec::with_capacity(self.pushed);
        if self.invalid {
            text.push(b'=');
        } else {
            encode_into(&self.out, &mut text);
            text.extend_from_slice(&self.held[..usize::from(self.held_len)]);
        }
        text.resize(self.pushed, b' ');
        text
    }

    /// Decodes all but the last 1..=4 bytes held so far; `false` when a
    /// decoded group or the held tail has a byte outside the alphabet
    /// other than padding.
    fn push_compact(&mut self, mut data: &[u8]) -> bool {
        let held = usize::from(self.held_len);
        if held + data.len() <= 4 {
            if data.iter().any(u8::is_ascii_whitespace) {
                return false;
            }
            self.held[held..held + data.len()].copy_from_slice(data);
            self.held_len += data.len() as u8;
            return true;
        }
        if held > 0 {
            let (head, rest) = data.split_at(4 - held);
            self.held[held..].copy_from_slice(head);
            data = rest;
            let n = group(self.held);
            if n & INVALID_GROUP != 0 {
                return false;
            }
            self.out.extend_from_slice(&n.to_be_bytes()[1..]);
        }
        let keep = match data.len() % 4 {
            0 => 4,
            r => r,
        };
        let (body, tail) = data.split_at(data.len() - keep);
        if tail.iter().any(u8::is_ascii_whitespace) || !decode_groups(body, &mut self.out) {
            return false;
        }
        self.held[..keep].copy_from_slice(tail);
        self.held_len = keep as u8;
        true
    }
}

/// Appends whole groups (`body.len()` a multiple of four); `false` at
/// a block of groups with a byte outside the alphabet.
fn decode_groups(body: &[u8], out: &mut Vec<u8>) -> bool {
    // The slack spares the final group's bytes a reallocation.
    out.reserve(body.len() / 4 * 3 + 3);
    // Staged through a small block: decoding into the vector itself would
    // first have to zero its spare capacity.
    let mut block = [0u8; 64 * 6];
    let (pairs, odd) = body.as_chunks::<8>();
    for run in pairs.chunks(64) {
        let mut invalid = 0;
        for (o, g) in block.as_chunks_mut::<6>().0.iter_mut().zip(run) {
            let hi = group([g[0], g[1], g[2], g[3]]);
            let lo = group([g[4], g[5], g[6], g[7]]);
            invalid |= hi | lo;
            let n = (u64::from(hi) << 24) | u64::from(lo);
            o.copy_from_slice(&n.to_be_bytes()[2..]);
        }
        if invalid & INVALID_GROUP != 0 {
            return false;
        }
        out.extend_from_slice(&block[..run.len() * 6]);
    }
    if let Ok(odd) = <[u8; 4]>::try_from(odd) {
        let n = group(odd);
        if n & INVALID_GROUP != 0 {
            return false;
        }
        out.extend_from_slice(&n.to_be_bytes()[1..]);
    }
    true
}

/// One whole group's 24 bits, with [`INVALID_GROUP`] set if any byte is
/// outside the alphabet.
fn group(g: [u8; 4]) -> u32 {
    SHIFTED[0][g[0] as usize]
        | SHIFTED[1][g[1] as usize]
        | SHIFTED[2][g[2] as usize]
        | SHIFTED[3][g[3] as usize]
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

/// Above the 24 bits a group decodes to.
const INVALID_GROUP: u32 = 1 << 31;

/// [`SEXTET`] pre-shifted to each slot of a group, so a group decodes with
/// four loads and three ORs.
static SHIFTED: [[u32; 256]; 4] = {
    let mut tables = [[INVALID_GROUP; 256]; 4];
    let mut slot = 0;
    while slot < 4 {
        let mut c = 0;
        while c < 256 {
            if let Some(v) = decode_char(c as u8) {
                tables[slot][c] = (v as u32) << (18 - 6 * slot);
            }
            c += 1;
        }
        slot += 1;
    }
    tables
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
    fn decodes_one_byte_payload_with_full_padding() {
        assert_eq!(decode(b"TQ=="), Some(vec![b'M']));
    }

    #[test]
    fn decodes_two_byte_payload_with_one_pad() {
        assert_eq!(decode(b"TWE="), Some(vec![b'M', b'a']));
    }

    #[test]
    fn decodes_three_byte_payload_with_no_padding() {
        assert_eq!(decode(b"TWFu"), Some(vec![b'M', b'a', b'n']));
    }

    #[test]
    fn decodes_long_payload() {
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

    fn reference_decode(input: &[u8]) -> Option<Vec<u8>> {
        let mut text: Vec<u8> = input
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        if text.len() % 4 == 0 {
            let pad = text
                .iter()
                .rev()
                .take(2)
                .take_while(|&&b| b == b'=')
                .count();
            text.truncate(text.len() - pad);
        }
        if text.len() % 4 == 1 {
            return None;
        }
        let (mut acc, mut bits, mut out) = (0u32, 0, Vec::new());
        for c in text {
            acc = (acc << 6) | u32::from(decode_char(c)?);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
                acc &= (1 << bits) - 1;
            }
        }
        Some(out)
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

        /// `decode` agrees with a strip-then-unpad-then-bit-loop decoder
        /// on input mixing the alphabet, padding, whitespace and foreign
        /// bytes, whatever group the odd byte lands in.
        #[test]
        fn decode_matches_a_bit_loop_reference(
            input in proptest::collection::vec(prop::sample::select(&b"AQw+/9= \n\t-"[..]), 0..48),
        ) {
            prop_assert_eq!(decode(&input), reference_decode(&input));
        }

        /// A payload pushed in pieces decodes as it does whole, wherever
        /// the boundaries split a group, its padding or its whitespace.
        #[test]
        fn pushing_in_pieces_decodes_like_the_whole(
            input in proptest::collection::vec(prop::sample::select(&b"AQw+/9= \n-"[..]), 0..48),
            cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..6),
        ) {
            let mut cuts: Vec<usize> = cuts.iter().map(|c| c.index(input.len() + 1)).collect();
            cuts.sort_unstable();
            let mut decoder = Decoder::new();
            let mut from = 0;
            for cut in cuts.into_iter().chain([input.len()]) {
                decoder.push(&input[from..cut]);
                from = cut;
            }
            prop_assert_eq!(decoder.pushed(), input.len());
            prop_assert_eq!(decoder.finish(), decode(&input));
        }

        /// A decoder rebuilt from its replay finishes like the original
        /// after any further input, and counts the same bytes pushed.
        #[test]
        fn a_replayed_decoder_continues_like_the_original(
            head in proptest::collection::vec(prop::sample::select(&b"AQw+/9= \n-"[..]), 0..24),
            rest in proptest::collection::vec(prop::sample::select(&b"AQw+/9= \n-"[..]), 0..24),
        ) {
            let mut original = Decoder::new();
            original.push(&head);
            let mut replayed = Decoder::new();
            replayed.push(&original.replay());
            prop_assert_eq!(replayed.pushed(), original.pushed());
            original.push(&rest);
            replayed.push(&rest);
            prop_assert_eq!(replayed.finish(), original.finish());
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

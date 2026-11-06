//! base64 decoder differential fuzz target.
//!
//! Input convention: the first byte picks the mode and the split points,
//! the rest is the input. Bit 0 set: the rest is a payload, encoded (bit 1
//! set: padding stripped) and decoded back. Clear: the rest is decoded
//! as is and checked against a strip-then-unpad-then-bit-loop oracle,
//! independent of the table and block kernels under test. Either way the
//! input is also pushed in pieces, at cuts drawn from the first byte, and
//! must finish alike.

#![no_main]

use felis_protocol::base64::{Decoder, decode, encode_into};
use libfuzzer_sys::fuzz_target;

fn sextet(c: u8) -> Option<u32> {
    let v = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(v))
}

fn oracle(input: &[u8]) -> Option<Vec<u8>> {
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
        acc = (acc << 6) | sextet(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let (input, expected) = if mode & 1 == 1 {
        let mut encoded = Vec::new();
        encode_into(rest, &mut encoded);
        if mode & 2 == 2 {
            encoded.retain(|&b| b != b'=');
        }
        (encoded, Some(rest.to_vec()))
    } else {
        (rest.to_vec(), oracle(rest))
    };
    let whole = decode(&input);
    assert_eq!(whole, expected);

    let mut decoder = Decoder::new();
    let step = usize::from(mode >> 2) * 7 + 1;
    for piece in input.chunks(step) {
        decoder.push(piece);
    }
    assert_eq!(decoder.finish(), whole);
});

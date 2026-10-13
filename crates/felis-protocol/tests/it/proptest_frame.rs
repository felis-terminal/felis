//! Property-based frame layer invariants: a corrupt header yields a
//! typed error or a well-formed `Frame` view, never a panic or an
//! over-read.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_protocol::frame::{
    DEFAULT_MAX_BODY, Frame, FrameError, HEADER_LEN, LEN_OVERHEAD, decode, decode_with_ceiling,
};
use proptest::prelude::*;

proptest! {
    /// Any encoded frame decodes back, consuming exactly its length.
    #[test]
    fn encode_then_decode_round_trips(
        kind in any::<u16>(),
        body in proptest::collection::vec(any::<u8>(), 0..4096),
    ) {
        let f = Frame { kind, body: &body };
        let bytes = f.encode().unwrap();
        let (decoded, consumed) = decode(&bytes).unwrap();
        prop_assert_eq!(decoded.kind, kind);
        prop_assert_eq!(decoded.body, body.as_slice());
        prop_assert_eq!(consumed, bytes.len());
    }

    /// Any short prefix returns `Incomplete` naming exactly the bytes
    /// still missing, so a reader can size its next read instead of
    /// polling.
    #[test]
    fn any_truncated_prefix_yields_incomplete(
        kind in any::<u16>(),
        body in proptest::collection::vec(any::<u8>(), 0..2048),
        chop in 1usize..16,
    ) {
        let f = Frame { kind, body: &body };
        let bytes = f.encode().unwrap();
        let cut = chop.min(bytes.len());
        if cut == 0 {
            return Ok(());
        }
        let truncated = &bytes[..bytes.len() - cut];
        // Below the length prefix the decoder cannot know the body
        // size yet, so it asks for the rest of that field.
        let missing = if truncated.len() < 4 {
            4 - truncated.len()
        } else {
            bytes.len() - truncated.len()
        };
        prop_assert_eq!(decode(truncated), Err(FrameError::Incomplete { need: missing }));
    }

    /// A body above the ceiling is rejected from the 4-byte length
    /// prefix alone.
    #[test]
    fn body_above_ceiling_is_rejected_from_just_the_length_prefix(
        body_len in 1u32..=u32::MAX / 2,
        ceiling in 0u32..=u32::MAX / 2,
    ) {
        prop_assume!(body_len > ceiling);
        let len = LEN_OVERHEAD.saturating_add(body_len);
        let bytes = len.to_le_bytes();
        let outcome = decode_with_ceiling(&bytes, ceiling);
        prop_assert_eq!(
            outcome,
            Err(FrameError::BodyTooLarge { body_len: u64::from(body_len), ceiling })
        );
    }

    #[test]
    fn random_input_never_panics(input in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode(&input);
    }
}

#[test]
fn default_ceiling_admits_a_50_mib_body() {
    // `docs/explanation/architecture/ipc.md` "Goals": 50 MiB rehydration.
    const _: () = assert!(DEFAULT_MAX_BODY >= 50 * 1024 * 1024);
}

#[test]
fn header_len_is_a_compile_time_invariant() {
    const _: () = assert!(HEADER_LEN == 6);
    const _: () = assert!(LEN_OVERHEAD == 2);
}

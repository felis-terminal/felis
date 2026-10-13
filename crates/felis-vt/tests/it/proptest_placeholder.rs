//! Property tests for the Kitty graphics Unicode-placeholder decoder:
//! totality on arbitrary UTF-8 (the dispatcher runs it on raw payloads),
//! and a build / decode round-trip over valid ranks in `0..=296` pinning
//! `rank_of` ↔ `diacritic_for` through the placeholder envelope.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_protocol::kitty_graphics::placeholder::{
    DIACRITIC_COUNT, PLACEHOLDER, Rank, decode_placement, diacritic_for,
};
use proptest::prelude::*;

const MAX_RANK: u32 = (DIACRITIC_COUNT - 1) as u32;

fn rank(n: u32) -> Rank {
    Rank::new(n).expect("strategy keeps ranks in the alphabet")
}

proptest! {
    /// Any `&str` decodes to `Some` or `None`, no panic.
    #[test]
    fn decode_is_total_on_arbitrary_strings(text in "\\PC{0,16}") {
        // 16 chars still covers the 0..=4-diacritic regions.
        let _ = decode_placement(&text);
    }

    /// Placeholder + 0..=3 valid diacritics round-trips for each length.
    #[test]
    fn build_decode_round_trips_for_zero_to_three_diacritics(
        row in 0..=MAX_RANK,
        col in 0..=MAX_RANK,
        msb in 0..=MAX_RANK,
        // The spec rejects a fourth diacritic.
        len in 0u32..=3,
    ) {
        let mut text = String::from(PLACEHOLDER);
        let ranks = [row, col, msb];
        for &r in &ranks[..len as usize] {
            text.push(diacritic_for(rank(r)));
        }

        let placement = decode_placement(&text).expect("valid placeholder cell decodes");

        let expected_row = (len >= 1).then_some(rank(row));
        let expected_col = (len >= 2).then_some(rank(col));
        let expected_msb = (len >= 3).then_some(rank(msb));

        prop_assert_eq!(placement.row, expected_row);
        prop_assert_eq!(placement.col, expected_col);
        prop_assert_eq!(placement.image_id_msb, expected_msb);
    }

    /// A fourth diacritic is always rejected, whatever ranks fill the slots.
    #[test]
    fn fourth_diacritic_is_always_rejected(
        a in 0..=MAX_RANK,
        b in 0..=MAX_RANK,
        c in 0..=MAX_RANK,
        d in 0..=MAX_RANK,
    ) {
        let mut text = String::from(PLACEHOLDER);
        text.push(diacritic_for(rank(a)));
        text.push(diacritic_for(rank(b)));
        text.push(diacritic_for(rank(c)));
        text.push(diacritic_for(rank(d)));
        prop_assert!(decode_placement(&text).is_none());
    }

    /// A non-alphabet codepoint after the placeholder rejects the cell: the
    /// dispatcher relies on `None` to fall back to a default placement.
    #[test]
    fn non_alphabet_codepoint_after_placeholder_is_rejected(
        // The strategy can't exclude the alphabet, so skip its members at
        // runtime.
        ch in any::<char>(),
    ) {
        if felis_protocol::kitty_graphics::placeholder::rank_of(ch).is_some() || ch == PLACEHOLDER {
            return Ok(());
        }
        let mut text = String::from(PLACEHOLDER);
        text.push(ch);
        prop_assert!(decode_placement(&text).is_none());
    }
}

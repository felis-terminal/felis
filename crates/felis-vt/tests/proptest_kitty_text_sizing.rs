//! Property tests for the OSC 66 (Kitty text-sizing) parser: arbitrary
//! bytes must never reach the dispatcher as a malformed `Sizing`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_vt::kitty_text_sizing::{HAlign, Sizing, VAlign, parse, parse_metadata};
use proptest::prelude::*;

proptest! {
    /// Totality of `parse` over arbitrary field counts and byte content.
    #[test]
    fn parse_payload_is_total_on_arbitrary_bodies(
        parts in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..32),
            0..6,
        ),
    ) {
        let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        drop(parse(&refs.join(&b';')));
    }

    /// Any `Sizing` `parse_metadata` accepts respects the spec ranges and
    /// the `d > n` invariant (the tripwire for a relaxation of
    /// `Sizing::new`, the type's only constructor); `parse_metadata` never
    /// panics.
    #[test]
    fn accepted_sizing_respects_spec_ranges(
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let Some(s) = parse_metadata(&bytes) else { return Ok(()) };
        prop_assert!((1..=7).contains(&s.scale()), "scale={} out of [1,7]", s.scale());
        prop_assert!(s.cell_width() <= 7, "cell_width={} > 7", s.cell_width());
        prop_assert!(s.frac_num() <= 15, "frac_num={} > 15", s.frac_num());
        prop_assert!(s.frac_den() <= 15, "frac_den={} > 15", s.frac_den());
        prop_assert!(matches!(s.valign(), VAlign::Top | VAlign::Bottom | VAlign::Center));
        prop_assert!(matches!(s.halign(), HAlign::Left | HAlign::Right | HAlign::Center));
        if s.frac_den() != 0 {
            prop_assert!(
                s.frac_num() < s.frac_den(),
                "fractional invariant: n={} d={}",
                s.frac_num(),
                s.frac_den(),
            );
        }
    }

    /// Spelled-out metadata parses to exactly what the constructor
    /// admits: the text front door adds no policy of its own beyond the
    /// alignment enums, so a range check may live in one place only.
    #[test]
    fn spelled_out_metadata_agrees_with_the_constructor(
        scale in 0u8..=9,
        cell_width in 0u8..=9,
        n in 0u8..=17,
        d in 0u8..=17,
        v in 0u8..=3,
        h in 0u8..=3,
    ) {
        let bytes = format!("s={scale}:w={cell_width}:n={n}:d={d}:v={v}:h={h}");
        let valign = match v {
            0 => Some(VAlign::Top),
            1 => Some(VAlign::Bottom),
            2 => Some(VAlign::Center),
            _ => None,
        };
        let halign = match h {
            0 => Some(HAlign::Left),
            1 => Some(HAlign::Right),
            2 => Some(HAlign::Center),
            _ => None,
        };
        let expected = match (valign, halign) {
            (Some(valign), Some(halign)) => {
                Sizing::new(scale, cell_width, n, d, valign, halign)
            }
            _ => None,
        };
        prop_assert_eq!(parse_metadata(bytes.as_bytes()), expected);
    }

    /// Bytes outside the permitted key set never produce a `Some(Sizing)`.
    #[test]
    fn unknown_keys_always_reject(
        key in "[a-zA-Z]",
        value in 0u8..=255,
    ) {
        let key_byte = key.as_bytes()[0];
        prop_assume!(!matches!(key_byte, b's' | b'w' | b'n' | b'd' | b'v' | b'h'));
        let bytes = format!("{key}={value}");
        prop_assert!(
            parse_metadata(bytes.as_bytes()).is_none(),
            "unknown key {key} should not parse",
        );
    }

    /// The empty-metadata path yields exactly `Sizing::default()`.
    #[test]
    fn empty_metadata_always_yields_default(
        // The unrelated noise byte makes proptest sample the empty-input
        // case repeatedly.
        _stir in any::<u8>(),
    ) {
        prop_assert_eq!(parse_metadata(b""), Some(Sizing::default()));
    }
}

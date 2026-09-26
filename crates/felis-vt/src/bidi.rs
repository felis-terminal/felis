//! Bidi-override codepoint detection for the Trojan-Source attack
//! (CVE-2021-42574): the UAX #9 explicit-formatting characters
//! (`Bidi_Class` `LRE`, `RLE`, `LRO`, `RLO`, `PDF`, `LRI`, `RLI`, `FSI`,
//! `PDI`). The mitigation per `docs/explanation/security-model.md` "Text
//! rendering" is a visible marker on every cell whose grapheme contains one.

/// True for the nine UAX #9 explicit-formatting characters: `U+202A..=U+202E`
/// (LRE, RLE, PDF, LRO, RLO) and `U+2066..=U+2069` (LRI, RLI, FSI, PDI).
///
/// RTL script characters (Arabic, Hebrew, …) are not flagged: they are
/// content, not formatting.
#[must_use]
pub const fn is_override(ch: char) -> bool {
    matches!(
        ch as u32,
        0x202A | 0x202B | 0x202C | 0x202D | 0x202E | 0x2066 | 0x2067 | 0x2068 | 0x2069
    )
}

pub fn iter_contains_override<I: IntoIterator<Item = char>>(chars: I) -> bool {
    chars.into_iter().any(is_override)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nine_explicit_formatting_characters_are_flagged() {
        for cp in [
            0x202Au32, 0x202B, 0x202C, 0x202D, 0x202E, 0x2066, 0x2067, 0x2068, 0x2069,
        ] {
            let ch = char::from_u32(cp).unwrap();
            assert!(is_override(ch), "U+{cp:04X} should be flagged");
        }
    }

    #[test]
    fn iter_finds_an_override_anywhere_in_the_cluster() {
        // A printable base with an RLO hidden inside the cluster.
        let attack = "a\u{202E}b";
        assert!(iter_contains_override(attack.chars()));

        assert!(!iter_contains_override("hello".chars()));
        assert!(!iter_contains_override("\u{0305}".chars()));

        assert!(!iter_contains_override(std::iter::empty()));
    }

    /// With `the_nine_explicit_formatting_characters_are_flagged` this fixes
    /// the accept-set to exactly those nine codepoints.
    #[test]
    fn exactly_nine_codepoints_are_overrides() {
        let count = (0..=char::MAX as u32)
            .filter_map(char::from_u32)
            .filter(|c| is_override(*c))
            .count();
        assert_eq!(count, 9);
    }
}

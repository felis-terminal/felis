//! The default presentation of a scalar, which picks a color or a mono
//! face before any shaping.

use std::cmp::Ordering;

mod tables;

use tables::EMOJI_PRESENTATION;

/// True for a scalar that renders as an emoji without a VS16.
pub(crate) fn is_emoji_presentation(c: char) -> bool {
    EMOJI_PRESENTATION
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                Ordering::Less
            } else if lo > c {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthChar;

    use super::*;

    #[test]
    fn table_shares_the_unicode_version_of_unicode_width() {
        assert_eq!(tables::UNICODE_VERSION, unicode_width::UNICODE_VERSION);
    }

    #[test]
    fn ranges_are_sorted_merged_and_disjoint() {
        for pair in EMOJI_PRESENTATION.windows(2) {
            let [(lo, hi), (next, _)] = [pair[0], pair[1]];
            assert!(lo <= hi, "{lo:?}..={hi:?}");
            assert!(u32::from(hi) + 1 < u32::from(next), "{hi:?} then {next:?}");
        }
    }

    /// Every scalar defaults to emoji presentation only at width 2, so a
    /// table from another Unicode version than unicode-width shows here.
    /// A lone regional indicator is the exception: only a pair is wide.
    #[test]
    fn every_emoji_presentation_scalar_is_wide() {
        let regional_indicator = '\u{1F1E6}'..='\u{1F1FF}';
        for &(lo, hi) in EMOJI_PRESENTATION {
            for c in (lo..=hi).filter(|c| !regional_indicator.contains(c)) {
                assert_eq!(c.width(), Some(2), "{c:?} U+{:04X}", u32::from(c));
            }
        }
    }

    #[test]
    fn the_property_holds_for_emoji_and_not_for_text_default_symbols() {
        for c in ['⌚', '⏩', '⏰', '⭐', '⚡', '☕', '🀄', '🏻', '🫸'] {
            assert!(is_emoji_presentation(c), "{c:?}");
        }
        // Text-default symbols, including the gaps between ranges
        // (U+23ED..=U+23EF, U+23F1..=U+23F2) and wide CJK.
        for c in ['⏭', '⏱', '⏲', '⏸', '✳', '❤', '☺', '♥', '©', '字', 'A'] {
            assert!(!is_emoji_presentation(c), "{c:?}");
        }
    }
}

//! The default presentation of a scalar, which picks a color or a mono
//! face before any shaping.

use std::cmp::Ordering;

/// `Emoji_Presentation` from Unicode 17.0.0 `emoji-data.txt`, the
/// version unicode-width encodes, with adjacent ranges merged.
#[rustfmt::skip]
const EMOJI_PRESENTATION: &[(char, char)] = &[
    ('\u{231A}', '\u{231B}'),
    ('\u{23E9}', '\u{23EC}'),
    ('\u{23F0}', '\u{23F0}'),
    ('\u{23F3}', '\u{23F3}'),
    ('\u{25FD}', '\u{25FE}'),
    ('\u{2614}', '\u{2615}'),
    ('\u{2648}', '\u{2653}'),
    ('\u{267F}', '\u{267F}'),
    ('\u{2693}', '\u{2693}'),
    ('\u{26A1}', '\u{26A1}'),
    ('\u{26AA}', '\u{26AB}'),
    ('\u{26BD}', '\u{26BE}'),
    ('\u{26C4}', '\u{26C5}'),
    ('\u{26CE}', '\u{26CE}'),
    ('\u{26D4}', '\u{26D4}'),
    ('\u{26EA}', '\u{26EA}'),
    ('\u{26F2}', '\u{26F3}'),
    ('\u{26F5}', '\u{26F5}'),
    ('\u{26FA}', '\u{26FA}'),
    ('\u{26FD}', '\u{26FD}'),
    ('\u{2705}', '\u{2705}'),
    ('\u{270A}', '\u{270B}'),
    ('\u{2728}', '\u{2728}'),
    ('\u{274C}', '\u{274C}'),
    ('\u{274E}', '\u{274E}'),
    ('\u{2753}', '\u{2755}'),
    ('\u{2757}', '\u{2757}'),
    ('\u{2795}', '\u{2797}'),
    ('\u{27B0}', '\u{27B0}'),
    ('\u{27BF}', '\u{27BF}'),
    ('\u{2B1B}', '\u{2B1C}'),
    ('\u{2B50}', '\u{2B50}'),
    ('\u{2B55}', '\u{2B55}'),
    ('\u{1F004}', '\u{1F004}'),
    ('\u{1F0CF}', '\u{1F0CF}'),
    ('\u{1F18E}', '\u{1F18E}'),
    ('\u{1F191}', '\u{1F19A}'),
    ('\u{1F1E6}', '\u{1F1FF}'),
    ('\u{1F201}', '\u{1F201}'),
    ('\u{1F21A}', '\u{1F21A}'),
    ('\u{1F22F}', '\u{1F22F}'),
    ('\u{1F232}', '\u{1F236}'),
    ('\u{1F238}', '\u{1F23A}'),
    ('\u{1F250}', '\u{1F251}'),
    ('\u{1F300}', '\u{1F320}'),
    ('\u{1F32D}', '\u{1F335}'),
    ('\u{1F337}', '\u{1F37C}'),
    ('\u{1F37E}', '\u{1F393}'),
    ('\u{1F3A0}', '\u{1F3CA}'),
    ('\u{1F3CF}', '\u{1F3D3}'),
    ('\u{1F3E0}', '\u{1F3F0}'),
    ('\u{1F3F4}', '\u{1F3F4}'),
    ('\u{1F3F8}', '\u{1F43E}'),
    ('\u{1F440}', '\u{1F440}'),
    ('\u{1F442}', '\u{1F4FC}'),
    ('\u{1F4FF}', '\u{1F53D}'),
    ('\u{1F54B}', '\u{1F54E}'),
    ('\u{1F550}', '\u{1F567}'),
    ('\u{1F57A}', '\u{1F57A}'),
    ('\u{1F595}', '\u{1F596}'),
    ('\u{1F5A4}', '\u{1F5A4}'),
    ('\u{1F5FB}', '\u{1F64F}'),
    ('\u{1F680}', '\u{1F6C5}'),
    ('\u{1F6CC}', '\u{1F6CC}'),
    ('\u{1F6D0}', '\u{1F6D2}'),
    ('\u{1F6D5}', '\u{1F6D8}'),
    ('\u{1F6DC}', '\u{1F6DF}'),
    ('\u{1F6EB}', '\u{1F6EC}'),
    ('\u{1F6F4}', '\u{1F6FC}'),
    ('\u{1F7E0}', '\u{1F7EB}'),
    ('\u{1F7F0}', '\u{1F7F0}'),
    ('\u{1F90C}', '\u{1F93A}'),
    ('\u{1F93C}', '\u{1F945}'),
    ('\u{1F947}', '\u{1F9FF}'),
    ('\u{1FA70}', '\u{1FA7C}'),
    ('\u{1FA80}', '\u{1FA8A}'),
    ('\u{1FA8E}', '\u{1FAC6}'),
    ('\u{1FAC8}', '\u{1FAC8}'),
    ('\u{1FACD}', '\u{1FADC}'),
    ('\u{1FADF}', '\u{1FAEA}'),
    ('\u{1FAEF}', '\u{1FAF8}'),
];

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

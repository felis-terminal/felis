use super::{is_extended_pictographic, is_grapheme_extend};
use unicode_segmentation::UnicodeSegmentation;

fn one_grapheme(s: &str) -> bool {
    s.graphemes(true).count() == 1
}

fn scalars() -> impl Iterator<Item = char> {
    (0..=0x10_FFFFu32).filter_map(char::from_u32)
}

#[test]
fn extended_pictographic_spot_checks() {
    for c in ['©', '❤', '☝', '👍', '🔥', '👩', '\u{1FFFD}'] {
        assert!(is_extended_pictographic(c), "{c:?}");
    }
    for c in ['a', 'Ā', '字', 'क', '🏻', '🇯', '\u{200D}', '\u{FE0F}', '1'] {
        assert!(!is_extended_pictographic(c), "{c:?}");
    }
}

#[test]
fn extend_spot_checks() {
    for c in [
        '\u{0301}',
        '\u{FE0F}',
        '\u{094D}',
        '🏻',
        '\u{20E3}',
        '\u{E0020}',
    ] {
        assert!(is_grapheme_extend(c), "{c:?}");
    }
    for c in [
        'a', '\u{200D}', '\u{200B}', '\u{202E}', '\u{00AD}', '\u{0903}',
    ] {
        assert!(!is_grapheme_extend(c), "{c:?}");
    }
}

/// GB11 is the only rule that joins a scalar after `👍 ZWJ` but not
/// after `a ZWJ`, so the pair of segmentations isolates the property
/// from Extend, `SpacingMark` and ZWJ followers.
#[test]
fn extended_pictographic_matches_unicode_segmentation() {
    let mismatches: Vec<char> = scalars()
        .filter(|&c| {
            let expected =
                one_grapheme(&format!("👍\u{200D}{c}")) && !one_grapheme(&format!("a\u{200D}{c}"));
            is_extended_pictographic(c) != expected
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:?}");
}

/// Only an Extend scalar between a pictographic and a ZWJ keeps GB11
/// open: a `SpacingMark` or a second ZWJ joins the cluster yet closes it.
#[test]
fn extend_matches_unicode_segmentation() {
    let mismatches: Vec<char> = scalars()
        .filter(|&c| is_grapheme_extend(c) != one_grapheme(&format!("👍{c}\u{200D}🔥")))
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:?}");
}

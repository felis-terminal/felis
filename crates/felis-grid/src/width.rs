//! Cell widths: `felis-grid` is the single source of truth, and the
//! renderer's overlays reuse these rules rather than restating them.

use std::sync::LazyLock;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::editing::{is_emoji_modifier, is_regional_indicator};
use crate::uax29::is_emoji_presentation;

/// A regional indicator is two cells even alone, as the text sizing
/// spec's width classes put it first; unicode-width gives it one.
#[must_use]
pub fn char_cell_width(c: char) -> u8 {
    if is_regional_indicator(c) {
        2
    } else {
        c.width().unwrap_or(0) as u8
    }
}

/// `0` for a cluster of zero-width scalars; callers that lay the cluster
/// out in a cell floor it to 1.
pub(crate) fn cluster_cell_width(cluster: &str) -> u8 {
    if cluster.chars().next().is_some_and(is_regional_indicator) {
        return 2;
    }
    if narrowed_by_vs15(cluster) {
        return 1;
    }
    cluster.width().min(2) as u8
}

/// The text sizing spec's VS15 rule: a U+FE0E right after a
/// `Basic_Emoji` listed without U+FE0F narrows its cell to one. Those
/// are the `Emoji_Presentation` scalars less the regional indicators.
/// unicode-width narrows only the pairs `emoji-variation-sequences.txt`
/// lists.
fn narrowed_by_vs15(cluster: &str) -> bool {
    let mut prev = None;
    cluster.chars().any(|c| {
        let hit = c == '\u{FE0E}'
            && prev.is_some_and(|p| is_emoji_presentation(p) && !is_regional_indicator(p));
        prev = Some(c);
        hit
    })
}

/// The text sizing spec's invalid code points, discarded before they
/// reach a cell: controls (C1 among them, as UTF-8 decodes them),
/// U+FDD0..U+FDEF, and the two noncharacters ending every plane.
pub(crate) const fn is_discarded_scalar(c: char) -> bool {
    matches!(c, '\0'..='\u{1F}' | '\u{7F}'..='\u{9F}' | '\u{FDD0}'..='\u{FDEF}')
        || c as u32 & 0xFFFE == 0xFFFE
}

/// `0` defers char to `put_grapheme`: width 0, a char that can fold
/// into the previous grapheme, or one it discards. Nonzero answers must match
/// `Grid::grapheme_width`.
pub(crate) fn bulk_width_or_defer(c: char) -> usize {
    if is_discarded_scalar(c) {
        0
    } else {
        kept_width_or_defer(c)
    }
}

fn kept_width_or_defer(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    if is_emoji_modifier(c) || is_regional_indicator(c) {
        0
    } else {
        c.width().unwrap_or(0)
    }
}

/// `bulk_width_or_defer` for every BMP scalar, two bits each. The batch
/// loop needs its width from a load, not a call: an opaque call between
/// cell stores makes the compiler reload `cells` on every store.
static BMP_WIDTHS: LazyLock<Box<[u8; 0x4000]>> = LazyLock::new(|| {
    let mut table = Box::new([0u8; 0x4000]);
    for cp in 0..0x1_0000u32 {
        let w = char::from_u32(cp).map_or(0, kept_width_or_defer);
        table[(cp >> 2) as usize] |= (w as u8) << ((cp & 3) * 2);
    }
    // Cleared afterwards, not tested per scalar: that test costs every
    // process printing non-ASCII ~1M instructions.
    for cp in (0..0x20)
        .chain(0x7F..=0x9F)
        .chain(0xFDD0..=0xFDEF)
        .chain(0xFFFE..=0xFFFF)
    {
        table[(cp >> 2) as usize] &= !(3 << ((cp & 3) * 2));
    }
    table
});

pub(crate) fn bmp_widths() -> &'static [u8; 0x4000] {
    &BMP_WIDTHS
}

/// Same answer as `bulk_width_or_defer`.
#[inline]
pub(crate) fn table_width(table: &[u8; 0x4000], c: char) -> usize {
    let cp = c as u32;
    if cp < 0x1_0000 {
        usize::from((table[(cp >> 2) as usize] >> ((cp & 3) * 2)) & 3)
    } else {
        bulk_width_or_defer(c)
    }
}

#[cfg(test)]
mod tests;

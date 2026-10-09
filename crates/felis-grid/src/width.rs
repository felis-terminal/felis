//! Cell widths: `felis-grid` is the single source of truth, and the
//! renderer's overlays reuse these rules rather than restating them.

use std::sync::LazyLock;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::editing::{is_emoji_modifier, is_regional_indicator};

#[must_use]
pub fn char_cell_width(c: char) -> u8 {
    c.width().unwrap_or(0) as u8
}

/// `0` for a cluster of zero-width scalars; callers that lay the cluster
/// out in a cell floor it to 1.
pub(crate) fn cluster_cell_width(cluster: &str) -> u8 {
    cluster.width().min(2) as u8
}

/// `0` defers char to `put_grapheme`: width 0, or a char that can fold
/// into the previous grapheme. Nonzero answers must match
/// `Grid::grapheme_width`.
pub(crate) fn bulk_width_or_defer(c: char) -> usize {
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
        let w = char::from_u32(cp).map_or(0, bulk_width_or_defer);
        table[(cp >> 2) as usize] |= (w as u8) << ((cp & 3) * 2);
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

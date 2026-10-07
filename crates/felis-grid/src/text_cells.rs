//! The cell layout of text drawn outside the grid (the IME pre-edit,
//! the bottom bars), so it takes the cells printing it would.

use std::ops::Range;

use unicode_width::UnicodeWidthStr;

use crate::char_cell_width;
use crate::editing::{continues_cluster, ends_in_pictographic_joiner, is_lone_regional_indicator};

/// Splits `text` into the cells the grid prints it as: one cell's byte
/// range and width (1 or 2) per item. Unlike printing, a leading
/// extender keeps a cell of its own and a cluster grows past
/// [`crate::ClusterText::CAP`], so everything typed stays visible.
#[must_use]
pub const fn text_cells(text: &str) -> TextCells<'_> {
    TextCells { text, pos: 0 }
}

#[derive(Debug, Clone)]
pub struct TextCells<'a> {
    text: &'a str,
    pos: usize,
}

impl Iterator for TextCells<'_> {
    type Item = (Range<usize>, u8);

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.pos;
        let mut chars = self.text[start..].chars();
        let base = chars.next()?;
        let mut end = start + base.len_utf8();
        let mut after_base = false;
        for c in chars {
            let cluster = &self.text[start..end];
            let folds = continues_cluster(
                c,
                char_cell_width(c),
                cluster.ends_with('\u{200D}'),
                after_base,
                || ends_in_pictographic_joiner(cluster),
                || is_lone_regional_indicator(cluster),
            );
            if !folds {
                break;
            }
            after_base |= crate::bidi::is_override(c);
            end += c.len_utf8();
        }
        self.pos = end;
        let cluster = &self.text[start..end];
        let width = if cluster.len() == base.len_utf8() {
            char_cell_width(base)
        } else {
            cluster.width().min(2) as u8
        };
        Some((start..end, width.max(1)))
    }
}

#[cfg(test)]
mod tests;

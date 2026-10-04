//! The cell layout of text drawn outside the grid (the IME pre-edit,
//! the bottom bars), so it takes the cells printing it would.

use std::ops::Range;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::editing::{ends_in_pictographic_joiner, is_emoji_modifier, is_regional_indicator};
use crate::uax29::is_extended_pictographic;

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
        for c in chars {
            let cluster = &self.text[start..end];
            let folds = char_width(c) == 0
                || is_emoji_modifier(c)
                || (is_extended_pictographic(c) && ends_in_pictographic_joiner(cluster))
                || (is_regional_indicator(c)
                    && cluster.chars().eq([base])
                    && is_regional_indicator(base));
            if !folds {
                break;
            }
            end += c.len_utf8();
        }
        self.pos = end;
        let cluster = &self.text[start..end];
        let width = if cluster.len() == base.len_utf8() {
            char_width(base)
        } else {
            cluster.width().min(2) as u8
        };
        Some((start..end, width.max(1)))
    }
}

/// The width the print path gives a lone scalar.
fn char_width(c: char) -> u8 {
    match crate::sink::table_width(crate::sink::bmp_widths(), c) {
        0 => c.width().unwrap_or(0) as u8,
        w => w as u8,
    }
}

#[cfg(test)]
mod tests;

//! One screen row as the text an accessibility tree exposes
//! (`docs/reference/accessibility.md`): one character per grid
//! character, with the column geometry and the word boundaries the tree
//! builder cuts into runs. Pure data; the AccessKit mapping lives in the
//! GUI client.

use felis_grid::{Grapheme, ScreenBuffer};

use crate::selection::{WordClass, classify_char};

/// AccessKit indexes characters within a text run with `u8`.
pub const RUN_CAP: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowText {
    pub value: String,
    /// UTF-8 bytes of each character; a cluster is at most
    /// `ClusterText::CAP` bytes, so every length fits.
    pub char_lengths: Vec<u8>,
    /// The first column each character covers.
    pub char_cols: Vec<u16>,
    /// Columns each character covers: 1 plus its trailing `Spacer` or
    /// `SizedSpacer` cells on this row; 0 for the line break.
    pub char_spans: Vec<u16>,
    /// Row-wide character indices, sorted.
    pub word_starts: Vec<usize>,
    pub ends_line: bool,
    last_class: Option<WordClass>,
}

struct Char {
    col: u16,
    span: u16,
    len: u8,
    class: WordClass,
    blank: bool,
    sizing: Option<felis_grid::SizingHandle>,
}

impl RowText {
    /// `pad_col` names a column that must map to a character of its own
    /// even past the trimmed content: the caret or a selection endpoint.
    /// `prev` is the row above, consulted only when this row continues
    /// it, so a word cut by the wrap does not start again here.
    #[must_use]
    pub fn build(
        screen: &ScreenBuffer,
        row: u16,
        pad_col: Option<u16>,
        prev: Option<&Self>,
    ) -> Self {
        let mut value = String::new();
        let mut chars: Vec<Char> = Vec::new();
        for col in 0..screen.cols() {
            let Some(cell) = screen.cell(row, col) else {
                break;
            };
            let contiguous = chars.last().is_some_and(|c| c.col + c.span == col);
            match cell.grapheme {
                Grapheme::Spacer => {
                    if contiguous && let Some(last) = chars.last_mut() {
                        last.span += 1;
                    }
                    continue;
                }
                // A continuation row of a sized run has no owner on its
                // own row, so its cells join nothing.
                Grapheme::SizedSpacer => {
                    if contiguous
                        && let Some(last) = chars.last_mut()
                        && last.sizing.is_some()
                        && last.sizing == cell.sizing
                    {
                        last.span += 1;
                    }
                    continue;
                }
                _ => {}
            }
            let start = value.len();
            let blank = matches!(cell.grapheme, Grapheme::Empty);
            felis_grid::push_cell_text(cell, screen.cluster_table(), &mut value);
            if value.len() == start {
                value.push(' ');
            }
            let class = value[start..]
                .chars()
                .next()
                .map_or(WordClass::Whitespace, classify_char);
            chars.push(Char {
                col,
                span: 1,
                len: u8::try_from(value.len() - start).unwrap_or(u8::MAX),
                class,
                blank,
                sizing: cell.sizing,
            });
        }

        let content_end = chars.iter().rposition(|c| !c.blank).map_or(0, |i| i + 1);
        let pad_end = pad_col.map_or(0, |pad| {
            chars
                .iter()
                .rposition(|c| c.col <= pad)
                .map_or(0, |i| i + 1)
        });
        let keep = content_end.max(pad_end);
        let kept_bytes: usize = chars[..keep].iter().map(|c| usize::from(c.len)).sum();
        chars.truncate(keep);
        value.truncate(kept_bytes);

        let ends_line = !screen.row_soft_wrap_continued(row.saturating_add(1));
        if chars.is_empty() && !ends_line {
            value.push(' ');
            chars.push(Char {
                col: 0,
                span: 1,
                len: 1,
                class: WordClass::Whitespace,
                blank: true,
                sizing: None,
            });
        }
        let line_break_col = chars.last().map_or(0, |c| c.col + c.span);
        if ends_line {
            value.push('\n');
            chars.push(Char {
                col: line_break_col,
                span: 0,
                len: 1,
                class: WordClass::Whitespace,
                blank: true,
                sizing: None,
            });
        }

        let continued_from = if screen.row_soft_wrap_continued(row) {
            prev.and_then(|p| p.last_class)
        } else {
            None
        };
        let mut word_starts = Vec::new();
        let mut before = continued_from;
        for (i, c) in chars.iter().enumerate() {
            if before != Some(c.class) {
                word_starts.push(i);
            }
            before = Some(c.class);
        }

        Self {
            value,
            char_lengths: chars.iter().map(|c| c.len).collect(),
            char_cols: chars.iter().map(|c| c.col).collect(),
            char_spans: chars.iter().map(|c| c.span).collect(),
            word_starts,
            ends_line,
            last_class: chars.last().map(|c| c.class),
        }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.char_lengths.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.char_lengths.is_empty()
    }

    /// The character covering `col`, or the boundary before the next
    /// character when none does (an ownerless `SizedSpacer`, or a
    /// trimmed blank). Never past a line break: AccessKit puts a caret
    /// at the end of a hard line on the break itself.
    #[must_use]
    pub fn index_of_col(&self, col: u16) -> usize {
        let next = self.char_cols.partition_point(|&c| c <= col);
        if next > 0 {
            let i = next - 1;
            if col < self.char_cols[i] + self.char_spans[i] {
                return i;
            }
        }
        next.min(self.len() - usize::from(self.ends_line))
    }

    /// The boundary after the character covering `col`.
    #[must_use]
    pub fn index_after_col(&self, col: u16) -> usize {
        let i = self.index_of_col(col);
        let covered = self
            .char_cols
            .get(i)
            .is_some_and(|&start| start <= col && col < start + self.char_spans[i]);
        if covered { i + 1 } else { i }
    }

    /// `(run, index within run)` for a row-wide character boundary; the
    /// end of the row stays in the last run rather than opening an empty
    /// one.
    #[must_use]
    pub const fn run_position(&self, index: usize) -> (usize, usize) {
        let len = self.len();
        if index >= len && len > 0 {
            let last = (len - 1) / RUN_CAP;
            return (last, len - last * RUN_CAP);
        }
        (index / RUN_CAP, index % RUN_CAP)
    }

    #[must_use]
    pub const fn run_count(&self) -> usize {
        self.len().div_ceil(RUN_CAP)
    }
}

#[cfg(test)]
mod tests {
    use felis_grid::Grid;
    use proptest::prelude::*;

    use super::*;
    use crate::selection::{GridPos, Selection};

    fn grid_with(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
        let mut g = Grid::new(rows, cols);
        let mut p = felis_vt::Parser::new();
        p.advance(&mut g, bytes);
        g
    }

    fn rows_of(g: &Grid) -> Vec<RowText> {
        let mut out: Vec<RowText> = Vec::new();
        for r in 0..g.screen().rows() {
            let t = RowText::build(g.screen(), r, None, out.last());
            out.push(t);
        }
        out
    }

    #[test]
    fn trailing_blanks_are_trimmed_and_a_hard_line_ends_with_a_line_break() {
        let g = grid_with(2, 8, b"ab  c\r\n");
        let rows = rows_of(&g);
        assert_eq!(rows[0].value, "ab  c\n");
        assert_eq!(rows[1].value, "\n");
    }

    #[test]
    fn a_soft_wrapped_row_carries_no_line_break() {
        let g = grid_with(2, 3, b"abcde");
        let rows = rows_of(&g);
        assert_eq!(rows[0].value, "abc");
        assert!(!rows[0].ends_line);
        assert_eq!(rows[1].value, "de\n");
    }

    #[test]
    fn a_wide_character_is_one_character_spanning_two_columns() {
        let g = grid_with(1, 6, "a字b".as_bytes());
        let t = &rows_of(&g)[0];
        assert_eq!(t.value, "a字b\n");
        assert_eq!(t.char_lengths, vec![1, 3, 1, 1]);
        assert_eq!(t.char_cols, vec![0, 1, 3, 4]);
        assert_eq!(t.char_spans, vec![1, 2, 1, 0]);
        assert_eq!(t.index_of_col(2), 1);
        assert_eq!(t.index_after_col(2), 2);
    }

    #[test]
    fn a_sized_character_spans_its_columns_and_leaves_its_lower_row_ownerless() {
        let g = grid_with(3, 8, b"x\x1b]66;s=2;A\x07y");
        let rows = rows_of(&g);
        assert_eq!(rows[0].value, "xAy\n");
        assert_eq!(rows[0].char_cols, vec![0, 1, 3, 4]);
        assert_eq!(rows[0].char_spans, vec![1, 2, 1, 0]);
        // The lower half of the sized run belongs to no character.
        assert_eq!(rows[1].value, "\n");
        assert_eq!(rows[1].index_of_col(1), 0);
    }

    #[test]
    fn padding_reaches_a_caret_past_the_content() {
        let g = grid_with(1, 8, b"ab");
        let t = RowText::build(g.screen(), 0, Some(5), None);
        assert_eq!(t.value, "ab    \n");
        assert_eq!(t.index_of_col(5), 5);
    }

    #[test]
    fn an_erased_wrapped_row_keeps_one_space() {
        let g = grid_with(2, 3, b"abcde\x1b[1;1H\x1b[2K");
        let rows = rows_of(&g);
        assert_eq!(rows[0].value, " ");
        assert_eq!(rows[0].len(), 1);
    }

    #[test]
    fn a_word_cut_by_a_wrap_does_not_start_again_on_the_next_row() {
        let g = grid_with(3, 3, b"abcde f");
        let rows = rows_of(&g);
        assert_eq!(rows[0].word_starts, vec![0]);
        assert_eq!(rows[1].word_starts, vec![2]);
        assert_eq!(rows[2].word_starts, vec![0, 1]);
    }

    #[test]
    fn word_starts_agree_with_double_click_selection() {
        let line = b"foo...bar  _x, y";
        let g = grid_with(1, 20, line);
        let t = &rows_of(&g)[0];
        let mut from_selection = Vec::new();
        let mut col = 0;
        while usize::from(col) < line.len() {
            from_selection.push(usize::from(col));
            let s = Selection::word_at(g.screen(), GridPos { row: 0, col }).unwrap();
            col = s.range().1.1 + 1;
        }
        let content_starts: Vec<usize> = t
            .word_starts
            .iter()
            .copied()
            .filter(|&i| i < line.len())
            .collect();
        assert_eq!(content_starts, from_selection);
    }

    #[test]
    fn the_end_of_a_row_stays_in_its_last_run() {
        let g = grid_with(1, 300, &[b'a'; 254]);
        let t = &rows_of(&g)[0];
        assert_eq!(t.len(), 255);
        assert_eq!(t.run_count(), 1);
        assert_eq!(t.run_position(255), (0, 255));
        assert_eq!(t.run_position(254), (0, 254));
    }

    proptest! {
        /// Pins: the character lengths partition the value, no row is
        /// empty, and every column maps to a boundary within the row.
        #[test]
        fn lengths_partition_the_value_and_every_column_maps_into_the_row(
            text in "[a-c字 \\r\\n]{0,40}",
            cols in 1u16..12,
            rows in 1u16..5,
            pad in 0u16..12,
        ) {
            let g = grid_with(rows, cols, text.as_bytes());
            let mut prev: Option<RowText> = None;
            for r in 0..rows {
                let pad_col = (pad < cols).then_some(pad);
                let t = RowText::build(g.screen(), r, pad_col, prev.as_ref());
                let total: usize = t.char_lengths.iter().map(|&l| usize::from(l)).sum();
                prop_assert_eq!(total, t.value.len());
                prop_assert!(!t.is_empty());
                prop_assert!(t.word_starts.windows(2).all(|w| w[0] < w[1]));
                for col in 0..cols {
                    prop_assert!(t.index_of_col(col) <= t.len());
                    prop_assert!(t.index_after_col(col) <= t.len());
                }
                if let Some(p) = pad_col {
                    prop_assert!(t.index_of_col(p) < t.len() - usize::from(t.ends_line));
                }
                prev = Some(t);
            }
        }
    }
}

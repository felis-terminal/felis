//! Mouse-driven cell selection model: pure data, no winit, IO or wire
//! dependency. The App owns the lifecycle; the renderer treats
//! `Option<Selection>` as a per-frame highlight hint.

use felis_grid::{Grapheme, ScreenBuffer};

/// The wire's X10/SGR mouse encodings speak 1-based `(col, row)`; that
/// shift happens once, at the `InputMsg::Mouse` boundary in the GUI client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPos {
    /// 0-based screen row.
    pub row: u16,
    /// 0-based screen column.
    pub col: u16,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Row-major reading order; boundary rows clip to the column range.
    #[default]
    Linear,
    /// Column-range x row-range: every row clips to `[sc, ec]`.
    Rectangle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    anchor: GridPos,
    extent: GridPos,
    mode: SelectionMode,
}

impl Selection {
    #[must_use]
    pub const fn new(cell: GridPos) -> Self {
        Self {
            anchor: cell,
            extent: cell,
            mode: SelectionMode::Linear,
        }
    }

    #[must_use]
    pub const fn new_rectangle(cell: GridPos) -> Self {
        Self {
            anchor: cell,
            extent: cell,
            mode: SelectionMode::Rectangle,
        }
    }

    pub const fn extend(&mut self, to: GridPos) {
        self.extent = to;
    }

    /// Normalized `(start, end)`: row-major for Linear, bounding-box
    /// corners for Rectangle. One tuple shape for both so the renderer
    /// consumes one seam and switches on its own `rectangle` flag.
    #[must_use]
    pub const fn range(self) -> ((u16, u16), (u16, u16)) {
        match self.mode {
            SelectionMode::Linear => {
                if cell_lt_or_eq(self.anchor, self.extent) {
                    (
                        (self.anchor.row, self.anchor.col),
                        (self.extent.row, self.extent.col),
                    )
                } else {
                    (
                        (self.extent.row, self.extent.col),
                        (self.anchor.row, self.anchor.col),
                    )
                }
            }
            SelectionMode::Rectangle => {
                let sr = if self.anchor.row <= self.extent.row {
                    self.anchor.row
                } else {
                    self.extent.row
                };
                let er = if self.anchor.row >= self.extent.row {
                    self.anchor.row
                } else {
                    self.extent.row
                };
                let sc = if self.anchor.col <= self.extent.col {
                    self.anchor.col
                } else {
                    self.extent.col
                };
                let ec = if self.anchor.col >= self.extent.col {
                    self.anchor.col
                } else {
                    self.extent.col
                };
                ((sr, sc), (er, ec))
            }
        }
    }

    #[must_use]
    pub const fn mode(self) -> SelectionMode {
        self.mode
    }

    /// Extracts text with trimmed trailing whitespace, joining rows with `\n`.
    ///
    /// Soft-wrapped lines join without `\n`: trailing `Empty` cells drop while
    /// explicit `Ascii(b' ')` cells survive, matching the search stitcher.
    #[must_use]
    pub fn extract_text(self, screen: &ScreenBuffer) -> String {
        let ((sr, sc), (er, ec)) = self.range();
        let mut out = String::new();
        let cols = screen.cols();
        let last_col = cols.saturating_sub(1);
        for row in sr..=er {
            let (row_start, row_end) = match self.mode {
                SelectionMode::Linear => {
                    let s = if row == sr { sc } else { 0 };
                    let e = if row == er { ec } else { last_col };
                    (s, e)
                }
                SelectionMode::Rectangle => (sc, ec),
            };
            let mut line = String::new();
            let mut written_len = 0;
            for col in row_start..=row_end {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                felis_grid::push_cell_text(cell, screen.cluster_table(), &mut line);
                if !matches!(cell.grapheme, Grapheme::Empty) {
                    written_len = line.len();
                }
            }
            let wraps_into_next = matches!(self.mode, SelectionMode::Linear)
                && row < er
                && screen.row_soft_wrap_continued(row + 1);
            if wraps_into_next {
                line.truncate(written_len);
                out.push_str(&line);
            } else {
                out.push_str(line.trim_end());
                if row < er {
                    out.push('\n');
                }
            }
        }
        out
    }

    /// Returns `None` only for an out-of-range row; an in-range click
    /// on an empty cell still yields a selection so the renderer has
    /// something to flash.
    #[must_use]
    pub fn word_at(screen: &ScreenBuffer, cell: GridPos) -> Option<Self> {
        let row = cell.row;
        if row >= screen.rows() {
            return None;
        }
        let cols = screen.cols();
        if cols == 0 {
            return None;
        }
        let col = cell.col.min(cols.saturating_sub(1));
        let target = cell_class(screen, row, col);
        let mut start_col = col;
        while start_col > 0 && cell_class(screen, row, start_col - 1) == target {
            start_col -= 1;
        }
        let mut end_col = col;
        while end_col + 1 < cols && cell_class(screen, row, end_col + 1) == target {
            end_col += 1;
        }
        let mut s = Self::new(GridPos {
            row,
            col: start_col,
        });
        s.extend(GridPos { row, col: end_col });
        Some(s)
    }

    /// The logical line: the clicked row plus every visible row its
    /// `soft_wrap_continued` bits tie to it, the same stitching the
    /// daemon's scrollback search applies, clipped at the screen edges.
    /// Returns `None` for an out-of-range row or a zero-column screen.
    #[must_use]
    pub fn line_at(screen: &ScreenBuffer, row: u16) -> Option<Self> {
        if row >= screen.rows() {
            return None;
        }
        let cols = screen.cols();
        if cols == 0 {
            return None;
        }
        let (start, end) = screen.logical_line_bounds(row);
        let mut s = Self::new(GridPos { row: start, col: 0 });
        s.extend(GridPos {
            row: end,
            col: cols - 1,
        });
        Some(s)
    }

    #[must_use]
    pub const fn contains(self, row: u16, col: u16) -> bool {
        let ((sr, sc), (er, ec)) = self.range();
        if row < sr || row > er {
            return false;
        }
        match self.mode {
            SelectionMode::Linear => {
                if row == sr && col < sc {
                    return false;
                }
                if row == er && col > ec {
                    return false;
                }
                true
            }
            SelectionMode::Rectangle => col >= sc && col <= ec,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Word,
    Whitespace,
    Punct,
}

fn cell_class(screen: &ScreenBuffer, row: u16, col: u16) -> WordClass {
    let Some(cell) = screen.cell(row, col) else {
        return WordClass::Whitespace;
    };
    classify_grapheme(screen, row, col, cell.grapheme)
}

fn classify_grapheme(screen: &ScreenBuffer, row: u16, col: u16, g: Grapheme) -> WordClass {
    match g {
        Grapheme::Empty | Grapheme::SizedSpacer => WordClass::Whitespace,
        Grapheme::Spacer => {
            // Two adjacent Spacers cannot happen (the VT never stamps
            // them), so that case falls back to Whitespace.
            if col == 0 {
                return WordClass::Whitespace;
            }
            let Some(left) = screen.cell(row, col - 1) else {
                return WordClass::Whitespace;
            };
            match left.grapheme {
                Grapheme::Spacer => WordClass::Whitespace,
                other => classify_grapheme(screen, row, col - 1, other),
            }
        }
        Grapheme::Ascii(b) => classify_char(b as char),
        Grapheme::Char(c) => classify_char(c),
        // A cluster classifies by its leading char; the marks that
        // follow are zero-width.
        Grapheme::Cluster(id) => screen
            .cluster_str(id)
            .and_then(|s| s.chars().next())
            .map_or(WordClass::Whitespace, classify_char),
    }
}

fn classify_char(c: char) -> WordClass {
    if c == '_' || c.is_alphanumeric() {
        WordClass::Word
    } else if c.is_whitespace() {
        WordClass::Whitespace
    } else {
        WordClass::Punct
    }
}

const fn cell_lt_or_eq(a: GridPos, b: GridPos) -> bool {
    if a.row < b.row {
        return true;
    }
    if a.row > b.row {
        return false;
    }
    a.col <= b.col
}

#[cfg(test)]
mod tests {
    use felis_grid::Grid;

    use proptest::prelude::*;

    use super::*;

    const fn at(row: u16, col: u16) -> GridPos {
        GridPos { row, col }
    }

    fn grid_with(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
        let mut g = Grid::new(rows, cols);
        let mut p = felis_vt::Parser::new();
        p.advance(&mut g, bytes);
        g
    }

    #[test]
    fn extract_text_single_cell_returns_that_glyph() {
        let g = grid_with(1, 5, b"hello");
        let mut s = Selection::new(at(0, 0));
        s.extend(at(0, 0));
        assert_eq!(s.extract_text(g.screen()), "h");
    }

    #[test]
    fn extract_text_single_row_concatenates_glyphs() {
        let g = grid_with(1, 5, b"hello");
        let mut s = Selection::new(at(0, 1));
        s.extend(at(0, 3));
        assert_eq!(s.extract_text(g.screen()), "ell");
    }

    #[test]
    fn extract_text_trims_trailing_spaces_from_each_line() {
        let g = grid_with(1, 5, b"hi");
        let mut s = Selection::new(at(0, 0));
        s.extend(at(0, 4));
        assert_eq!(s.extract_text(g.screen()), "hi");
    }

    #[test]
    fn extract_text_multi_row_joins_with_newlines() {
        let g = grid_with(2, 4, b"ab\r\ncd");
        let mut s = Selection::new(at(0, 0));
        s.extend(at(1, 3));
        assert_eq!(s.extract_text(g.screen()), "ab\ncd");
    }

    #[test]
    fn extract_text_clips_boundary_rows_to_column_range() {
        let g = grid_with(3, 4, b"abcd\r\nefgh\r\nijkl");
        let mut s = Selection::new(at(0, 2));
        s.extend(at(2, 1));
        assert_eq!(s.extract_text(g.screen()), "cd\nefgh\nij");
    }

    #[test]
    fn extract_text_skips_wide_glyph_spacer_cell() {
        let g = grid_with(1, 4, "あ".as_bytes());
        let mut s = Selection::new(at(0, 0));
        s.extend(at(0, 1));
        assert_eq!(s.extract_text(g.screen()), "あ");
    }

    #[test]
    fn word_at_picks_an_alphanumeric_run() {
        let g = grid_with(1, 11, b"hello world");
        let s = Selection::word_at(g.screen(), at(0, 3)).unwrap();
        assert_eq!(s.range(), ((0, 0), (0, 4)));
        assert_eq!(s.extract_text(g.screen()), "hello");
    }

    #[test]
    fn word_at_on_whitespace_picks_the_run_of_spaces() {
        // extract_text trims to empty, but the highlighted range still
        // covers the spaces.
        let g = grid_with(1, 6, b"ab  cd");
        let s = Selection::word_at(g.screen(), at(0, 2)).unwrap();
        assert_eq!(s.range(), ((0, 2), (0, 3)));
    }

    #[test]
    fn word_at_on_punct_selects_only_the_single_char() {
        let g = grid_with(1, 7, b"hi, you");
        let s = Selection::word_at(g.screen(), at(0, 2)).unwrap();
        assert_eq!(s.range(), ((0, 2), (0, 2)));
        assert_eq!(s.extract_text(g.screen()), ",");
    }

    #[test]
    fn word_at_includes_underscore_in_word() {
        let g = grid_with(1, 11, b"foo_bar baz");
        let s = Selection::word_at(g.screen(), at(0, 0)).unwrap();
        assert_eq!(s.range(), ((0, 0), (0, 6)));
        assert_eq!(s.extract_text(g.screen()), "foo_bar");
    }

    #[test]
    fn word_at_handles_unicode_alpha_runs() {
        let g = grid_with(1, 6, "あいう".as_bytes());
        let s = Selection::word_at(g.screen(), at(0, 2)).unwrap();
        assert_eq!(s.range(), ((0, 0), (0, 5)));
        assert_eq!(s.extract_text(g.screen()), "あいう");
    }

    #[test]
    fn word_at_on_wide_glyph_spacer_inherits_left_half_class() {
        let g = grid_with(1, 6, "あいう".as_bytes());
        let s = Selection::word_at(g.screen(), at(0, 1)).unwrap();
        assert_eq!(s.range(), ((0, 0), (0, 5)));
    }

    #[test]
    fn word_at_returns_none_when_row_out_of_range() {
        let g = grid_with(1, 5, b"hello");
        assert!(Selection::word_at(g.screen(), at(1, 0)).is_none());
    }

    #[test]
    fn word_at_clamps_column_to_last_valid_cell() {
        // Cursor coords can hit cols == screen.cols when the renderer's
        // metrics race a resize.
        let g = grid_with(1, 5, b"hello");
        let s = Selection::word_at(g.screen(), at(0, 99)).unwrap();
        assert_eq!(s.range(), ((0, 0), (0, 4)));
    }

    #[test]
    fn line_at_selects_whole_row_columns() {
        let g = grid_with(2, 5, b"hello\r\nworld");
        let s = Selection::line_at(g.screen(), 1).unwrap();
        assert_eq!(s.range(), ((1, 0), (1, 4)));
        assert_eq!(s.extract_text(g.screen()), "world");
    }

    #[test]
    fn line_at_returns_none_for_out_of_range_row() {
        let g = grid_with(2, 5, b"");
        assert!(Selection::line_at(g.screen(), 2).is_none());
    }

    #[test]
    fn line_at_extract_trims_padding() {
        let g = grid_with(1, 5, b"hi");
        let s = Selection::line_at(g.screen(), 0).unwrap();
        assert_eq!(s.extract_text(g.screen()), "hi");
    }

    #[test]
    fn line_at_stitches_autowrapped_rows_into_one_logical_line() {
        // "abcdef" autowraps in a 3-col grid; the copy must paste back
        // as the single line the program printed.
        let g = grid_with(3, 3, b"abcdef");
        for row in [0, 1] {
            let s = Selection::line_at(g.screen(), row).unwrap();
            assert_eq!(s.range(), ((0, 0), (1, 2)), "clicked row {row}");
            assert_eq!(s.extract_text(g.screen()), "abcdef");
        }
    }

    #[test]
    fn line_at_does_not_stitch_across_hard_newlines() {
        let g = grid_with(3, 3, b"abc\r\ndef");
        let s = Selection::line_at(g.screen(), 0).unwrap();
        assert_eq!(s.range(), ((0, 0), (0, 2)));
        assert_eq!(s.extract_text(g.screen()), "abc");
    }

    #[test]
    fn line_at_drops_wide_glyph_wrap_pad_but_keeps_printed_spaces() {
        // Pins: the unfillable Empty pad a wide-glyph wrap leaves must not
        // decode to a phantom space (mirror of the search stitcher's
        // trim_trailing_empty).
        let g = grid_with(2, 3, "ああ".as_bytes());
        let s = Selection::line_at(g.screen(), 0).unwrap();
        assert_eq!(s.extract_text(g.screen()), "ああ");
        // Printed spaces at the wrap edge are content, not padding.
        let g = grid_with(2, 3, b"a  b");
        let s = Selection::line_at(g.screen(), 0).unwrap();
        assert_eq!(s.extract_text(g.screen()), "a  b");
    }

    #[test]
    fn drag_selection_across_soft_wrap_joins_without_newline() {
        // Pins: soft-wrap joining is a property of extract_text, not of
        // the triple-click gesture.
        let g = grid_with(3, 3, b"abcdef");
        let mut s = Selection::new(at(0, 1));
        s.extend(at(1, 1));
        assert_eq!(s.extract_text(g.screen()), "bcde");
    }

    #[test]
    fn rectangle_extract_text_joins_rows_with_clipped_columns() {
        let g = grid_with(3, 6, b"abcdef\r\nghijkl\r\nmnopqr");
        let mut s = Selection::new_rectangle(at(0, 1));
        s.extend(at(2, 3));
        assert_eq!(s.extract_text(g.screen()), "bcd\nhij\nnop");
    }

    #[test]
    fn rectangle_extract_text_trims_trailing_whitespace_per_row() {
        let g = grid_with(2, 5, b"ab\r\ncd");
        let mut s = Selection::new_rectangle(at(0, 0));
        s.extend(at(1, 4));
        assert_eq!(s.extract_text(g.screen()), "ab\ncd");
    }

    #[test]
    fn rectangle_mode_round_trips_through_mode_accessor() {
        let s = Selection::new_rectangle(at(0, 0));
        assert_eq!(s.mode(), SelectionMode::Rectangle);
    }

    /// Mirrors `felis-render-wgpu`'s `selection_contains` predicate.
    ///
    /// Restated here because client-core cannot depend on the renderer
    /// (`docs/explanation/architecture/overview.md`).
    fn renderer_selection_contains(
        start: (u16, u16),
        end: (u16, u16),
        rectangle: bool,
        row: u16,
        col: u16,
    ) -> bool {
        let (sr, sc) = start;
        let (er, ec) = end;
        if row < sr || row > er {
            return false;
        }
        if rectangle {
            return col >= sc && col <= ec;
        }
        if row == sr && col < sc {
            return false;
        }
        if row == er && col > ec {
            return false;
        }
        true
    }

    /// Filled edge to edge with printable ASCII so no trailing-whitespace
    /// trim can fire; `hard_breaks[r]` ends row `r` with CR LF instead of
    /// autowrap, which decides the `soft_wrap_continued` bit below it.
    fn dense_grid(rows: u16, cols: u16, hard_breaks: &[bool]) -> Grid {
        let mut bytes = Vec::new();
        for r in 0..rows {
            for c in 0..cols {
                let n = u32::from(r) * u32::from(cols) + u32::from(c);
                bytes.push(b'a' + u8::try_from(n % 26).unwrap());
            }
            if r + 1 < rows && hard_breaks.get(r as usize).copied().unwrap_or(false) {
                bytes.extend_from_slice(b"\r\n");
            }
        }
        grid_with(rows, cols, &bytes)
    }

    proptest! {
        /// Pins: on a screen with no blank cells, `extract_text` emits
        /// exactly the glyphs the renderer's `selection_contains` mirror
        /// reports as inside, in row-major order. Selections are
        /// generated past the screen's edges too, where the two clipping
        /// rules could drift apart.
        #[test]
        fn extract_text_covers_exactly_the_cells_the_renderer_highlights(
            rows in 1u16..=4,
            cols in 1u16..=5,
            hard_breaks in prop::collection::vec(any::<bool>(), 4),
            rectangle in any::<bool>(),
            ar in 0u16..7,
            ac in 0u16..7,
            er in 0u16..7,
            ec in 0u16..7,
        ) {
            let screen = dense_grid(rows, cols, &hard_breaks);
            let mut sel = if rectangle {
                Selection::new_rectangle(at(ar, ac))
            } else {
                Selection::new(at(ar, ac))
            };
            sel.extend(at(er, ec));

            let (start, end) = sel.range();
            let mut want = String::new();
            for row in start.0..=end.0 {
                for col in 0..screen.cols() {
                    if !renderer_selection_contains(start, end, rectangle, row, col) {
                        continue;
                    }
                    if let Some(cell) = screen.cell(row, col) {
                        felis_grid::push_cell_text(cell, screen.cluster_table(), &mut want);
                    }
                }
                let wraps_into_next =
                    !rectangle && row < end.0 && screen.row_soft_wrap_continued(row + 1);
                if row < end.0 && !wraps_into_next {
                    want.push('\n');
                }
            }
            prop_assert_eq!(sel.extract_text(screen.screen()), want);
        }

        /// Pins: `Selection::contains` and the renderer's mirror agree
        /// cell for cell in both modes.
        #[test]
        fn contains_agrees_with_the_renderer_mirror(
            rectangle in any::<bool>(),
            ar in 0u16..30,
            ac in 0u16..30,
            er in 0u16..30,
            ec in 0u16..30,
            r  in 0u16..30,
            c  in 0u16..30,
        ) {
            let mut sel = if rectangle {
                Selection::new_rectangle(at(ar, ac))
            } else {
                Selection::new(at(ar, ac))
            };
            sel.extend(at(er, ec));
            let (start, end) = sel.range();
            prop_assert_eq!(
                sel.contains(r, c),
                renderer_selection_contains(start, end, rectangle, r, c),
            );
        }

        /// Pins: the anchor and extent cells are contained regardless of
        /// drag direction.
        #[test]
        fn endpoints_are_always_contained(
            ar in 0u16..200,
            ac in 0u16..200,
            er in 0u16..200,
            ec in 0u16..200,
        ) {
            let mut s = Selection::new(at(ar, ac));
            s.extend(at(er, ec));
            prop_assert!(s.contains(ar, ac));
            prop_assert!(s.contains(er, ec));
        }

        #[test]
        fn range_normalization_is_symmetric(
            ar in 0u16..200,
            ac in 0u16..200,
            er in 0u16..200,
            ec in 0u16..200,
        ) {
            let mut forward = Selection::new(at(ar, ac));
            forward.extend(at(er, ec));
            let mut backward = Selection::new(at(er, ec));
            backward.extend(at(ar, ac));
            prop_assert_eq!(forward.range(), backward.range());
            let (lo, hi) = ((ar, ac).min((er, ec)), (ar, ac).max((er, ec)));
            prop_assert_eq!(forward.range(), (lo, hi));
        }

        #[test]
        fn contains_matches_row_major_interval_predicate(
            ar in 0u16..50,
            ac in 0u16..50,
            er in 0u16..50,
            ec in 0u16..50,
            r  in 0u16..50,
            c  in 0u16..50,
        ) {
            let mut s = Selection::new(at(ar, ac));
            s.extend(at(er, ec));
            let (start, end) = s.range();
            let scalar = |(r, c): (u16, u16)| u32::from(r) * 1_000 + u32::from(c);
            let want = scalar(start) <= scalar((r, c)) && scalar((r, c)) <= scalar(end);
            prop_assert_eq!(s.contains(r, c), want);
        }

        #[test]
        fn rectangle_endpoints_are_always_contained(
            ar in 0u16..200,
            ac in 0u16..200,
            er in 0u16..200,
            ec in 0u16..200,
        ) {
            let mut s = Selection::new_rectangle(at(ar, ac));
            s.extend(at(er, ec));
            prop_assert!(s.contains(ar, ac));
            prop_assert!(s.contains(er, ec));
        }

        #[test]
        fn rectangle_range_is_axis_symmetric(
            ar in 0u16..200,
            ac in 0u16..200,
            er in 0u16..200,
            ec in 0u16..200,
        ) {
            let mut a = Selection::new_rectangle(at(ar, ac));
            a.extend(at(er, ec));
            let mut b = Selection::new_rectangle(at(ar, ec));
            b.extend(at(er, ac));
            prop_assert_eq!(a.range(), b.range());
            let mut c = Selection::new_rectangle(at(er, ec));
            c.extend(at(ar, ac));
            prop_assert_eq!(a.range(), c.range());
            prop_assert_eq!(
                a.range(),
                ((ar.min(er), ac.min(ec)), (ar.max(er), ac.max(ec))),
            );
        }

        #[test]
        fn rectangle_contains_matches_axis_intersection(
            ar in 0u16..50,
            ac in 0u16..50,
            er in 0u16..50,
            ec in 0u16..50,
            r  in 0u16..50,
            c  in 0u16..50,
        ) {
            let mut s = Selection::new_rectangle(at(ar, ac));
            s.extend(at(er, ec));
            let ((sr, sc), (xr, xc)) = s.range();
            let want = r >= sr && r <= xr && c >= sc && c <= xc;
            prop_assert_eq!(s.contains(r, c), want);
        }

        /// Pins: on a single row, linear boundary clipping reduces to
        /// rectangle's column band.
        #[test]
        fn single_row_linear_equals_rectangle(
            row in 0u16..50,
            ac  in 0u16..50,
            ec  in 0u16..50,
            c   in 0u16..50,
        ) {
            let mut linear = Selection::new(at(row, ac));
            linear.extend(at(row, ec));
            let mut rect = Selection::new_rectangle(at(row, ac));
            rect.extend(at(row, ec));
            prop_assert_eq!(linear.contains(row, c), rect.contains(row, c));
        }
    }
}

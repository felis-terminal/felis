//! Text-sizing (OSC 66) state: the sized-run registry, per-cell
//! handle stamping, and the spanned-block bookkeeping the OSC 66
//! dispatcher and the renderer read.

use super::{Cell, Grapheme, Grid, Sizing, SizingHandle, ViewportRow};

/// An OSC 66 character's footprint: `rows × cols` cells from its
/// primary at `(top, left)`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SizedBlock {
    pub(crate) top: u16,
    pub(crate) left: u16,
    rows: u16,
    cols: u16,
    handle: SizingHandle,
}

impl SizedBlock {
    pub(crate) const fn within(self, top: u16, left: u16, rows: u16, cols: u16) -> bool {
        self.top >= top
            && self.left >= left
            && self.top as u32 + self.rows as u32 <= top as u32 + rows as u32
            && self.left as u32 + self.cols as u32 <= left as u32 + cols as u32
    }

    fn same_as(&self, other: &Self) -> bool {
        self.top == other.top && self.left == other.left && self.handle == other.handle
    }
}

impl Grid {
    /// The block of the OSC 66 character covering `(row, col)`. One run
    /// shares a handle across its characters, so the primary is the
    /// nearest non-continuation cell left of `col` on the block's top
    /// row, and the handle alone cannot bound the block.
    pub(crate) fn sized_block_at(&self, row: u16, col: u16) -> Option<SizedBlock> {
        let handle = self.screen.cell(row, col)?.sizing?;
        let sizing = *self.screen.sizing_table.get(handle.get() as usize - 1)?;
        let scale = u16::from(sizing.scale().max(1));
        let ours = |r: u16, c: u16| {
            self.screen
                .cell(r, c)
                .filter(|cell| cell.sizing == Some(handle))
        };
        let continues = |g: Grapheme| matches!(g, Grapheme::Spacer | Grapheme::SizedSpacer);
        for top in (row.saturating_sub(scale - 1)..=row).rev() {
            let mut left = col;
            while left > 0 && ours(top, left).is_some_and(|c| continues(c.grapheme)) {
                left -= 1;
            }
            let Some(primary) = ours(top, left).filter(|c| !continues(c.grapheme)) else {
                continue;
            };
            let (rows, cols) = self.screen.sizing_block_extent(sizing, primary.grapheme);
            if u32::from(top) + u32::from(rows) > u32::from(row)
                && u32::from(left) + u32::from(cols) > u32::from(col)
            {
                return Some(SizedBlock {
                    top,
                    left,
                    rows,
                    cols,
                    handle,
                });
            }
        }
        None
    }

    /// A foreign cell inside the block's coordinates already cleared its
    /// own sizing on its own write, so only cells still carrying the
    /// handle are cleared.
    pub(crate) fn clear_sized_block(&mut self, block: SizedBlock) {
        self.clear_sized_block_from(block, block.top);
    }

    fn clear_sized_block_from(&mut self, block: SizedBlock, first_row: u16) {
        let last_row = block.top.saturating_add(block.rows).min(self.screen.rows);
        let last_col = block.left.saturating_add(block.cols).min(self.screen.cols);
        for r in first_row.max(block.top)..last_row {
            for c in block.left..last_col {
                let idx = self.screen.idx(r, c);
                if self.screen.cells[idx].sizing == Some(block.handle) {
                    self.screen.cells[idx] = Cell::default();
                }
            }
            self.screen.damage.mark(usize::from(r));
        }
    }

    /// Distinct multi-row blocks with a cell inside the watermark of
    /// `row` × `[left, right)` that `keep` accepts.
    fn multirow_blocks_on(
        &self,
        row: u16,
        left: u16,
        right: u16,
        keep: impl Fn(&SizedBlock) -> bool,
    ) -> Vec<SizedBlock> {
        let mut out: Vec<SizedBlock> = Vec::new();
        if !self.screen.has_sized_cells() || row >= self.screen.rows {
            return out;
        }
        let phys = self.screen.phys_row(row);
        let end = right.min(self.screen.occupancy[phys]).min(self.screen.cols);
        if left >= end {
            return out;
        }
        let base = phys * usize::from(self.screen.cols);
        let cells = &self.screen.cells[base + usize::from(left)..base + usize::from(end)];
        for (col, cell) in (left..end).zip(cells) {
            if cell.sizing.is_some()
                && let Some(block) = self.sized_block_at(row, col)
                && block.rows > 1
                && keep(&block)
                && !out.iter().any(|b| b.same_as(&block))
            {
                out.push(block);
            }
        }
        out
    }

    /// A row-local shift (ICH, DCH, IRM) moves one row of a multi-row
    /// block and not the others, so it erases every one it reaches
    /// first, as kitty's `nuke_multiline_char_intersecting_with` does.
    pub(crate) fn erase_multirow_blocks_on(&mut self, row: u16, left: u16, right: u16) {
        if !self.screen.has_sized_cells() {
            return;
        }
        for block in self.multirow_blocks_on(row, left, right, |_| true) {
            self.clear_sized_block(block);
        }
    }

    /// Erases each multi-row block in `[left, right)` that a band move
    /// splits at the boundary above one of `seams`. A block whose
    /// primary is among the first `departing` rows, which go to
    /// history, keeps those rows there and loses only its live ones.
    pub(crate) fn erase_blocks_across_row_seams(
        &mut self,
        seams: &[u16],
        left: u16,
        right: u16,
        departing: u16,
    ) {
        if !self.screen.has_sized_cells() {
            return;
        }
        let mut cut: Vec<SizedBlock> = Vec::new();
        for &seam in seams {
            if seam == 0 {
                continue;
            }
            for block in self.multirow_blocks_on(seam, left, right, |b| b.top < seam) {
                if !cut.iter().any(|b| b.same_as(&block)) {
                    cut.push(block);
                }
            }
        }
        for block in cut {
            if block.top < departing {
                self.clear_sized_block_from(block, departing);
            } else {
                self.clear_sized_block(block);
            }
        }
    }

    /// Kitty spec: any write into a multi-cell character erases the
    /// whole character (the caller does the subsequent write).
    fn clear_sized_run_at(&mut self, r: u16, c: u16) {
        if let Some(block) = self.sized_block_at(r, c) {
            self.clear_sized_block(block);
        } else {
            // A handle past the registry or a block with no primary
            // left: clear just this cell so its state converges instead
            // of panicking on a fuzz-found mismatch.
            let idx = self.screen.idx(r, c);
            self.screen.cells[idx] = Cell::default();
            self.screen.damage.mark(usize::from(r));
        }
    }

    /// Same-handle writes are the dispatcher stamping its own block and
    /// must not self-erase.
    pub(crate) fn clear_foreign_sized_run(&mut self, r: u16, c: u16) {
        let Some(existing) = self.screen.cells[self.screen.idx(r, c)].sizing else {
            return;
        };
        if Some(existing) == self.current_sizing_handle {
            return;
        }
        self.clear_sized_run_at(r, c);
    }

    /// Places one OSC 66 cluster as a `w·s` × `s` block, the way kitty's
    /// `handle_fixed_width_multicell_command` does: a block larger than
    /// the screen or the scroll region is dropped (REQ-406), anything
    /// else is moved, wrapped or scrolled until it fits whole. `false`
    /// when the block is discarded.
    pub(crate) fn place_sized_cluster(
        &mut self,
        text: &str,
        sizing: Sizing,
        handle: SizingHandle,
    ) -> bool {
        let g = self.sized_cluster_grapheme(text);
        let (scale, block_w) = self.screen.sizing_block_extent(sizing, g);
        let region_h = self.margins.bottom - self.margins.top + 1;
        if block_w > self.screen.cols || scale > region_h {
            return false;
        }
        self.fit_sized_block(scale, block_w);
        if self.insert_mode {
            let row = self.screen.cursor.row;
            for r in row..row + scale {
                self.insert_mode_shift(r, usize::from(block_w));
            }
        }
        self.stamp_sized_block(g, scale, block_w, handle);
        true
    }

    fn sized_cluster_grapheme(&mut self, text: &str) -> Grapheme {
        let mut chars = text.chars();
        let Some(base) = chars.next() else {
            return Grapheme::Empty;
        };
        if chars.next().is_none() {
            return match u8::try_from(base) {
                Ok(b) if (0x20..=0x7E).contains(&b) => Grapheme::Ascii(b),
                _ => Grapheme::Char(base),
            };
        }
        self.intern_cluster(text)
            .map_or(Grapheme::Char(base), Grapheme::Cluster)
    }

    /// Moves the cursor to the block's primary: scroll or step up until
    /// `scale` rows fit, then take the first span on the row that holds
    /// no lower row of another block, wrapping under DECAWM.
    fn fit_sized_block(&mut self, scale: u16, block_w: u16) {
        let wrap_allowed = self.autowrap && block_w <= self.sized_wrap_band();
        let mut stalled = false;
        let mut wraps: u32 = 0;
        loop {
            self.fit_sized_rows(scale);
            let wrap_ok = wrap_allowed && wraps <= u32::from(self.screen.rows);
            if std::mem::take(&mut self.screen.cursor.pending_wrap) && wrap_ok {
                if !self.sized_wrap(&mut stalled) {
                    break;
                }
                wraps += 1;
                continue;
            }
            let right = self.print_right_edge_for_cursor();
            if let Some(col) = self.free_sized_span(self.screen.cursor.col, block_w, right) {
                self.screen.cursor.col = col;
                return;
            }
            if !wrap_ok || !self.sized_wrap(&mut stalled) {
                break;
            }
            wraps += 1;
        }
        let right = self.print_right_edge_for_cursor();
        self.screen.cursor.col = (right + 1).saturating_sub(block_w);
    }

    /// The columns a wrapped block can use: from the wrap's left edge to
    /// the right margin, or the screen edge without DECLRMM.
    const fn sized_wrap_band(&self) -> u16 {
        let right = if self.left_right_margin_mode {
            self.margins.right
        } else {
            self.screen.cols.saturating_sub(1)
        };
        (right + 1).saturating_sub(self.print_left_edge_for_wrap())
    }

    /// `false`, without wrapping, when a wrap already left the cursor on
    /// its row and this one would too: below the scroll region on the
    /// last row, or outside the DECLRMM band, a line feed neither moves
    /// nor scrolls.
    fn sized_wrap(&mut self, stalled: &mut bool) -> bool {
        let row = self.screen.cursor.row;
        let progresses = if row == self.margins.bottom {
            !self.cursor_outside_left_right()
        } else {
            row + 1 < self.screen.rows
        };
        if progresses {
            self.line_feed();
            self.screen.set_soft_wrap(self.screen.cursor.row, true);
        } else if std::mem::replace(stalled, true) {
            return false;
        }
        self.screen.cursor.col = self.print_left_edge_for_wrap();
        self.screen.cursor.pending_wrap = false;
        true
    }

    fn fit_sized_rows(&mut self, scale: u16) {
        let row = self.screen.cursor.row;
        if row <= self.margins.bottom {
            let available = self.margins.bottom - row + 1;
            if scale > available {
                let n = scale - available;
                self.scroll_region_up(n);
                self.screen.cursor.row = row - n;
            }
        } else {
            let available = self.screen.rows - row;
            if scale > available {
                self.screen.cursor.row = row - (scale - available);
            }
        }
    }

    fn free_sized_span(&self, from: u16, block_w: u16, right: u16) -> Option<u16> {
        let row = self.screen.cursor.row;
        let mut col = from;
        while u32::from(col) + u32::from(block_w) <= u32::from(right) + 1 {
            match (col..col + block_w)
                .rev()
                .find(|&c| self.holds_lower_block_row(row, c))
            {
                None => return Some(col),
                Some(blocked) => col = blocked + 1,
            }
        }
        None
    }

    fn holds_lower_block_row(&self, row: u16, col: u16) -> bool {
        self.screen.has_sized_cells()
            && self
                .screen
                .cell(row, col)
                .is_some_and(|c| c.sizing.is_some())
            && self.sized_block_at(row, col).is_some_and(|b| b.top < row)
    }

    /// Erases every sized character and wide pair the block overlaps,
    /// whatever run it came from, then writes the block at the cursor.
    fn stamp_sized_block(&mut self, g: Grapheme, scale: u16, block_w: u16, handle: SizingHandle) {
        let top = self.screen.cursor.row;
        let left = self.screen.cursor.col;
        let bottom = top + scale;
        let right = left + block_w;
        let natural_w = u16::from(self.screen.grapheme_width(g).max(1));
        for r in top..bottom {
            if self.screen.has_sized_cells() {
                let occ = self.screen.occupancy[self.screen.phys_row(r)];
                for c in left..right.min(occ) {
                    if self.screen.cells[self.screen.idx(r, c)].sizing.is_some() {
                        self.clear_sized_run_at(r, c);
                    }
                }
            }
            self.erase_pair_across(r, usize::from(left));
            self.erase_pair_across(r, usize::from(right));
        }
        for r in top..bottom {
            let phys = self.screen.phys_row(r);
            let base = phys * usize::from(self.screen.cols);
            if usize::from(left) > usize::from(self.screen.occupancy[phys]) {
                self.fill_leading_gap(phys, usize::from(left));
            }
            for c in left..right {
                let grapheme = if r == top && c == left {
                    g
                } else if r == top && c == left + 1 && natural_w == 2 {
                    Grapheme::Spacer
                } else {
                    Grapheme::SizedSpacer
                };
                self.screen.write_cell(
                    base + usize::from(c),
                    Cell {
                        grapheme,
                        style: self.pen_style,
                        link: self.current_link,
                        sizing: Some(handle),
                    },
                );
            }
            self.screen.occ_bump_phys(phys, right);
            self.screen.damage.mark(usize::from(r));
        }
        self.last_printed = Some(g);
        self.zwj_pending = false;
        let edge = self.print_right_edge_for_cursor();
        if right > edge {
            self.screen.cursor.col = edge;
            self.screen.cursor.pending_wrap = true;
        } else {
            self.screen.cursor.col = right;
        }
    }

    /// Scrollback rows read as unsized: exposing scrolled-out sizings is
    /// a follow-up (docs/explanation/data-model/grid-and-cells.md).
    #[must_use]
    pub fn cell_sizing_at_viewport(&self, viewport: u32, row: u16, col: u16) -> Option<Sizing> {
        if col >= self.screen.cols {
            return None;
        }
        match self.screen.resolve_viewport_row(viewport, row)? {
            ViewportRow::Live(live_row) => self.screen.cell_sizing(live_row, col).copied(),
            ViewportRow::Scrollback(_) => None,
        }
    }

    /// The wire layer uses this to decide whether a rehydrate burst must
    /// ship the sizing band.
    #[must_use]
    pub fn sized_cell_count(&self) -> usize {
        self.screen
            .cells
            .iter()
            .filter(|c| c.sizing.is_some())
            .count()
    }
}

#[cfg(test)]
mod tests;

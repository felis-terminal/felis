//! Text-sizing (OSC 66) state: the sized-run registry, per-cell
//! handle stamping, and the spanned-block bookkeeping the OSC 66
//! dispatcher and the renderer read.

use super::{Cell, Grapheme, Grid, Sizing, SizingHandle, ViewportRow};

/// An OSC 66 character's footprint: `rows × cols` cells from its
/// primary at `(top, left)`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SizedBlock {
    top: u16,
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
            let (rows, block_w) = self.screen.sizing_block_extent(sizing, primary.grapheme);
            // A `w` narrower than the glyph still prints its `Spacer`.
            let natural_w = u16::from(self.screen.grapheme_width(primary.grapheme).max(1));
            let cols = block_w.max(natural_w);
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
        let last_row = block.top.saturating_add(block.rows).min(self.screen.rows);
        let last_col = block.left.saturating_add(block.cols).min(self.screen.cols);
        for r in block.top..last_row {
            for c in block.left..last_col {
                let idx = self.screen.idx(r, c);
                if self.screen.cells[idx].sizing == Some(block.handle) {
                    self.screen.cells[idx] = Cell::default();
                }
            }
            self.screen.damage.mark(usize::from(r));
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

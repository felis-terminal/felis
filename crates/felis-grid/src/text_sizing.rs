//! Text-sizing (OSC 66) state: the sized-run registry, per-cell
//! handle stamping, and the spanned-block bookkeeping the OSC 66
//! dispatcher and the renderer read.

use super::{Cell, Grid, Sizing, SizingHandle, ViewportRow};

impl Grid {
    /// Stops at handle boundaries, so two adjacent runs with different
    /// handles do not merge.
    fn find_sized_primary(&self, r: u16, c: u16, handle: SizingHandle) -> (u16, u16) {
        let mut pr = r;
        while pr > 0 && self.screen.cells[self.screen.idx(pr - 1, c)].sizing == Some(handle) {
            pr -= 1;
        }
        let mut pc = c;
        while pc > 0 && self.screen.cells[self.screen.idx(pr, pc - 1)].sizing == Some(handle) {
            pc -= 1;
        }
        (pr, pc)
    }

    /// Kitty spec: any write into a multi-cell character erases the
    /// whole character (the caller does the subsequent write).
    fn clear_sized_run_at(&mut self, r: u16, c: u16) {
        let Some(handle) = self.screen.cells[self.screen.idx(r, c)].sizing else {
            return;
        };
        let Some(sizing) = self
            .screen
            .sizing_table
            .get(handle.get() as usize - 1)
            .copied()
        else {
            // Orphan handle past the registry: clear just this cell so
            // its state converges instead of panicking on a fuzz-found
            // mismatch.
            let idx = self.screen.idx(r, c);
            self.screen.cells[idx] = Cell::default();
            self.screen.damage.mark(usize::from(r));
            return;
        };
        let (pr, pc) = self.find_sized_primary(r, c, handle);
        let primary_idx = self.screen.idx(pr, pc);
        let (scale, block_w) = self
            .screen
            .sizing_block_extent(sizing, self.screen.cells[primary_idx].grapheme);
        for dr in 0..scale {
            let rr = pr.saturating_add(dr);
            if rr >= self.screen.rows {
                continue;
            }
            for dc in 0..block_w {
                let cc = pc.saturating_add(dc);
                if cc >= self.screen.cols {
                    continue;
                }
                // A foreign cell inside the block's coordinates already
                // cleared its own sizing on its own write.
                let idx = self.screen.idx(rr, cc);
                if self.screen.cells[idx].sizing == Some(handle) {
                    self.screen.cells[idx] = Cell::default();
                }
            }
            self.screen.damage.mark(usize::from(rr));
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

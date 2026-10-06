//! The screen buffer: cell ring and indexed tables.
//! Shared by [`Grid`](crate::Grid) and the client shadow buffer.
//! Fields belong here when mirrored over IPC via `GridMsg`
//! (`docs/explanation/architecture/overview.md`).

use std::num::{NonZeroU16, NonZeroU32};

use crate::cluster_table::ClusterTable;
use crate::damage::Damage;
use crate::link_table::LinkTable;
use crate::style_table::{StyleId, StyleTable};
use crate::{
    AltScreenRows, Attributes, BLANK_CELL, Cell, ClusterText, Cursor, CursorStyle, Grapheme,
    HyperlinkEntry, ScrollDirection, ScrollbackView, Sizing, SizingHandle, ViewportRow,
    ViewportRowView, hist_phys_of, ring_cells,
};

#[derive(Clone, PartialEq, Eq)]
pub struct ScreenBuffer {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) cells: Vec<Cell>,
    /// Physical index of logical viewport row 0: logical row `r`
    /// resolves to physical `(base + r) % phys_cap` ([`Self::phys_row`]).
    /// A full-screen scroll advances `base` instead of moving rows; a
    /// sub-region scroll (DECSTBM / IL / DL) rotates the scroll band
    /// below (docs/explanation/data-model/scrollback.md).
    pub(crate) base: usize,
    /// Interior scroll band: while `band_len != 0`, rows
    /// `[band_top, band_top + band_len)` resolve with an extra rotation
    /// by `band_rot` ([`Self::phys_row_at`];
    /// `docs/explanation/data-model/scrollback.md` "Interior scroll band").
    /// Paths reasoning about raw ring order must call [`Self::materialize_band`].
    pub(crate) band_top: usize,
    /// See `band_top`; `0` means no active band.
    pub(crate) band_len: usize,
    /// See `band_top`.
    pub(crate) band_rot: usize,
    /// Per-physical-row soft-wrap bit, indexed like `cells` so the band
    /// rotation moves it with the row; only blanked rows need an
    /// explicit clear. Partial overwrites keep the bit: it records how
    /// the content arrived, not what the row currently holds
    /// (docs/explanation/data-model/scrollback.md "Soft wrap").
    pub(crate) soft_wrap: Box<[bool]>,
    /// Per-physical-row occupancy watermark: columns `[0..occupancy[phys])`
    /// are live; past it bytes are undefined. Readers clip at it, writers
    /// blank gaps before writing, and BCE pins it at `cols`
    /// (`docs/explanation/data-model/grid-and-cells.md` "Occupancy watermark").
    pub(crate) occupancy: Box<[u16]>,
    pub(crate) cursor: Cursor,
    /// At screen scope, not on `Cursor`: xterm saves and restores the
    /// position via DECSC / DECRC while the cursor style persists across
    /// those. DECSTR resets it to `Block`.
    pub(crate) cursor_style: CursorStyle,
    /// Observed only by DECRQSS: programs probing `DCS $ q SP q ST`
    /// expect to read back the Ps they wrote.
    pub(crate) cursor_blink: bool,
    pub(crate) style_table: StyleTable,
    pub(crate) damage: Damage,
    /// Retained scrollback capacity in rows; `0` on the alternate screen.
    pub(crate) cap: usize,
    /// Ring modulus, in `[rows, rows + cap]`; the arrays grow lazily as
    /// history fills.
    pub(crate) phys_cap: usize,
    /// History occupies `[base - history_len, base)` (mod `phys_cap`);
    /// the viewport `[base, base + rows)`.
    pub(crate) history_len: usize,
    /// `Some` while the alternate screen is active.
    pub(crate) saved_primary: Option<SavedScreen>,
    /// `?1047l` and `?1049l` drop this slot on leave (xterm: those modes
    /// "clear the alt screen before switching"); `?47l` keeps it so a
    /// re-entry via `?47h` finds the prior scribble intact.
    pub(crate) saved_alternate: Option<SavedScreen>,
    /// Cells reference entries by 1-based index. Grows monotonically:
    /// entries are not freed on scroll-out, since reattaching clients
    /// receive the table on rehydrate and need stable indices;
    /// [`LinkTable`] bounds the count and the entries.
    pub(crate) link_table: LinkTable,
    /// Same monotonic-growth contract as [`Self::link_table`], bounded by
    /// [`ClusterText::CAP`] and [`CLUSTER_TABLE_CAP`]
    /// (`docs/explanation/data-model/grid-and-cells.md` "Cluster
    /// interning").
    pub(crate) cluster_table: ClusterTable,
    /// 1-based; `None` on a cell means the pen defaults. The cells of one
    /// OSC 66 run share a single entry (`protocols/kitty-text-sizing.md`).
    pub(crate) sizing_table: Vec<Sizing>,
    /// Conservative hint for the [`sink::print_str`](crate::sink) bulk
    /// fast path. May read true after the last sized cell scrolled away,
    /// which only de-opts the fast path.
    pub(crate) has_sized_cells: bool,
    /// Bumped on every geometry change, and carried by every
    /// [`PtyEffect::Scrolled`](crate::PtyEffect): a consumer that ships
    /// a directive one generation late would shift rows the resize has
    /// already moved, and no coordinate check can tell the two apart.
    pub(crate) geometry_gen: u64,
    /// Counts the scroll effects this grid has produced. A consumer
    /// that read the rows at sequence `n` already holds the shift of
    /// every effect at or below it.
    pub(crate) scroll_seq: u64,
}

/// Scrollback is shared across both buffers per xterm. The
/// `sizing_table` registry is not saved: it persists across toggles so
/// handles in the saved cells stay valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedScreen {
    pub(crate) cells: Vec<Cell>,
    /// Need not be the live grid's: a resize under the alt screen leaves
    /// the saved primary at its snapshot geometry until
    /// `Grid::leave_alternate` re-wraps it, so reads of this buffer
    /// resolve rows through these.
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) base: usize,
    /// History lives inside the primary ring, so a scrollback read under
    /// the alt screen routes here.
    pub(crate) phys_cap: usize,
    pub(crate) history_len: usize,
    pub(crate) cap: usize,
    pub(crate) soft_wrap: Box<[bool]>,
    pub(crate) occupancy: Box<[u16]>,
    pub(crate) cursor: Cursor,
    pub(crate) pen: Attributes,
}

impl ScreenBuffer {
    #[must_use]
    pub fn with_scrollback(rows: u16, cols: u16, scrollback_rows: usize) -> Self {
        let cells = ring_cells(
            usize::from(rows) * usize::from(cols),
            usize::from(rows) + scrollback_rows,
            usize::from(cols),
        );
        let mut damage = Damage::default();
        damage.resize(rows.into());
        damage.mark_all();
        Self {
            rows,
            cols,
            cells,
            base: 0,
            band_top: 0,
            band_len: 0,
            band_rot: 0,
            soft_wrap: vec![false; usize::from(rows)].into_boxed_slice(),
            occupancy: vec![0u16; usize::from(rows)].into_boxed_slice(),
            cursor: Cursor::new(),
            cursor_style: CursorStyle::default(),
            cursor_blink: true,
            style_table: StyleTable::new(),
            damage,
            cap: scrollback_rows,
            phys_cap: usize::from(rows),
            history_len: 0,
            saved_primary: None,
            saved_alternate: None,
            link_table: LinkTable::default(),
            cluster_table: ClusterTable::default(),
            sizing_table: Vec::new(),
            has_sized_cells: false,
            geometry_gen: 0,
            scroll_seq: 0,
        }
    }
}

impl ScreenBuffer {
    /// Shadow-side `GridMsg::Scrolled`, `false` and no cell touched when
    /// the region or the count does not fit this grid. Unlike
    /// `scroll_region_up` / `scroll_region_down` it reaches no
    /// scrollback and queues no [`PtyEffect`](crate::PtyEffect), and a
    /// follow-up `RowDelta` carries any pen-colored blank.
    #[must_use]
    pub fn apply_scroll_directive(
        &mut self,
        region_top: u16,
        region_bottom: u16,
        n_rows: u16,
        direction: ScrollDirection,
    ) -> bool {
        if region_top > region_bottom || region_bottom >= self.rows {
            return false;
        }
        let region_top_u = usize::from(region_top);
        let region_bottom_u = usize::from(region_bottom);
        let region_height = region_bottom_u - region_top_u + 1;
        let n = usize::from(n_rows);
        if n == 0 || n > region_height {
            return false;
        }
        self.rotate_region(
            region_top_u,
            region_bottom_u,
            n,
            direction,
            &Cell::default(),
        );
        for r in region_top_u..=region_bottom_u {
            self.damage.mark(r);
        }
        true
    }

    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let phys = self.phys_row(row);
        if usize::from(col) >= usize::from(self.occupancy[phys]) {
            return Some(&BLANK_CELL);
        }
        Some(&self.cells[self.idx(row, col)])
    }

    /// The first and last column of the character drawn at `(row, col)`:
    /// a wide character's lead and its `Spacer`, reached from either half.
    #[must_use]
    pub fn char_span(&self, row: u16, col: u16) -> (u16, u16) {
        let is_spacer = |c: u16| {
            self.cell(row, c)
                .is_some_and(|cell| matches!(cell.grapheme, Grapheme::Spacer))
        };
        if col > 0 && is_spacer(col) {
            (col - 1, col)
        } else if is_spacer(col.saturating_add(1)) {
            (col, col + 1)
        } else {
            (col, col)
        }
    }

    /// `viewport` rows of retained scrollback lift the view: visible row
    /// `r < viewport` sources from scrollback index
    /// `scrollback().len() - viewport + r`, the rest from live row
    /// `r - viewport`. Columns past a scrollback row's stored length read
    /// blank: the trim/pad resize path leaves shorter rows in the ring.
    #[must_use]
    pub fn cell_at_viewport(&self, viewport: u32, row: u16, col: u16) -> Option<Cell> {
        if col >= self.cols {
            return None;
        }
        match self.resolve_viewport_row(viewport, row)? {
            ViewportRow::Live(live_row) => self.cell(live_row, col).copied(),
            ViewportRow::Scrollback(sb_idx) => {
                let sb_row = self.scrollback().row(sb_idx)?;
                Some(sb_row.get(usize::from(col)).copied().unwrap_or_default())
            }
        }
    }

    /// Largest legal `lines_from_bottom` ≤ `requested`; `0` on the
    /// alternate screen (no scrollback), which is how the daemon
    /// short-circuits a stale `Viewport { N>0 }` from a client that has
    /// not yet seen the `?1049h` flip.
    #[must_use]
    pub fn clamp_viewport(&self, requested: u32) -> u32 {
        if self.on_alternate_screen() {
            return 0;
        }
        let cap = u32::try_from(self.scrollback().len()).unwrap_or(u32::MAX);
        requested.min(cap)
    }

    #[must_use]
    pub const fn cols(&self) -> u16 {
        self.cols
    }

    /// See [`Self::geometry_gen`].
    #[must_use]
    pub const fn geometry_gen(&self) -> u64 {
        self.geometry_gen
    }

    /// See [`Self::scroll_seq`].
    #[must_use]
    pub const fn scroll_seq(&self) -> u64 {
        self.scroll_seq
    }

    pub(crate) const fn next_scroll_seq(&mut self) -> u64 {
        self.scroll_seq += 1;
        self.scroll_seq
    }

    #[must_use]
    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Forwarded to the client, whose frame timer animates the blink; the
    /// daemon never sends blink frames.
    #[must_use]
    pub const fn cursor_blink(&self) -> bool {
        self.cursor_blink
    }

    #[must_use]
    pub const fn cursor_style(&self) -> CursorStyle {
        self.cursor_style
    }

    /// Drop sized runs whose block does not fit at the primary
    /// after a resize; the trim leaves such primaries in place.
    pub(crate) fn discard_unfit_sized_runs(&mut self) {
        let mut cells = std::mem::take(&mut self.cells);
        let dropped = self
            .discard_unfit_sized_runs_in(&mut cells, self.rows, self.cols, |r, c| self.idx(r, c));
        self.cells = cells;
        for row in dropped {
            self.damage.mark(usize::from(row));
        }
    }

    /// Returns the primaries' rows. Shared by the live screen and the
    /// saved alternate one, whose rows `idx` lays out differently.
    fn discard_unfit_sized_runs_in(
        &self,
        cells: &mut [Cell],
        rows: u16,
        cols: u16,
        idx: impl Fn(u16, u16) -> usize,
    ) -> Vec<u16> {
        let mut to_drop: Vec<(u16, u16, u16, u16, u16)> = Vec::new();
        for r in 0..rows {
            for c in 0..cols {
                let i = idx(r, c);
                let Some(handle) = cells[i].sizing else {
                    continue;
                };
                // Spacers follow their primary; the drop sweep below
                // cleans them.
                if matches!(
                    cells[i].grapheme,
                    Grapheme::SizedSpacer | Grapheme::Spacer | Grapheme::Empty
                ) {
                    continue;
                }
                let Some(sizing) = self.sizing_table.get(handle.get() as usize - 1) else {
                    continue;
                };
                let (scale, block_w) = self.sizing_block_extent(*sizing, cells[i].grapheme);
                let natural_w = u16::from(self.grapheme_width(cells[i].grapheme).max(1));
                if r.saturating_add(scale) > rows || c.saturating_add(block_w) > cols {
                    to_drop.push((r, c, scale, block_w, natural_w));
                }
            }
        }
        let mut dropped = Vec::with_capacity(to_drop.len());
        for (pr, pc, scale, block_w, natural_w) in to_drop {
            for dr in 0..scale {
                for dc in 0..block_w {
                    let r = pr.saturating_add(dr);
                    let c = pc.saturating_add(dc);
                    if r >= rows || c >= cols {
                        continue;
                    }
                    let i = idx(r, c);
                    cells[i].sizing = None;
                    // The primary keeps its grapheme (only the run's
                    // sizing is dropped), and wide-glyph `Spacer`
                    // partners stay so the natural-width pair survives.
                    if matches!(cells[i].grapheme, Grapheme::SizedSpacer) {
                        cells[i].grapheme = Grapheme::Empty;
                    }
                }
            }
            // The pair cannot outlive its block: a glyph wider than its
            // block has no `Spacer`, and one the new edge cut has no room.
            if natural_w > block_w || pc.saturating_add(natural_w) > cols {
                cells[idx(pr, pc)] = Cell::default();
            }
            dropped.push(pr);
        }
        dropped
    }

    /// `Spacer` is `0`: the right half is already accounted for by its
    /// owning wide cell. An unresolvable cluster handle falls back to
    /// width 1 so the cell still advances the cursor.
    pub(crate) fn grapheme_width(&self, g: Grapheme) -> u8 {
        use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
        match g {
            Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => 0,
            Grapheme::Ascii(_) => 1,
            // A deferred `0` covers the fold set, whose standalone width
            // unicode-width still decides.
            Grapheme::Char(c) => match crate::sink::table_width(crate::sink::bmp_widths(), c) {
                0 => c.width().unwrap_or(0) as u8,
                w => w as u8,
            },
            Grapheme::Cluster(id) => self.cluster_str(id).map_or(1, |s| s.width().min(2) as u8),
        }
    }

    /// `0` = oldest retained, `history_len - 1` = youngest.
    pub(crate) fn hist_phys(&self, idx_from_oldest: usize) -> usize {
        debug_assert!(idx_from_oldest < self.history_len);
        hist_phys_of(self.base, self.phys_cap, self.history_len, idx_from_oldest)
    }

    /// Oldest → newest, each trimmed to its occupancy. Reads the live
    /// fields rather than [`Self::scrollback`] (which redirects to the
    /// saved primary while the alt screen is up), so a resize of the alt
    /// buffer sees its own empty history.
    pub(crate) fn history_rows_owned(&self) -> Vec<(Vec<Cell>, bool)> {
        let cols = usize::from(self.cols);
        (0..self.history_len)
            .map(|i| {
                let phys = self.hist_phys(i);
                let occ = usize::from(self.occupancy[phys]).min(cols);
                let start = phys * cols;
                (
                    self.cells[start..start + occ].to_vec(),
                    self.soft_wrap[phys],
                )
            })
            .collect()
    }

    pub(crate) fn idx(&self, row: u16, col: u16) -> usize {
        self.phys_row(row) * usize::from(self.cols) + usize::from(col)
    }

    /// Lays `history` (oldest → newest, trimmed) at `[0, history_len)`
    /// and the viewport after it, `base` at `history_len`, `phys_cap`
    /// sized to exactly the occupied window; history past `cap` is
    /// dropped oldest-first.
    pub(crate) fn install_ring(
        &mut self,
        rows: u16,
        cols: u16,
        history: &[(Vec<Cell>, bool)],
        viewport_cells: &[Cell],
        viewport_soft_wrap: &[bool],
    ) {
        let cols_usize = usize::from(cols);
        let rows_usize = usize::from(rows);
        let hist_all = history.len();
        let history_len = hist_all.min(self.cap);
        let hist_drop = hist_all - history_len;
        let phys_cap = history_len + rows_usize;
        let mut cells = ring_cells(phys_cap * cols_usize, rows_usize + self.cap, cols_usize);
        let mut soft_wrap = vec![false; phys_cap].into_boxed_slice();
        let mut occupancy = vec![0u16; phys_cap].into_boxed_slice();
        for (dst, (row, wrap)) in history[hist_drop..].iter().enumerate() {
            let occ = row.len().min(cols_usize);
            cells[dst * cols_usize..dst * cols_usize + occ].copy_from_slice(&row[..occ]);
            soft_wrap[dst] = *wrap;
            occupancy[dst] = u16::try_from(occ).unwrap_or(cols);
        }
        let vp = history_len * cols_usize;
        cells[vp..vp + rows_usize * cols_usize].copy_from_slice(viewport_cells);
        for r in 0..rows_usize {
            soft_wrap[history_len + r] = viewport_soft_wrap[r];
            // Watermark provenance is lost across the rebuild; `cols` is
            // the always-safe upper bound and self-heals on the next
            // recycle.
            occupancy[history_len + r] = cols;
        }
        self.rows = rows;
        self.cols = cols;
        self.cells = cells;
        self.soft_wrap = soft_wrap;
        self.occupancy = occupancy;
        self.base = history_len;
        self.phys_cap = phys_cap;
        self.history_len = history_len;
        // Callers read `viewport` through the band-aware accessors, so
        // the fresh layout carries no rotation.
        self.band_len = 0;
        self.band_rot = 0;
    }

    /// Inclusive `(first, last)` of the logical line `row` belongs to,
    /// clipped to the live screen: a line whose head has scrolled into
    /// scrollback reports the visible tail's first row. An out-of-range
    /// `row` reports itself on both ends.
    #[must_use]
    pub fn logical_line_bounds(&self, row: u16) -> (u16, u16) {
        if row >= self.rows {
            return (row, row);
        }
        let mut start = row;
        while start > 0 && self.row_soft_wrap_continued(start) {
            start -= 1;
        }
        let mut end = row;
        while end + 1 < self.rows && self.row_soft_wrap_continued(end + 1) {
            end += 1;
        }
        (start, end)
    }

    /// Reorder the active band's rows into identity order and deactivate
    /// it (see `band_top` for who must call this first).
    #[inline]
    pub(crate) fn materialize_band(&mut self) {
        // Split so the no-band check inlines into the per-LF scroll
        // path; folding the reorder body in demotes the guard to a real
        // call (−9% on the sparse full-screen scroll floor).
        if self.band_len != 0 {
            self.materialize_band_reorder();
        }
    }

    #[cold]
    pub(crate) fn materialize_band_reorder(&mut self) {
        let (top, len, rot) = (self.band_top, self.band_len, self.band_rot);
        if rot == 0 {
            self.band_len = 0;
            return;
        }
        let cols = usize::from(self.cols);
        // The band's physical slots may wrap the ring, so the permutation
        // is over a slot sequence, not a contiguous range; a temp copy
        // beats cycle-walking for a path this cold.
        let mut tmp_cells = vec![Cell::default(); len * cols];
        let mut tmp_wrap = vec![false; len];
        let mut tmp_occ = vec![0u16; len];
        for i in 0..len {
            let sp = self.phys_row_at(top + i);
            let occ = usize::from(self.occupancy[sp]);
            tmp_cells[i * cols..i * cols + occ]
                .copy_from_slice(&self.cells[sp * cols..sp * cols + occ]);
            tmp_wrap[i] = self.soft_wrap[sp];
            tmp_occ[i] = self.occupancy[sp];
        }
        self.band_len = 0;
        self.band_rot = 0;
        for i in 0..len {
            let dp = self.phys_row_at(top + i);
            let occ = usize::from(tmp_occ[i]);
            self.cells[dp * cols..dp * cols + occ]
                .copy_from_slice(&tmp_cells[i * cols..i * cols + occ]);
            self.soft_wrap[dp] = tmp_wrap[i];
            self.occupancy[dp] = tmp_occ[i];
        }
    }

    /// Raising is always safe (the watermark is an upper bound), so write
    /// sites call this unconditionally.
    #[inline]
    pub(crate) fn occ_bump_idx(&mut self, idx: usize) {
        let cols = usize::from(self.cols);
        let phys = idx / cols;
        let end = u16::try_from(idx % cols + 1).unwrap_or(u16::MAX);
        let o = &mut self.occupancy[phys];
        if *o < end {
            *o = end;
        }
    }

    #[inline]
    pub(crate) fn occ_bump_phys(&mut self, phys: usize, end_col: u16) {
        let end = end_col.min(self.cols);
        let o = &mut self.occupancy[phys];
        if *o < end {
            *o = end;
        }
    }

    #[inline]
    pub(crate) fn occ_bump_row(&mut self, row: u16, end_col: u16) {
        let phys = self.phys_row(row);
        self.occ_bump_phys(phys, end_col);
    }

    #[must_use]
    pub const fn on_alternate_screen(&self) -> bool {
        self.saved_primary.is_some()
    }

    /// The single seam every reader and writer resolves logical rows
    /// through.
    pub(crate) fn phys_row(&self, row: u16) -> usize {
        self.phys_row_at(usize::from(row))
    }

    /// A conditional subtract, not `% rows`: `rows` is a runtime value,
    /// so `%` lowers to a real `idiv` (~4% on the ascii print floor,
    /// since `idx` runs it per row chunk).
    pub(crate) fn phys_row_at(&self, logical: usize) -> usize {
        debug_assert!(logical < usize::from(self.rows));
        // The in-band wrap stays a `%`: the conditional-subtract form
        // bloats this function's many inlined copies (−8% on the sparse
        // full-screen scroll floor), while the `idiv` sits only on
        // in-band resolutions.
        let logical = if logical.wrapping_sub(self.band_top) < self.band_len {
            self.band_top + (logical - self.band_top + self.band_rot) % self.band_len
        } else {
            logical
        };
        let p = self.base + logical;
        if p >= self.phys_cap {
            p - self.phys_cap
        } else {
            p
        }
    }

    pub(crate) fn refresh_has_sized_cells(&mut self) {
        self.has_sized_cells = self.cells.iter().any(|c| c.sizing.is_some());
    }

    /// The reservation covers the whole retention window up front
    /// (`ring_cells`), so this is the term that decides a grid's
    /// address-space cost.
    #[must_use]
    pub const fn reserved_cell_bytes(&self) -> usize {
        self.cells.capacity() * size_of::<Cell>()
    }

    /// Clamps `viewport` itself, so callers pass the raw request. `None`
    /// only for `row` out of range.
    pub(crate) fn resolve_viewport_row(&self, viewport: u32, row: u16) -> Option<ViewportRow> {
        if row >= self.rows {
            return None;
        }
        let viewport = self.clamp_viewport(viewport);
        if viewport == 0 || u32::from(row) >= viewport {
            let live_row = row - u16::try_from(viewport).unwrap_or(self.rows);
            return Some(ViewportRow::Live(live_row));
        }
        let sb_len = self.scrollback().len();
        let sb_idx = sb_len - usize::try_from(viewport).unwrap_or(sb_len) + usize::from(row);
        Some(ViewportRow::Scrollback(sb_idx))
    }

    /// O(1) row rotate: full-screen bumps `base`; sub-regions bump band rotation.
    /// Region changes materialize the existing band.
    /// Caller must push to scrollback before rotation while rows sit at
    /// pre-rotation positions.
    pub(crate) fn rotate_region(
        &mut self,
        top: usize,
        bottom: usize,
        n: usize,
        direction: ScrollDirection,
        blank_with: &Cell,
    ) {
        debug_assert!(top <= bottom);
        debug_assert!(bottom < usize::from(self.rows));
        let region_height = bottom - top + 1;
        debug_assert!(n > 0 && n <= region_height);
        let rows_usize = usize::from(self.rows);
        // With history present the viewport shares the ring with it, so
        // a downward `base` bump would resurrect the youngest history
        // rows. Scroll-into-history up never reaches here
        // (`scroll_full_screen_into_history`), so a full-screen rotate
        // with history takes the band path.
        let full_screen = top == 0 && bottom == rows_usize - 1 && self.history_len == 0;
        if full_screen {
            // Band offsets apply before `base`, so shifting `base` under
            // a rotated band re-targets the band's rows.
            self.materialize_band();
            let phys_cap = self.phys_cap;
            self.base = match direction {
                ScrollDirection::Up => (self.base + n) % phys_cap,
                ScrollDirection::Down => (self.base + phys_cap - n) % phys_cap,
            };
        } else {
            // `band_rot < band_len` is the `phys_row_at` wrap invariant;
            // `n <= region_height` keeps the sums below
            // `2 * region_height`.
            if self.band_len != 0 && (self.band_top != top || self.band_len != region_height) {
                self.materialize_band();
            }
            self.band_top = top;
            self.band_len = region_height;
            let rot = match direction {
                ScrollDirection::Up => self.band_rot + n,
                ScrollDirection::Down => self.band_rot + region_height - n,
            };
            self.band_rot = if rot >= region_height {
                rot - region_height
            } else {
                rot
            };
        }
        // A default blank drops the watermark and leaves unreferenced cell data,
        // eliding the per-line `cols`-cell memset; a BCE blank is live
        // content.
        let cols = usize::from(self.cols);
        let blank_is_default = *blank_with == Cell::default();
        let recycled = match direction {
            ScrollDirection::Up => (bottom + 1 - n)..=bottom,
            ScrollDirection::Down => top..=(top + n - 1),
        };
        for r in recycled {
            let phys = self.phys_row_at(r);
            if blank_is_default {
                self.occupancy[phys] = 0;
            } else {
                let start = phys * cols;
                self.cells[start..start + cols].fill(*blank_with);
                self.occupancy[phys] = u16::try_from(cols).unwrap_or(u16::MAX);
            }
            self.soft_wrap[phys] = false;
        }
    }

    /// The raw physical row, stale tail past the watermark included; the
    /// daemon's encoders take it to skip a per-row clone. Composed-view
    /// callers (browse mode) must use [`Self::viewport_row`], which mixes
    /// scrollback rows in.
    #[must_use]
    pub fn row_cells(&self, row: u16) -> Option<&[Cell]> {
        if row >= self.rows {
            return None;
        }
        let start = self.idx(row, 0);
        let end = start + usize::from(self.cols);
        Some(&self.cells[start..end])
    }

    /// `[0..occupancy)`. Every reader that treats a row as "content then
    /// blanks" (reflow, search, pipe, wire encode) must go through this
    /// rather than [`Self::row_cells`].
    #[must_use]
    pub fn row_content(&self, row: u16) -> Option<&[Cell]> {
        if row >= self.rows {
            return None;
        }
        let phys = self.phys_row(row);
        let start = phys * usize::from(self.cols);
        let occ = usize::from(self.occupancy[phys]).min(usize::from(self.cols));
        Some(&self.cells[start..start + occ])
    }

    #[must_use]
    pub fn row_soft_wrap_continued(&self, row: u16) -> bool {
        if row >= self.rows {
            return false;
        }
        self.soft_wrap[self.phys_row(row)]
    }

    #[must_use]
    pub fn row_view(&self, row: u16) -> ViewportRowView<'_> {
        ViewportRowView {
            cells: self.row_content(row).unwrap_or(&[]),
            soft_wrap_continued: self.row_soft_wrap_continued(row),
            sized_cells: self.row_sized_cells(row),
        }
    }

    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// Scrollback (oldest → newest) then live rows, so a logical line
    /// whose head is in scrollback and whose tail is on screen stitches
    /// across the seam. Rows are occupancy-clipped. `alt` decides whether
    /// the live half joins while the alternate screen holds the display;
    /// the scrollback half is walked either way.
    pub fn rows_with_wrap(&self, alt: AltScreenRows) -> impl Iterator<Item = (&[Cell], bool)> {
        let sb = self.scrollback();
        let sb_len = sb.len();
        let live = if alt == AltScreenRows::Skip && self.on_alternate_screen() {
            0
        } else {
            self.rows
        };
        (0..sb_len)
            .map(move |i| {
                (
                    sb.row(i).unwrap_or(&[]),
                    sb.soft_wrap_continued(i).unwrap_or(false),
                )
            })
            .chain((0..live).map(move |r| {
                (
                    self.row_content(r).unwrap_or(&[]),
                    self.row_soft_wrap_continued(r),
                )
            }))
    }

    #[must_use]
    pub fn scrollback(&self) -> ScrollbackView<'_> {
        // History lives inside the primary ring, so under the alt screen
        // it is read from the saved primary at the snapshot's own width
        // (a resize under the alt screen moves the live dimensions ahead
        // of it until `?1049l` re-wraps).
        self.saved_primary.as_ref().map_or_else(
            || ScrollbackView {
                cells: &self.cells,
                soft_wrap: &self.soft_wrap,
                occupancy: &self.occupancy,
                base: self.base,
                phys_cap: self.phys_cap,
                history_len: self.history_len,
                cols: usize::from(self.cols),
            },
            |saved| ScrollbackView {
                cells: &saved.cells,
                soft_wrap: &saved.soft_wrap,
                occupancy: &saved.occupancy,
                base: saved.base,
                phys_cap: saved.phys_cap,
                history_len: saved.history_len,
                cols: usize::from(saved.cols),
            },
        )
    }

    #[must_use]
    pub const fn scrollback_capacity(&self) -> usize {
        self.cap
    }

    /// The shadow needs to write graphemes like [`Grapheme::SizedSpacer`]
    /// that no VT sequence emits.
    pub fn set_cell(&mut self, row: u16, col: u16, cell: Cell) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let phys = self.phys_row(row);
        let occ = usize::from(self.occupancy[phys]);
        let c = usize::from(col);
        let idx = self.idx(row, col);
        let current = if c >= occ {
            Cell::default()
        } else {
            self.cells[idx]
        };
        if current != cell {
            if c > occ {
                let base = phys * usize::from(self.cols);
                self.cells[base + occ..base + c].fill(Cell::default());
            }
            self.cells[idx] = cell;
            self.occ_bump_idx(idx);
            self.damage.mark(row.into());
        }
    }

    /// The move clears deferred-wrap state. `false` without moving the
    /// cursor when the coordinate is outside this grid; the caller
    /// decides whether that is corruption or its own stale geometry.
    #[must_use]
    pub const fn set_cursor_state(
        &mut self,
        row: u16,
        col: u16,
        visible: bool,
        style: CursorStyle,
        blink: bool,
    ) -> bool {
        if row >= self.rows || col >= self.cols {
            return false;
        }
        self.cursor.row = row;
        self.cursor.col = col;
        self.cursor.visible = visible;
        self.cursor.pending_wrap = false;
        self.cursor_style = style;
        self.cursor_blink = blink;
        true
    }

    /// Public for the wire-replay: the bit rides the row codec and no VT
    /// sequence could recreate it on the client.
    pub fn set_soft_wrap(&mut self, row: u16, continued: bool) {
        if row >= self.rows {
            return;
        }
        self.soft_wrap[self.phys_row(row)] = continued;
    }

    /// `(scale, block_w)`: `block_w` is the run's cell-width override (or
    /// the primary's natural width) times the vertical scale. The single
    /// source for OSC 66 block geometry.
    pub(crate) fn sizing_block_extent(&self, sizing: Sizing, primary: Grapheme) -> (u16, u16) {
        let scale = u16::from(sizing.scale().max(1));
        let natural_w = u16::from(self.grapheme_width(primary).max(1));
        let override_w = if sizing.cell_width() > 0 {
            u16::from(sizing.cell_width())
        } else {
            natural_w
        };
        (scale, override_w.saturating_mul(scale))
    }

    /// An unknown id (a stale handle or junk from a peer) resolves to the
    /// default pen.
    #[must_use]
    pub fn style(&self, id: StyleId) -> &Attributes {
        self.style_table.resolve(id)
    }

    /// Rows written since the last [`Self::clear_damage`].
    #[must_use]
    pub const fn damage(&self) -> &Damage {
        &self.damage
    }

    pub fn clear_damage(&mut self) {
        self.damage.clear();
    }

    #[must_use]
    pub const fn style_table(&self) -> &StyleTable {
        &self.style_table
    }

    /// The daemon's ids never cross the wire; the shadow re-mints its own
    /// here via [`crate::decode_row`].
    pub const fn style_table_mut(&mut self) -> &mut StyleTable {
        &mut self.style_table
    }

    /// One seam resolve per row rather than [`Self::cell_at_viewport`]'s
    /// per column. [`ViewportRowView::cells`] is occupancy-clipped; the
    /// caller pads to [`Self::cols`] with [`Cell::BLANK`]
    /// ([`RowEncode::pad_to`](crate::wire::RowEncode::pad_to)).
    #[must_use]
    pub fn viewport_row(&self, viewport: u32, row: u16) -> Option<ViewportRowView<'_>> {
        match self.resolve_viewport_row(viewport, row)? {
            ViewportRow::Live(live_row) => Some(self.row_view(live_row)),
            ViewportRow::Scrollback(sb_idx) => {
                let sb = self.scrollback();
                Some(ViewportRowView {
                    cells: sb.row(sb_idx).unwrap_or(&[]),
                    soft_wrap_continued: sb.soft_wrap_continued(sb_idx).unwrap_or(false),
                    // Scrollback carries no OSC 66 side-band
                    // (docs/explanation/data-model/grid-and-cells.md).
                    sized_cells: Vec::new(),
                })
            }
        }
    }

    /// `cells.len()` must equal `cols` (other widths stay on the per-cell
    /// path); out-of-range rows are ignored.
    pub fn write_row_cells(&mut self, row: u16, cells: &[Cell]) -> bool {
        if row >= self.rows || cells.len() != usize::from(self.cols) {
            return false;
        }
        let start = self.idx(row, 0);
        // Storage past the watermark is unread, not blank: a scroll
        // directive leaves the rotated-out cells there.
        let full = usize::from(self.occupancy[self.phys_row(row)]) >= cells.len();
        let dst = &mut self.cells[start..start + cells.len()];
        if full && dst == cells {
            return false;
        }
        dst.copy_from_slice(cells);
        self.occ_bump_row(row, self.cols);
        self.damage.mark(row.into());
        true
    }

    /// The daemon's own table is gapless, so this is the bound its
    /// registry drain walks.
    #[must_use]
    pub const fn cluster_count(&self) -> usize {
        self.cluster_table.len()
    }

    /// `None` for an id no `Cluster` message has installed, which the
    /// causal delivery rule leaves unnamed by any admitted row
    /// (`docs/reference/ipc.md` "Registry delivery").
    #[must_use]
    pub fn cluster_str(&self, id: NonZeroU32) -> Option<&str> {
        self.cluster_table.get(id)
    }

    /// Handed out as the table, not a slice: a client's table is sparse
    /// (visible-first rehydrate installs the ids its rows name), and
    /// [`ClusterTable::get`] keeps an uninstalled handle a `None`.
    #[must_use]
    pub const fn cluster_table(&self) -> &ClusterTable {
        &self.cluster_table
    }

    /// Takes a [`ClusterText`] so a peer that streams an over-cap cluster
    /// cannot make this client hold it. `false` when the table refused
    /// the handle.
    pub fn install_cluster(&mut self, id: NonZeroU32, text: ClusterText) -> bool {
        self.cluster_table.install(id, text)
    }

    #[must_use]
    pub fn cell_sizing(&self, row: u16, col: u16) -> Option<&Sizing> {
        let handle = self.cell_sizing_handle(row, col)?;
        self.sizing_by_handle(handle)
    }

    #[must_use]
    pub fn cell_sizing_handle(&self, row: u16, col: u16) -> Option<SizingHandle> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells[self.idx(row, col)].sizing
    }

    /// `None` at the `u16::MAX` cap, which keeps handles
    /// `NonZeroU16`-shaped on the wire.
    pub fn install_sizing(&mut self, sizing: Sizing) -> Option<SizingHandle> {
        let next = u16::try_from(self.sizing_table.len() + 1).ok()?;
        let handle = SizingHandle::new(next)?;
        self.sizing_table.push(sizing);
        Some(handle)
    }

    #[must_use]
    pub fn row_sized_cells(&self, row: u16) -> Vec<(u16, Sizing)> {
        if !self.has_sized_cells || row >= self.rows {
            return Vec::new();
        }
        let mut out: Vec<(u16, Sizing)> = Vec::new();
        for c in 0..self.cols {
            if let Some(handle) = self.cells[self.idx(row, c)].sizing
                && let Some(sizing) = self.sizing_by_handle(handle).copied()
            {
                out.push((c, sizing));
            }
        }
        out
    }

    /// Marks damage only when the effective sizing changes, so an idle
    /// dispatcher loop does not march damage every frame.
    pub fn set_cell_sizing(&mut self, row: u16, col: u16, handle: Option<SizingHandle>) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let idx = self.idx(row, col);
        let prior = self.cells[idx].sizing;
        match (prior, handle) {
            (None, None) => {}
            (Some(p), Some(h)) if p == h => {}
            (_, Some(h)) => {
                self.cells[idx].sizing = Some(h);
                self.has_sized_cells = true;
                self.occ_bump_idx(idx);
                self.damage.mark(usize::from(row));
            }
            (Some(_), None) => {
                self.cells[idx].sizing = None;
                self.damage.mark(usize::from(row));
            }
        }
    }

    #[must_use]
    pub fn sizing_by_handle(&self, handle: SizingHandle) -> Option<&Sizing> {
        self.sizing_table.get(handle.get() as usize - 1)
    }

    /// The wire layer walks `0..len` to ship the table on rehydrate.
    #[must_use]
    pub const fn sizing_count(&self) -> usize {
        self.sizing_table.len()
    }
}

impl ScreenBuffer {
    /// `None` for an id no `Hyperlink` message has installed, which the
    /// causal delivery rule leaves unnamed by any admitted row
    /// (`docs/reference/ipc.md` "Registry delivery").
    #[must_use]
    pub fn hyperlink(&self, id: NonZeroU16) -> Option<&HyperlinkEntry> {
        self.link_table.get(id)
    }

    /// The daemon's own table is gapless, so this is the bound its
    /// registry drain walks.
    #[must_use]
    pub const fn hyperlink_count(&self) -> usize {
        self.link_table.len()
    }

    /// Handed out as the table for the reason [`Self::cluster_table`]
    /// gives.
    #[must_use]
    pub const fn hyperlink_table(&self) -> &LinkTable {
        &self.link_table
    }

    /// Ids arrive in reference order rather than densely
    /// (`docs/reference/ipc.md` "Registry delivery"), so an id past the
    /// end spans the table and leaves the ids below it absent. Only ids
    /// no row names may stay absent. A repeat overwrites.
    pub fn install_hyperlink(&mut self, id: NonZeroU16, entry: HyperlinkEntry) -> bool {
        self.link_table.install(id, entry)
    }
}

impl ScreenBuffer {
    /// Trim/pad, not reflow: the alternate screen's and the client
    /// shadow's path. The primary re-wraps instead
    /// ([`Grid::reflow`](crate::Grid::reflow)), which needs the prompt
    /// marks the parser owns. Callers that own margins and tab stops
    /// re-track those afterwards.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        let old_rows = self.rows;
        let old_cols = self.cols;
        let history = self.history_rows_owned();
        let ocols = usize::from(old_cols);
        let mut vp_logical = vec![Cell::default(); usize::from(old_rows) * ocols];
        let mut occ_logical = vec![0u16; usize::from(old_rows)];
        let mut wrap_logical = vec![false; usize::from(old_rows)];
        for r in 0..usize::from(old_rows) {
            let phys = self.phys_row_at(r);
            vp_logical[r * ocols..r * ocols + ocols]
                .copy_from_slice(&self.cells[phys * ocols..phys * ocols + ocols]);
            occ_logical[r] = self.occupancy[phys];
            wrap_logical[r] = self.soft_wrap[phys];
        }
        let new_viewport =
            crate::trim_cells(&vp_logical, old_rows, old_cols, &occ_logical, rows, cols);
        let new_vp_wrap = crate::trim_soft_wrap(&wrap_logical, rows);
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.pending_wrap = false;
        self.install_ring(rows, cols, &history, &new_viewport, &new_vp_wrap);
        self.retrack_geometry(rows, cols);
    }

    /// Must run after the new ring is installed:
    /// `discard_unfit_sized_runs` and `refresh_has_sized_cells` read the
    /// post-resize cells.
    pub(crate) fn retrack_geometry(&mut self, rows: u16, cols: u16) {
        self.geometry_gen = self.geometry_gen.wrapping_add(1);
        // `saved_primary` is left alone: trim/pad is the wrong operator
        // for the primary; `leave_alternate` re-wraps it.
        if let Some(mut saved) = self.saved_alternate.take() {
            crate::resize_saved_screen(&mut saved, rows, cols);
            let saved_cols = usize::from(saved.cols);
            self.discard_unfit_sized_runs_in(&mut saved.cells, saved.rows, saved.cols, |r, c| {
                usize::from(r) * saved_cols + usize::from(c)
            });
            self.saved_alternate = Some(saved);
        }
        self.damage.resize(rows.into());
        self.damage.mark_all();
        // REQ-407 / `protocols/kitty-text-sizing.md`: a run whose block does
        // not fit at its primary is discarded whole rather than reflowed;
        // handles remain valid for surviving cells.
        self.discard_unfit_sized_runs();
        self.refresh_has_sized_cells();
    }
}

impl ScreenBuffer {
    /// Returns scanned cells and whether the table compacted.
    /// When compacted, holders of handles outside cells must re-establish them.
    /// Unlike cluster/link tables, style tables can grow unboundedly
    /// (`docs/explanation/data-model/grid-and-cells.md` "Style interning").
    pub(crate) fn gc_styles(&mut self) -> (usize, bool) {
        let mut compacted = false;
        let mut marks = self.style_table.mark_buffer();
        let mut scanned = self.cells.len();
        for c in &self.cells {
            StyleTable::mark(&mut marks, c.style);
        }
        for saved in [self.saved_primary.as_ref(), self.saved_alternate.as_ref()]
            .into_iter()
            .flatten()
        {
            scanned += saved.cells.len();
            for c in &saved.cells {
                StyleTable::mark(&mut marks, c.style);
            }
        }
        if let Some(remap) = self.style_table.compact(&marks) {
            let apply = |id: &mut StyleId| *id = remap[id.get() as usize];
            for c in &mut self.cells {
                apply(&mut c.style);
            }
            for saved in [self.saved_primary.as_mut(), self.saved_alternate.as_mut()]
                .into_iter()
                .flatten()
            {
                for c in &mut saved.cells {
                    apply(&mut c.style);
                }
            }
            compacted = true;
        }
        (scanned, compacted)
    }

    /// Monotonically grows toward the `u16` cap, distinguishing separate runs.
    /// Not deduped by equal sizing so adjacent runs remain distinct.
    /// Returns old -> new remap for external handle holders; `None` means reclaimed.
    pub(crate) fn gc_sizings(&mut self) -> Vec<Option<SizingHandle>> {
        let mut used: Vec<bool> = vec![false; self.sizing_table.len()];
        let mark = |handle: SizingHandle, used: &mut Vec<bool>| {
            if let Some(slot) = used.get_mut(handle.get() as usize - 1) {
                *slot = true;
            }
        };
        for c in &self.cells {
            if let Some(handle) = c.sizing {
                mark(handle, &mut used);
            }
        }
        for saved in [self.saved_primary.as_ref(), self.saved_alternate.as_ref()]
            .into_iter()
            .flatten()
        {
            for c in &saved.cells {
                if let Some(handle) = c.sizing {
                    mark(handle, &mut used);
                }
            }
        }
        let mut remap: Vec<Option<SizingHandle>> = vec![None; self.sizing_table.len()];
        let mut kept: Vec<Sizing> = Vec::new();
        for (i, sizing) in self.sizing_table.iter().enumerate() {
            if !used.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some(new) = u16::try_from(kept.len() + 1)
                .ok()
                .and_then(SizingHandle::new)
            else {
                continue;
            };
            kept.push(*sizing);
            remap[i] = Some(new);
        }
        self.sizing_table = kept;
        // A handle past the pre-compaction table bounds is an orphan;
        // the sweep clears it.
        let translate = |handle: SizingHandle| -> Option<SizingHandle> {
            remap.get(handle.get() as usize - 1).copied().flatten()
        };
        for c in &mut self.cells {
            if let Some(old) = c.sizing {
                c.sizing = translate(old);
            }
        }
        for saved in [self.saved_primary.as_mut(), self.saved_alternate.as_mut()]
            .into_iter()
            .flatten()
        {
            for c in &mut saved.cells {
                if let Some(old) = c.sizing {
                    c.sizing = translate(old);
                }
            }
        }
        remap
    }
}

impl ScreenBuffer {
    #[must_use]
    pub const fn style_table_len(&self) -> usize {
        self.style_table.len()
    }
}

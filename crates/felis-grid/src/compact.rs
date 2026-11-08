//! Moving ring history into the cold tier, and the live-row walks every
//! raw scan of a ring takes so it never touches a vacant row's pages
//! (docs/explanation/data-model/scrollback.md "Structure").

use std::ops::Range;

use crate::Cell;
use crate::cold::ColdStore;
use crate::paged_cells::{self, PagedCells};

/// Physical rows `[start, start + len)` in ring order, as at most two
/// ascending ranges.
pub(crate) const fn ring_segments(start: usize, len: usize, phys_cap: usize) -> [Range<usize>; 2] {
    let end = start + len;
    if end <= phys_cap {
        [start..end, 0..0]
    } else {
        [start..phys_cap, 0..end - phys_cap]
    }
}

/// The ring window `[base - history_len, base + rows)`: every physical
/// row a reader can reach.
pub(crate) const fn live_rows(
    base: usize,
    phys_cap: usize,
    history_len: usize,
    rows: usize,
) -> [Range<usize>; 2] {
    let oldest = (base + phys_cap - history_len) % phys_cap;
    ring_segments(oldest, history_len + rows, phys_cap)
}

/// The physical rows outside the ring window, whose pages compaction
/// discards.
const fn vacant_rows(
    base: usize,
    phys_cap: usize,
    history_len: usize,
    rows: usize,
) -> [Range<usize>; 2] {
    ring_segments(
        (base + rows) % phys_cap,
        phys_cap - history_len - rows,
        phys_cap,
    )
}

/// A ring's fields, borrowed from the live screen or a saved primary.
pub(crate) struct RingRef<'a> {
    pub(crate) cells: &'a [Cell],
    pub(crate) soft_wrap: &'a [bool],
    pub(crate) occupancy: &'a [u16],
    pub(crate) base: usize,
    pub(crate) phys_cap: usize,
    pub(crate) history_len: usize,
    pub(crate) rows: usize,
    pub(crate) cols: usize,
    pub(crate) cold: &'a ColdStore,
}

impl RingRef<'_> {
    pub(crate) const fn live_rows(&self) -> [Range<usize>; 2] {
        live_rows(self.base, self.phys_cap, self.history_len, self.rows)
    }

    /// A copy of the live rows at the same layout; vacant rows read as
    /// default cells and are never touched, so a clone of a compacted
    /// ring faults in only what the ring keeps.
    pub(crate) fn clone_cells(&self, src: &PagedCells) -> PagedCells {
        let mut copy = PagedCells::with_reserve(src.len(), src.capacity());
        for rows in self.live_rows() {
            let cells = rows.start * self.cols..rows.end * self.cols;
            copy[cells.clone()].copy_from_slice(&src[cells]);
        }
        copy
    }
}

#[cfg(any(test, feature = "state-dump"))]
impl<'a> RingRef<'a> {
    /// Cold history, ring history, then the viewport's physical rows in
    /// order from `base` (a scroll band resolves on top of that order),
    /// each clipped at its occupancy: the ring as it would read laid out
    /// densely with no cold tier.
    pub(crate) fn canonical_rows(&self) -> impl Iterator<Item = (&'a [Cell], bool)> + '_ {
        let ring = (self.phys_cap - self.history_len..self.phys_cap + self.rows).map(|offset| {
            let phys = (self.base + offset) % self.phys_cap;
            let start = phys * self.cols;
            let occupied = usize::from(self.occupancy[phys]).min(self.cols);
            (&self.cells[start..start + occupied], self.soft_wrap[phys])
        });
        self.cold.rows().chain(ring)
    }
}

/// A ring's fields, mutably, for compaction and the scroll path.
pub(crate) struct RingMut<'a> {
    pub(crate) cells: &'a mut PagedCells,
    pub(crate) soft_wrap: &'a [bool],
    pub(crate) occupancy: &'a [u16],
    pub(crate) base: usize,
    pub(crate) phys_cap: usize,
    pub(crate) history_len: &'a mut usize,
    pub(crate) rows: usize,
    pub(crate) cols: usize,
    pub(crate) cold: &'a mut ColdStore,
    pub(crate) gap: &'a mut GapReuse,
}

impl RingMut<'_> {
    /// Moves up to `max_rows` of the oldest ring history rows past the
    /// hot window (one screen of history) into the cold store, then
    /// discards the vacated pages. `true` once nothing is left to move.
    pub(crate) fn compact(self, max_rows: usize) -> bool {
        let excess = self.history_len.saturating_sub(self.rows);
        let moved = excess.min(max_rows);
        let first_vacated = (self.base + self.phys_cap - *self.history_len) % self.phys_cap;
        for _ in 0..moved {
            let phys = (self.base + self.phys_cap - *self.history_len) % self.phys_cap;
            let start = phys * self.cols;
            let occupied = usize::from(self.occupancy[phys]).min(self.cols);
            self.cold.push_back(
                &self.cells[start..start + occupied],
                self.soft_wrap[phys],
                self.cols,
            );
            *self.history_len -= 1;
        }
        let gap = self.phys_cap - *self.history_len - self.rows;
        if gap == 0 {
            return moved == excess;
        }
        let discard = if self.gap.is_intact() {
            vacant_rows(self.base, self.phys_cap, *self.history_len, self.rows)
        } else {
            // The page that straddles the previous slice's last row and
            // this slice's first lies wholly inside the gap only now.
            let row_bytes = self.cols * size_of::<Cell>();
            let back = paged_cells::page_bytes()
                .div_ceil(row_bytes)
                .min(gap - moved);
            ring_segments(
                (first_vacated + self.phys_cap - back) % self.phys_cap,
                moved + back,
                self.phys_cap,
            )
        };
        if moved > 0 || self.gap.is_intact() {
            for rows in discard {
                // A failed discard changed nothing: those pages stay
                // resident and every row stays valid.
                drop(
                    self.cells
                        .discard(rows.start * self.cols..rows.end * self.cols),
                );
            }
            *self.gap = GapReuse(0);
        }
        moved == excess
    }

    /// Before the gap's first row turns live: makes sure its pages are in
    /// use again (macOS accounts for them there).
    #[inline]
    pub(crate) fn take_gap_row(self) {
        debug_assert!(
            self.gap.0 < self.phys_cap - *self.history_len - self.rows,
            "the reused prefix lies inside the gap"
        );
        if self.gap.0 == 0 {
            self.reuse_ahead();
        } else {
            self.gap.0 -= 1;
        }
    }

    /// Reuses the pages under the next [`REUSE_AHEAD_BYTES`] of the gap,
    /// not the whole gap: macOS charges every page `MADV_FREE_REUSE`
    /// covers at once, so footprint would jump back to the full ring on
    /// the first line of output.
    #[cold]
    fn reuse_ahead(self) {
        let gap = self.phys_cap - *self.history_len - self.rows;
        let ahead = REUSE_AHEAD_BYTES
            .max(paged_cells::page_bytes())
            .div_ceil(self.cols * size_of::<Cell>())
            .min(gap);
        let front = (self.base + self.rows) % self.phys_cap;
        for rows in ring_segments(front, ahead, self.phys_cap) {
            drop(
                self.cells
                    .reuse(rows.start * self.cols..rows.end * self.cols),
            );
        }
        *self.gap = if ahead == gap {
            GapReuse::INTACT
        } else {
            GapReuse(ahead - 1)
        };
    }
}

/// One `madvise` per this much refilled history, rather than per page.
pub(crate) const REUSE_AHEAD_BYTES: usize = 64 << 10;

/// Which of the vacant gap's rows may turn live without reusing their
/// pages first. Either no page wholly inside the gap is discarded, or
/// the first `n` gap rows (in ring order from the viewport bottom) lie
/// on reused pages and `n` is less than the gap's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GapReuse(usize);

impl GapReuse {
    pub(crate) const INTACT: Self = Self(usize::MAX);

    #[inline]
    pub(crate) const fn is_intact(self) -> bool {
        self.0 == usize::MAX
    }
}

impl Default for GapReuse {
    fn default() -> Self {
        Self::INTACT
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{live_rows, vacant_rows};

    /// The live window and the vacant gap partition the ring: every
    /// physical row lies in exactly one of the four ranges, so a scan
    /// over the live ranges touches no vacant row and misses no live one.
    #[kani::proof]
    fn live_and_vacant_rows_partition_the_ring() {
        let phys_cap: usize = kani::any();
        let rows: usize = kani::any();
        let history_len: usize = kani::any();
        let base: usize = kani::any();
        let row: usize = kani::any();
        kani::assume(phys_cap <= 1 << 20 && rows >= 1 && rows <= phys_cap);
        kani::assume(history_len <= phys_cap);
        kani::assume(history_len + rows <= phys_cap && base < phys_cap && row < phys_cap);
        let [l0, l1] = live_rows(base, phys_cap, history_len, rows);
        let [v0, v1] = vacant_rows(base, phys_cap, history_len, rows);
        for r in [&l0, &l1, &v0, &v1] {
            assert!(r.start <= r.end && r.end <= phys_cap);
        }
        let hits = [&l0, &l1, &v0, &v1]
            .iter()
            .filter(|r| r.contains(&row))
            .count();
        assert_eq!(hits, 1);
        let live = (l0.end - l0.start) + (l1.end - l1.start);
        assert_eq!(live, history_len + rows);
    }
}

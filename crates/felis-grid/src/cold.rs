//! The cold tier of scrollback: history rows older than the ring's hot
//! window, each clipped at its occupancy and packed into recycled chunks
//! in FIFO order (docs/explanation/data-model/scrollback.md "Structure").

use std::collections::VecDeque;

use crate::Cell;

/// Smallest chunk, in cells; a chunk also holds at least sixteen rows of
/// the widest row it may take, so a row never straddles two chunks.
const MIN_CHUNK_SHIFT: u32 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ColdRow {
    /// Offset of the row's first cell in the store's cell stream, where
    /// chunk `n` spans `[n << shift, (n + 1) << shift)`.
    start: u64,
    len: u16,
    soft_wrap: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ColdStore {
    /// Oldest first.
    rows: VecDeque<ColdRow>,
    /// `chunks[i]` is stream chunk `first_chunk + i`; its length is how
    /// far rows fill it.
    chunks: VecDeque<Vec<Cell>>,
    first_chunk: u64,
    /// Where the next row may start.
    next: u64,
    /// Fixed while any row is stored, so every offset decodes the same.
    shift: u32,
    /// One emptied chunk kept for the next fill, so a steady
    /// push-and-evict cycle allocates nothing.
    spare: Option<Vec<Cell>>,
}

/// The chunk size, as a shift, for rows at most `cols` cells wide.
fn chunk_shift(cols: usize) -> u32 {
    cols.saturating_mul(16)
        .max(1)
        .next_power_of_two()
        .trailing_zeros()
        .max(MIN_CHUNK_SHIFT)
}

/// Where a `len`-cell row goes when the stream is filled up to `next`:
/// there, or at the next chunk boundary when the rest of the chunk is
/// too short. Requires `len <= 1 << shift`.
const fn place(next: u64, len: u64, shift: u32) -> u64 {
    let size = 1u64 << shift;
    let within = next & (size - 1);
    if within + len > size {
        (next | (size - 1)) + 1
    } else {
        next
    }
}

impl ColdStore {
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Appends `cells` as the youngest cold row. `cols` is the width of
    /// the ring it came from, which bounds every row the store takes
    /// until it empties.
    pub(crate) fn push_back(&mut self, cells: &[Cell], soft_wrap: bool, cols: usize) {
        if self.rows.is_empty() {
            self.reset(chunk_shift(cols));
        }
        let size = 1usize << self.shift;
        let len = u16::try_from(cells.len()).unwrap_or(u16::MAX);
        assert!(usize::from(len) == cells.len() && cells.len() <= size);
        let start = place(self.next, cells.len() as u64, self.shift);
        let chunk = usize::try_from((start >> self.shift) - self.first_chunk).unwrap_or(usize::MAX);
        while self.chunks.len() <= chunk {
            let fresh = self
                .spare
                .take()
                .unwrap_or_else(|| Vec::with_capacity(size));
            self.chunks.push_back(fresh);
        }
        let dst = &mut self.chunks[chunk];
        debug_assert_eq!(dst.len() as u64, start & (size as u64 - 1));
        dst.extend_from_slice(cells);
        self.next = start + cells.len() as u64;
        self.rows.push_back(ColdRow {
            start,
            len,
            soft_wrap,
        });
    }

    /// Drops the oldest row and recycles every chunk no remaining row
    /// reaches: O(1), since a row spans one chunk.
    pub(crate) fn pop_front(&mut self) {
        if self.rows.pop_front().is_none() {
            return;
        }
        let Some(front) = self.rows.front() else {
            self.reset(self.shift);
            return;
        };
        let keep_from = front.start >> self.shift;
        while self.first_chunk < keep_from {
            let Some(mut emptied) = self.chunks.pop_front() else {
                break;
            };
            self.first_chunk += 1;
            emptied.clear();
            self.spare = Some(emptied);
        }
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    fn reset(&mut self, shift: u32) {
        if self.shift == shift
            && let Some(mut chunk) = self.chunks.pop_front()
        {
            chunk.clear();
            self.spare = Some(chunk);
        }
        if self.shift != shift {
            self.spare = None;
        }
        self.chunks.clear();
        self.first_chunk = 0;
        self.next = 0;
        self.shift = shift;
    }

    /// Cold row `idx` from the oldest, as stored.
    pub(crate) fn row(&self, idx: usize) -> Option<&[Cell]> {
        let row = self.rows.get(idx)?;
        let (chunk, within) = self.locate(row);
        Some(&self.chunks[chunk][within..within + usize::from(row.len)])
    }

    pub(crate) fn soft_wrap(&self, idx: usize) -> Option<bool> {
        self.rows.get(idx).map(|row| row.soft_wrap)
    }

    /// Every stored row, oldest first, for the table sweeps.
    pub(crate) fn for_each_row_mut(&mut self, mut f: impl FnMut(&mut [Cell])) {
        for row in &self.rows {
            let (chunk, within) = locate(row, self.first_chunk, self.shift);
            f(&mut self.chunks[chunk][within..within + usize::from(row.len)]);
        }
    }

    pub(crate) fn rows(&self) -> impl Iterator<Item = (&[Cell], bool)> {
        self.rows.iter().map(|row| {
            let (chunk, within) = self.locate(row);
            (
                &self.chunks[chunk][within..within + usize::from(row.len)],
                row.soft_wrap,
            )
        })
    }

    /// Cells held, spare chunk and per-row index included.
    #[cfg_attr(not(test), expect(dead_code, reason = "measured by tests only"))]
    pub(crate) fn heap_bytes(&self) -> usize {
        let chunks: usize = self
            .chunks
            .iter()
            .chain(&self.spare)
            .map(Vec::capacity)
            .sum();
        chunks * size_of::<Cell>() + self.rows.capacity() * size_of::<ColdRow>()
    }

    fn locate(&self, row: &ColdRow) -> (usize, usize) {
        locate(row, self.first_chunk, self.shift)
    }
}

/// `(chunk index, offset within it)` of a stored row.
fn locate(row: &ColdRow, first_chunk: u64, shift: u32) -> (usize, usize) {
    let chunk = (row.start >> shift) - first_chunk;
    let within = row.start & ((1u64 << shift) - 1);
    (
        usize::try_from(chunk).unwrap_or(usize::MAX),
        usize::try_from(within).unwrap_or(usize::MAX),
    )
}

#[cfg(test)]
mod tests;

#[cfg(kani)]
mod kani_proofs {
    use super::{MIN_CHUNK_SHIFT, place};

    /// A placed row starts at or after the fill point, wastes less than a
    /// chunk, and ends inside the chunk it starts in, so `locate`'s
    /// `(start >> shift, start & mask)` always addresses `len` cells of
    /// one chunk.
    #[kani::proof]
    fn a_placed_row_lies_inside_one_chunk() {
        let shift: u32 = kani::any();
        kani::assume((MIN_CHUNK_SHIFT..=17).contains(&shift));
        let size = 1u64 << shift;
        let next: u64 = kani::any();
        kani::assume(next < 1 << 48);
        let len: u64 = kani::any();
        kani::assume(len <= size);
        let start = place(next, len, shift);
        assert!(start >= next);
        assert!(start - next < size);
        assert!((start & (size - 1)) + len <= size);
        if len > 0 {
            assert_eq!(start >> shift, (start + len - 1) >> shift);
        }
    }

    /// Successive rows never overlap and keep stream order, which is what
    /// lets a chunk be recycled once the front row has moved past it.
    #[kani::proof]
    fn successive_rows_keep_stream_order() {
        let shift: u32 = kani::any();
        kani::assume((MIN_CHUNK_SHIFT..=17).contains(&shift));
        let size = 1u64 << shift;
        let next: u64 = kani::any();
        kani::assume(next < 1 << 48);
        let (a, b): (u64, u64) = (kani::any(), kani::any());
        kani::assume(a <= size && b <= size);
        let first = place(next, a, shift);
        let second = place(first + a, b, shift);
        assert!(second >= first + a);
        assert!(second >> shift >= first >> shift);
        assert!((second >> shift) - (first >> shift) <= 1);
    }
}

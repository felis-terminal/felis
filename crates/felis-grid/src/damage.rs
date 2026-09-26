//! Row-granularity damage tracker, a bitset rather than `Vec<bool>`:
//! the daemon probes a scroll region per line feed, which must stay
//! one AND + compare.

use crate::ScrollDirection;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Damage {
    /// Tail bits beyond `len` stay zero; `dirty_rows()` and
    /// `mark_all()` depend on it.
    blocks: Vec<u64>,
    len: usize,
}

impl Damage {
    const BITS: usize = u64::BITS as usize;

    const fn block_count(rows: usize) -> usize {
        rows.div_ceil(Self::BITS)
    }

    /// All rows clean, unlike [`Self::resize`]: the daemon seeds a
    /// per-subscriber tracker right after a rehydrate burst
    /// (`docs/explanation/architecture/session-lifecycle.md` "Same-user
    /// mirroring"), which leaves nothing dirty.
    #[must_use]
    pub fn new(rows: usize) -> Self {
        Self {
            blocks: vec![0; Self::block_count(rows)],
            len: rows,
        }
    }

    /// A length mismatch means the grid resized since this tracker was
    /// sized; it resizes to match, which marks everything dirty.
    pub fn merge(&mut self, other: &Self) {
        if self.len != other.len {
            self.resize(other.len);
            return;
        }
        for (mine, theirs) in self.blocks.iter_mut().zip(&other.blocks) {
            *mine |= *theirs;
        }
    }

    const fn last_block_mask(len: usize) -> u64 {
        let tail = len % Self::BITS;
        if tail == 0 { !0u64 } else { (1u64 << tail) - 1 }
    }

    pub fn clear(&mut self) {
        self.blocks.fill(0);
    }

    /// Out-of-range rows are ignored.
    pub fn mark(&mut self, row: usize) {
        if row < self.len {
            self.blocks[row / Self::BITS] |= 1u64 << (row % Self::BITS);
        }
    }

    /// Mark `[start, end)` dirty. Out-of-range arguments clamp to
    /// `[0, len)`.
    pub fn mark_range(&mut self, start: usize, end: usize) {
        let start = start.min(self.len);
        let end = end.min(self.len);
        if start >= end {
            return;
        }
        let first_block = start / Self::BITS;
        let last_block = (end - 1) / Self::BITS;
        let head_offset = start % Self::BITS;
        let tail_excl = end % Self::BITS;
        if first_block == last_block {
            let tail = if tail_excl == 0 {
                Self::BITS
            } else {
                tail_excl
            };
            let bits = tail - head_offset;
            let mask = if bits == Self::BITS {
                !0u64
            } else {
                ((1u64 << bits) - 1) << head_offset
            };
            self.blocks[first_block] |= mask;
            return;
        }
        self.blocks[first_block] |= !0u64 << head_offset;
        for block in &mut self.blocks[first_block + 1..last_block] {
            *block = !0u64;
        }
        let tail_mask = if tail_excl == 0 {
            !0u64
        } else {
            (1u64 << tail_excl) - 1
        };
        self.blocks[last_block] |= tail_mask;
    }

    pub fn mark_all(&mut self) {
        if self.blocks.is_empty() {
            return;
        }
        let last = self.blocks.len() - 1;
        for block in &mut self.blocks[..last] {
            *block = !0u64;
        }
        self.blocks[last] = Self::last_block_mask(self.len);
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Out-of-range rows read clean.
    #[must_use]
    pub fn is_dirty(&self, row: usize) -> bool {
        row < self.len && self.blocks[row / Self::BITS] & (1u64 << (row % Self::BITS)) != 0
    }

    /// Dirty rows in ascending order.
    pub fn dirty_rows(&self) -> impl Iterator<Item = usize> + '_ {
        let current = self.blocks.first().copied().unwrap_or(0);
        DirtyRows {
            blocks: &self.blocks,
            cursor: 0,
            current,
        }
    }

    /// Whether any row in `[start, end)` is dirty. Out-of-range
    /// arguments clamp to `[0, len)`.
    #[must_use]
    pub fn any_dirty_in_range(&self, start: usize, end: usize) -> bool {
        let start = start.min(self.len);
        let end = end.min(self.len);
        if start >= end {
            return false;
        }
        let first_block = start / Self::BITS;
        let last_block = (end - 1) / Self::BITS;
        let head_offset = start % Self::BITS;
        let tail_excl = end % Self::BITS;
        if first_block == last_block {
            let tail = if tail_excl == 0 {
                Self::BITS
            } else {
                tail_excl
            };
            let bits = tail - head_offset;
            let mask = if bits == Self::BITS {
                !0u64
            } else {
                ((1u64 << bits) - 1) << head_offset
            };
            return self.blocks[first_block] & mask != 0;
        }
        let head_mask = !0u64 << head_offset;
        if self.blocks[first_block] & head_mask != 0 {
            return true;
        }
        for block in &self.blocks[first_block + 1..last_block] {
            if *block != 0 {
                return true;
            }
        }
        let tail_mask = if tail_excl == 0 {
            !0u64
        } else {
            (1u64 << tail_excl) - 1
        };
        self.blocks[last_block] & tail_mask != 0
    }

    /// Whether every row in `[start, end)` is dirty; vacuously true for
    /// an empty range. Out-of-range arguments clamp to `[0, len)`.
    #[must_use]
    pub fn all_dirty_in_range(&self, start: usize, end: usize) -> bool {
        let end = end.min(self.len);
        let mut at = start.min(end);
        while at < end {
            let len = (end - at).min(Self::BITS);
            if self.bits_at(at, len) != Self::low_mask(len) {
                return false;
            }
            at += len;
        }
        true
    }

    /// Move the flags of the band `[top, bottom]` (inclusive) with a
    /// scroll of `n` rows in `direction`, the way the rows themselves
    /// move: flags shifted past the band's edge drop, and the `n` rows
    /// the scroll vacates are marked. A band reaching past `len` clamps
    /// to it.
    pub fn shift_band(&mut self, top: usize, bottom: usize, n: usize, direction: ScrollDirection) {
        let end = bottom.saturating_add(1).min(self.len);
        if top >= end || n == 0 {
            return;
        }
        let n = n.min(end - top);
        let moved = end - top - n;
        if top / Self::BITS == (end - 1) / Self::BITS {
            let block = top / Self::BITS;
            let offset = top % Self::BITS;
            let band = Self::low_mask(end - top) << offset;
            let word = self.blocks[block];
            let shift = u32::try_from(n).unwrap_or(u32::MAX);
            let (kept, vacated) = match direction {
                ScrollDirection::Up => (
                    (word & band).checked_shr(shift).unwrap_or(0) & band,
                    Self::low_mask(n) << (offset + moved),
                ),
                ScrollDirection::Down => (
                    (word & band).checked_shl(shift).unwrap_or(0) & band,
                    Self::low_mask(n) << offset,
                ),
            };
            self.blocks[block] = (word & !band) | kept | vacated;
            return;
        }
        match direction {
            ScrollDirection::Up => {
                let mut dst = top;
                while dst < top + moved {
                    let len = (top + moved - dst).min(Self::BITS);
                    let bits = self.bits_at(dst + n, len);
                    self.set_bits_at(dst, len, bits);
                    dst += len;
                }
                self.mark_range(end - n, end);
            }
            ScrollDirection::Down => {
                let mut dst_end = end;
                while dst_end > top + n {
                    let len = (dst_end - top - n).min(Self::BITS);
                    let bits = self.bits_at(dst_end - len - n, len);
                    self.set_bits_at(dst_end - len, len, bits);
                    dst_end -= len;
                }
                self.mark_range(top, top + n);
            }
        }
    }

    const fn low_mask(len: usize) -> u64 {
        if len >= Self::BITS {
            !0u64
        } else {
            (1u64 << len) - 1
        }
    }

    /// The `len <= 64` flags starting at row `start`, row `start` in bit 0.
    fn bits_at(&self, start: usize, len: usize) -> u64 {
        let block = start / Self::BITS;
        let offset = start % Self::BITS;
        let mut bits = self.blocks[block] >> offset;
        if offset != 0 && offset + len > Self::BITS {
            bits |= self.blocks[block + 1] << (Self::BITS - offset);
        }
        bits & Self::low_mask(len)
    }

    fn set_bits_at(&mut self, start: usize, len: usize, bits: u64) {
        let block = start / Self::BITS;
        let offset = start % Self::BITS;
        let mask = Self::low_mask(len);
        let bits = bits & mask;
        self.blocks[block] = (self.blocks[block] & !(mask << offset)) | (bits << offset);
        if offset != 0 && offset + len > Self::BITS {
            let spill = Self::BITS - offset;
            self.blocks[block + 1] = (self.blocks[block + 1] & !(mask >> spill)) | (bits >> spill);
        }
    }

    /// Resize to `rows`, starting every row dirty. Public because the
    /// daemon's per-subscriber trackers live outside any `Grid`
    /// (`docs/explanation/architecture/session-lifecycle.md` "Same-user
    /// mirroring").
    pub fn resize(&mut self, rows: usize) {
        self.len = rows;
        self.blocks.resize(Self::block_count(rows), 0);
        // Also re-masks the tail past `rows`.
        self.mark_all();
    }
}

struct DirtyRows<'a> {
    blocks: &'a [u64],
    cursor: usize,
    current: u64,
}

impl Iterator for DirtyRows<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.current != 0 {
                let bit = self.current.trailing_zeros() as usize;
                self.current &= self.current - 1;
                return Some(self.cursor * Damage::BITS + bit);
            }
            self.cursor += 1;
            if self.cursor >= self.blocks.len() {
                return None;
            }
            self.current = self.blocks[self.cursor];
        }
    }
}

#[cfg(test)]
mod tests;

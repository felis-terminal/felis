//! Interned SGR attribute registry
//! (`docs/explanation/data-model/grid-and-cells.md` "Style interning").
//! Id 0 is always the default pen (`Cell::BLANK` is const). Id equality
//! matches attribute equality within a table. Daemon and shadow intern
//! independently; the wire ships resolved [`Attributes`].

use std::collections::HashMap;

use foldhash::fast::FixedState;

use crate::{AttrFlags, Attributes};

/// Not `SipHash`: `intern` hashes a 16-byte [`Attributes`] per cell on a
/// truecolor producer, and the hash cost dominates the parse floor
/// there (`docs/explanation/data-model/grid-and-cells.md` "Style
/// interning" weighs that against the fixed seed's exposure).
type PenMap = HashMap<PenKey, StyleId, FixedState>;

/// [`Attributes::pack`]: a probe compares two words where the derived
/// `Attributes` equality matches three enums.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PenKey([u64; 2]);

impl core::hash::Hash for PenKey {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.0[0]);
        state.write_u64(self.0[1]);
    }
}

/// Handle into a grid's [`StyleTable`]. A plain `u32`: id 0 is the
/// default pen, so a cell's default is a zero bit-pattern. Not `u16`:
/// `Grapheme` is `align(4)`, so a narrower id pads back to the same
/// cell size, and a truecolor session can hold more than 65 535 live
/// pens between compactions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct StyleId(u32);

impl StyleId {
    /// Resolves to `Attributes::default()` in every [`StyleTable`].
    pub const DEFAULT: Self = Self(0);

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

const RECENT_BITS: u32 = 10;

/// An all-zero slot is a true entry (the default pen packs to zero and
/// is always id 0), so a fresh or cleared cache needs no validity flag.
#[derive(Debug, Clone, Copy, Default)]
struct RecentSlot {
    key: PenKey,
    id: StyleId,
}

/// Equality compares `entries` alone: `dedup` and `recent` only index
/// them.
#[derive(Debug, Clone)]
pub struct StyleTable {
    entries: Vec<Attributes>,
    dedup: PenMap,
    /// Direct-mapped in front of `dedup`, not a single last-hit entry:
    /// DOOM-fire alternates among a few hundred pens, so consecutive
    /// SGRs rarely repeat one but nearly always hit a recent one.
    /// `compact` clears it, since it renumbers the ids it holds.
    recent: Box<[RecentSlot; 1 << RECENT_BITS]>,
    /// Whether any entry carries `ISO_PROTECTED`; kept a function of
    /// `entries` alone, so no id of any age can resolve to a protected
    /// pen while it is clear.
    has_iso_protected: bool,
}

impl PartialEq for StyleTable {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for StyleTable {}

impl Default for StyleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleTable {
    #[must_use]
    pub fn new() -> Self {
        let default = Attributes::default();
        let mut dedup = PenMap::default();
        dedup.insert(PenKey(default.pack()), StyleId::DEFAULT);
        Self {
            entries: vec![default],
            dedup,
            recent: vec![RecentSlot::default(); 1 << RECENT_BITS]
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!()),
            has_iso_protected: false,
        }
    }

    /// An id past the table (junk from a peer, or a stale handle after
    /// an unapplied `compact`) resolves to the default pen, as
    /// `sizing_by_handle` does.
    #[must_use]
    pub fn resolve(&self, id: StyleId) -> &Attributes {
        self.entries
            .get(id.get() as usize)
            .unwrap_or(&self.entries[0])
    }

    pub fn intern(&mut self, attrs: Attributes) -> StyleId {
        let key = PenKey(attrs.pack());
        let idx = recent_index(key);
        let slot = self.recent[idx];
        if slot.key == key {
            return slot.id;
        }
        let id = self.intern_uncached(key, attrs);
        self.recent[idx] = RecentSlot { key, id };
        id
    }

    fn intern_uncached(&mut self, key: PenKey, attrs: Attributes) -> StyleId {
        if let Some(&id) = self.dedup.get(&key) {
            return id;
        }
        let id = StyleId(u32::try_from(self.entries.len()).unwrap_or(u32::MAX));
        self.entries.push(attrs);
        self.dedup.insert(key, id);
        self.has_iso_protected |= attrs.flags.contains(AttrFlags::ISO_PROTECTED);
        id
    }

    #[must_use]
    pub const fn has_iso_protected(&self) -> bool {
        self.has_iso_protected
    }

    /// Includes the default pen.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table holds only the default pen.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.len() <= 1
    }

    /// A dense `bool` per id rather than a `HashSet<StyleId>`: the mark
    /// pass visits every cell of the ring, and hashing each id is 90%
    /// of the parse thread under a unique-pen flood.
    #[must_use]
    pub fn mark_buffer(&self) -> Vec<bool> {
        vec![false; self.entries.len()]
    }

    /// An id past the buffer is dropped, matching [`Self::resolve`].
    pub fn mark(marks: &mut [bool], id: StyleId) {
        if let Some(slot) = marks.get_mut(id.get() as usize) {
            *slot = true;
        }
    }

    /// Keep only the marked ids plus the default, returning a remap
    /// indexed by old id, or `None` when nothing moved. On `Some`, the
    /// caller must rewrite every stored [`StyleId`] (live and saved
    /// cells, memoized pen ids) before the next `intern` / `resolve`.
    #[must_use = "dropping the remap leaves every stored StyleId pointing at the wrong pen"]
    pub fn compact(&mut self, marks: &[bool]) -> Option<Vec<StyleId>> {
        let live = 1 + marks.iter().skip(1).filter(|m| **m).count();
        // A sweep that reclaims nothing (a flood whose pens are all
        // still on screen) must not cost the caller a full rewrite.
        if live == self.entries.len() {
            return None;
        }
        let mut remap = vec![StyleId::DEFAULT; self.entries.len()];
        let mut new_entries: Vec<Attributes> = Vec::with_capacity(live);
        for (old_idx, &attrs) in self.entries.iter().enumerate() {
            if old_idx == 0 || marks.get(old_idx).copied().unwrap_or(false) {
                let new = StyleId(u32::try_from(new_entries.len()).unwrap_or(u32::MAX));
                new_entries.push(attrs);
                remap[old_idx] = new;
            }
        }
        // In place rather than a fresh map, which would rehash every
        // survivor.
        self.dedup.retain(|_, id| {
            let old = id.get() as usize;
            let kept = old == 0 || marks.get(old).copied().unwrap_or(false);
            if kept {
                *id = remap[old];
            }
            kept
        });
        self.has_iso_protected = new_entries
            .iter()
            .any(|a| a.flags.contains(AttrFlags::ISO_PROTECTED));
        self.entries = new_entries;
        self.recent.fill(RecentSlot::default());
        Some(remap)
    }

    #[cfg(feature = "state-dump")]
    pub(crate) fn entries(&self) -> &[Attributes] {
        &self.entries
    }

    /// Rebuilds a table whose ids are the positions in `entries`. `None`
    /// when slot 0 is not the default pen or a pen appears twice, either
    /// of which would break id equality matching pen equality.
    #[cfg(feature = "state-dump")]
    pub(crate) fn from_entries(entries: &[Attributes]) -> Option<Self> {
        let (first, rest) = entries.split_first()?;
        if *first != Attributes::default() {
            return None;
        }
        let mut table = Self::new();
        for attrs in rest {
            let before = table.len();
            let _id = table.intern(*attrs);
            if table.len() != before + 1 {
                return None;
            }
        }
        Some(table)
    }
}

const fn recent_index(PenKey([lo, hi]): PenKey) -> usize {
    let h = (lo ^ hi.rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h >> (64 - RECENT_BITS)) as usize
}

#[cfg(test)]
mod tests;

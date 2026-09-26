//! Interned grapheme-cluster registry.
//!
//! Monotonic growth: cells in reattaching clients retain handles, so entries
//! are never freed or moved. Only the daemon mints handles.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::num::NonZeroU32;

use foldhash::fast::FixedState;

use crate::ClusterText;

/// Cap on entries per grid's cluster interner to prevent unbounded table growth.
/// Past this cap, marks stop folding without corrupting cells.
pub const CLUSTER_TABLE_CAP: usize = 1 << 17;

/// Keyed by digest rather than text so the index holds no second copy
/// of every cluster; a hit is confirmed against the entry, so a
/// collision costs a duplicate row, never the wrong text.
type ClusterMap = HashMap<u64, NonZeroU32, FixedState>;

/// Grid-owned registry mapping [`NonZeroU32`] handles to cluster text.
#[derive(Debug, Clone, Default)]
pub struct ClusterTable {
    /// `None` is a gap: the visible-first rehydrate feeding
    /// [`Self::install`] sends entry 100 before 1-99, so ids no row
    /// names may be absent. A gap resolves to nothing, where an entry
    /// whose text is legitimately empty draws nothing.
    entries: Vec<Option<ClusterText>>,
    dedup: ClusterMap,
    /// Reused across folds: repeated marks hit dedup each time, and fresh
    /// strings per mark would dominate short-lived heap allocations.
    /// Not table state, so neither serialized nor compared.
    pub(crate) scratch: String,
}

impl PartialEq for ClusterTable {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for ClusterTable {}

impl ClusterTable {
    /// `None` for a handle past the table or on a gap, which a
    /// reattaching client can hold before the `Cluster` message streams.
    #[must_use]
    pub fn get(&self, id: NonZeroU32) -> Option<&str> {
        self.entries
            .get(id.get() as usize - 1)?
            .as_ref()
            .map(AsRef::as_ref)
    }

    /// Handles the table spans, gaps included.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `None` when the text is past [`ClusterText::CAP`] or the table
    /// is full; the caller leaves the cell as it stands either way.
    pub fn intern(&mut self, text: &str) -> Option<NonZeroU32> {
        let key = Self::digest(text);
        if let Some(&existing) = self.dedup.get(&key)
            && self.get(existing) == Some(text)
        {
            return Some(existing);
        }
        // After the dedup lookup, not before: a full table must keep
        // folding the clusters already on screen.
        if self.entries.len() >= CLUSTER_TABLE_CAP {
            return None;
        }
        let text = ClusterText::new(text)?;
        let handle = NonZeroU32::new(u32::try_from(self.entries.len() + 1).ok()?)?;
        self.dedup.insert(key, handle);
        self.entries.push(Some(text));
        Some(handle)
    }

    /// Install a cluster at a daemon-assigned handle, growing with gaps.
    /// `false` at or past [`CLUSTER_TABLE_CAP`], which would let a peer
    /// force a multi-gigabyte gap fill, and for an id already installed,
    /// whose handle cells hold: a second text would redefine what they
    /// already drew.
    pub fn install(&mut self, id: NonZeroU32, text: ClusterText) -> bool {
        let idx = id.get() as usize - 1;
        if idx >= CLUSTER_TABLE_CAP {
            return false;
        }
        if self.entries.get(idx).is_some_and(Option::is_some) {
            return false;
        }
        if idx >= self.entries.len() {
            self.entries.resize_with(idx + 1, || None);
        }
        self.dedup.insert(Self::digest(text.as_str()), id);
        self.entries[idx] = Some(text);
        true
    }

    fn digest(text: &str) -> u64 {
        FixedState::default().hash_one(text)
    }
}

#[cfg(test)]
mod tests;

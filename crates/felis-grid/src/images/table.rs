//! The insertion-ordered, keyed storage behind [`super::Placements`].

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{ImageId, Placement, PlacementId, VirtualPlacement};

pub(super) type Key = (ImageId, Option<PlacementId>);

pub(super) trait Keyed {
    fn key(&self) -> Key;
}

impl Keyed for Placement {
    fn key(&self) -> Key {
        (self.image_id, self.placement_id)
    }
}

impl Keyed for VirtualPlacement {
    fn key(&self) -> Key {
        (self.image_id, self.placement_id)
    }
}

/// Entries ordered by the sequence number they were inserted under, and
/// found by key without a scan.
#[derive(Debug, Clone)]
pub(super) struct Table<T> {
    by_seq: BTreeMap<u64, T>,
    seq_of: HashMap<Key, u64>,
    /// Each image's entries, so dropping or asking after one image costs
    /// its own placements rather than the table's.
    by_image: HashMap<ImageId, BTreeSet<u64>>,
}

impl<T: Keyed> Table<T> {
    pub(super) fn new() -> Self {
        Self {
            by_seq: BTreeMap::new(),
            seq_of: HashMap::new(),
            by_image: HashMap::new(),
        }
    }

    fn index(&mut self, key: Key, seq: u64) {
        self.seq_of.insert(key, seq);
        self.by_image.entry(key.0).or_default().insert(seq);
    }

    fn unindex(&mut self, key: Key) -> Option<u64> {
        let seq = self.seq_of.remove(&key)?;
        if let Some(seqs) = self.by_image.get_mut(&key.0) {
            seqs.remove(&seq);
            if seqs.is_empty() {
                self.by_image.remove(&key.0);
            }
        }
        Some(seq)
    }

    pub(super) fn len(&self) -> usize {
        self.by_seq.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.by_seq.is_empty()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &T> {
        self.by_seq.values()
    }

    pub(super) fn iter_seq(&self) -> impl Iterator<Item = (u64, &T)> {
        self.by_seq.iter().map(|(&seq, entry)| (seq, entry))
    }

    pub(super) fn get(&self, key: Key) -> Option<&T> {
        self.by_seq.get(self.seq_of.get(&key)?)
    }

    pub(super) fn contains(&self, key: Key) -> bool {
        self.seq_of.contains_key(&key)
    }

    pub(super) fn oldest_seq(&self) -> Option<u64> {
        self.by_seq.first_key_value().map(|(&seq, _)| seq)
    }

    /// Appends under `seq`, which is newer than every entry's. An entry
    /// already under the key leaves the table and comes back.
    pub(super) fn insert(&mut self, seq: u64, entry: T) -> Option<T> {
        let old = self.remove(entry.key());
        self.index(entry.key(), seq);
        self.by_seq.insert(seq, entry);
        old
    }

    /// Puts `entry` where the entry under `key` was, keeping its
    /// position; does nothing when `key` is free.
    pub(super) fn replace(&mut self, key: Key, entry: T) -> Option<T> {
        let seq = self.unindex(key)?;
        if let Some(other) = self.unindex(entry.key()) {
            self.by_seq.remove(&other);
        }
        self.index(entry.key(), seq);
        self.by_seq.insert(seq, entry)
    }

    pub(super) fn remove(&mut self, key: Key) -> Option<T> {
        let seq = self.unindex(key)?;
        self.by_seq.remove(&seq)
    }

    pub(super) fn has_image(&self, image_id: ImageId) -> bool {
        self.by_image.contains_key(&image_id)
    }

    /// The image's entries, in table order.
    pub(super) fn of_image(&self, image_id: ImageId) -> impl Iterator<Item = &T> {
        self.by_image
            .get(&image_id)
            .into_iter()
            .flatten()
            .filter_map(|seq| self.by_seq.get(seq))
    }

    /// Removes the image's entries and returns them in table order.
    pub(super) fn extract_image(&mut self, image_id: ImageId) -> Vec<T> {
        let seqs = self.by_image.remove(&image_id).unwrap_or_default();
        seqs.into_iter()
            .filter_map(|seq| {
                let entry = self.by_seq.remove(&seq)?;
                self.seq_of.remove(&entry.key());
                Some(entry)
            })
            .collect()
    }

    pub(super) fn pop_oldest(&mut self) -> Option<T> {
        let (_, entry) = self.by_seq.pop_first()?;
        self.unindex(entry.key());
        Some(entry)
    }

    /// Removes the entries `remove` picks and returns them in table
    /// order. `remove` may rewrite what it keeps, except the key.
    pub(super) fn extract(&mut self, mut remove: impl FnMut(&mut T) -> bool) -> Vec<T> {
        let doomed: Vec<u64> = self
            .by_seq
            .iter_mut()
            .filter_map(|(&seq, entry)| {
                let key = entry.key();
                let gone = remove(entry);
                debug_assert!(gone || entry.key() == key, "a kept entry changed its key");
                gone.then_some(seq)
            })
            .collect();
        doomed
            .into_iter()
            .filter_map(|seq| {
                let entry = self.by_seq.remove(&seq)?;
                self.unindex(entry.key());
                Some(entry)
            })
            .collect()
    }

    /// Every entry, for rewrites that leave keys alone.
    pub(super) fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.by_seq.values_mut()
    }
}

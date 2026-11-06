//! The insertion-ordered storage behind [`super::ImageStore`], indexed
//! by what eviction asks for so a pass costs what it evicts rather than
//! what the store holds.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use foldhash::fast::FixedState;

use super::{ImageEntry, ImageId};

/// Every index here is derived from the entries' `refcount`, `atime`
/// and byte length, so those change only through this type's methods;
/// [`Self::get_mut`] and [`Self::iter_mut`] lend the rest.
#[derive(Debug, Clone, Default)]
pub(crate) struct Entries {
    by_seq: BTreeMap<u64, (ImageId, ImageEntry)>,
    seq_of: HashMap<ImageId, u64, FixedState>,
    next_seq: u64,
    /// Refcount-0 entries, the ones the store's own eviction may drop.
    unpinned: BTreeSet<u64>,
    unpinned_bytes: usize,
    /// `(atime, seq)`: least recently used first, ties oldest first.
    by_atime: BTreeSet<(u64, u64)>,
}

impl Entries {
    pub(crate) fn len(&self) -> usize {
        self.by_seq.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.by_seq.is_empty()
    }

    pub(super) fn contains(&self, id: ImageId) -> bool {
        self.seq_of.contains_key(&id)
    }

    pub(super) fn get(&self, id: ImageId) -> Option<&ImageEntry> {
        self.by_seq
            .get(self.seq_of.get(&id)?)
            .map(|(_, entry)| entry)
    }

    /// The caller leaves `refcount` and `atime` alone and reports a
    /// byte change through [`Self::recharge`].
    pub(super) fn get_mut(&mut self, id: ImageId) -> Option<&mut ImageEntry> {
        self.by_seq
            .get_mut(self.seq_of.get(&id)?)
            .map(|(_, entry)| entry)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (ImageId, &ImageEntry)> {
        self.by_seq.values().map(|(id, entry)| (*id, entry))
    }

    /// As [`Self::get_mut`], every entry in order.
    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = (ImageId, &mut ImageEntry)> {
        self.by_seq
            .values_mut()
            .map(|(id, entry)| (*id, &mut *entry))
    }

    /// Appends as the newest entry; `id` is absent.
    pub(super) fn push(&mut self, id: ImageId, entry: ImageEntry) {
        debug_assert!(!self.contains(id), "image {} pushed twice", id.0);
        let seq = self.next_seq;
        self.next_seq += 1;
        if entry.refcount == 0 {
            self.unpinned.insert(seq);
            self.unpinned_bytes += entry.byte_len();
        }
        self.by_atime.insert((entry.atime, seq));
        self.seq_of.insert(id, seq);
        self.by_seq.insert(seq, (id, entry));
        self.debug_check();
    }

    pub(super) fn remove(&mut self, id: ImageId) -> Option<ImageEntry> {
        let seq = self.seq_of.remove(&id)?;
        let (_, entry) = self.by_seq.remove(&seq)?;
        if self.unpinned.remove(&seq) {
            self.unpinned_bytes -= entry.byte_len();
        }
        self.by_atime.remove(&(entry.atime, seq));
        self.debug_check();
        Some(entry)
    }

    pub(super) fn retain(&mut self, id: ImageId) -> bool {
        let Some(&seq) = self.seq_of.get(&id) else {
            return false;
        };
        let Some((_, entry)) = self.by_seq.get_mut(&seq) else {
            return false;
        };
        if entry.refcount == 0 {
            self.unpinned.remove(&seq);
            self.unpinned_bytes -= entry.byte_len();
        }
        entry.refcount = entry.refcount.saturating_add(1);
        self.debug_check();
        true
    }

    /// `false` when the id is unknown or the refcount is already zero.
    pub(super) fn release(&mut self, id: ImageId) -> bool {
        let Some(&seq) = self.seq_of.get(&id) else {
            return false;
        };
        let Some((_, entry)) = self.by_seq.get_mut(&seq).filter(|(_, e)| e.refcount > 0) else {
            return false;
        };
        entry.refcount -= 1;
        if entry.refcount == 0 {
            self.unpinned.insert(seq);
            self.unpinned_bytes += entry.byte_len();
        }
        self.debug_check();
        true
    }

    pub(super) fn set_atime(&mut self, id: ImageId, atime: u64) {
        let Some(&seq) = self.seq_of.get(&id) else {
            return;
        };
        if let Some((_, entry)) = self.by_seq.get_mut(&seq) {
            self.by_atime.remove(&(entry.atime, seq));
            entry.atime = atime;
            self.by_atime.insert((atime, seq));
        }
        self.debug_check();
    }

    /// Accounts an entry that [`Self::get_mut`] grew or shrank from
    /// `old` bytes to `new`.
    pub(super) fn recharge(&mut self, id: ImageId, old: usize, new: usize) {
        if let Some(seq) = self.seq_of.get(&id)
            && self.unpinned.contains(seq)
        {
            self.unpinned_bytes = self.unpinned_bytes - old + new;
        }
        self.debug_check();
    }

    /// Bytes held by refcount-0 entries, `except` aside.
    pub(super) fn unpinned_bytes(&self, except: Option<ImageId>) -> usize {
        let spared = except
            .and_then(|id| self.seq_of.get(&id))
            .filter(|seq| self.unpinned.contains(seq))
            .and_then(|seq| self.by_seq.get(seq))
            .map_or(0, |(_, entry)| entry.byte_len());
        self.unpinned_bytes - spared
    }

    /// Refcount-0 ids, oldest first.
    pub(super) fn unpinned_ids(&self) -> impl Iterator<Item = ImageId> + '_ {
        self.unpinned.iter().filter_map(|seq| self.id_at(*seq))
    }

    /// The least recently touched id, `except` aside; ties go to the
    /// oldest entry.
    pub(super) fn least_recently_used(&self, except: Option<ImageId>) -> Option<ImageId> {
        self.by_atime
            .iter()
            .filter_map(|&(_, seq)| self.id_at(seq))
            .find(|id| Some(*id) != except)
    }

    fn id_at(&self, seq: u64) -> Option<ImageId> {
        self.by_seq.get(&seq).map(|(id, _)| *id)
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    const fn debug_check(&self) {}

    /// Recomputes every index from the entries.
    #[cfg(test)]
    fn debug_check(&self) {
        let unpinned: BTreeSet<u64> = self
            .by_seq
            .iter()
            .filter(|(_, (_, entry))| entry.refcount == 0)
            .map(|(&seq, _)| seq)
            .collect();
        let unpinned_bytes: usize = unpinned
            .iter()
            .filter_map(|seq| self.by_seq.get(seq))
            .map(|(_, entry)| entry.byte_len())
            .sum();
        let by_atime: BTreeSet<(u64, u64)> = self
            .by_seq
            .iter()
            .map(|(&seq, (_, entry))| (entry.atime, seq))
            .collect();
        assert_eq!(self.seq_of.len(), self.by_seq.len(), "id index size");
        assert!(
            self.by_seq
                .iter()
                .all(|(seq, (id, _))| self.seq_of.get(id) == Some(seq)),
            "id index points elsewhere"
        );
        assert_eq!(self.unpinned, unpinned, "unpinned index");
        assert_eq!(self.unpinned_bytes, unpinned_bytes, "unpinned byte total");
        assert_eq!(self.by_atime, by_atime, "atime index");
    }
}

impl PartialEq for Entries {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl Eq for Entries {}

impl FromIterator<(ImageId, ImageEntry)> for Entries {
    fn from_iter<I: IntoIterator<Item = (ImageId, ImageEntry)>>(iter: I) -> Self {
        let mut entries = Self::default();
        for (id, entry) in iter {
            entries.remove(id);
            entries.push(id, entry);
        }
        entries
    }
}

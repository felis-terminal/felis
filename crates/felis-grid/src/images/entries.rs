//! The insertion-ordered storage behind [`super::ImageStore`], indexed
//! by what eviction asks for so a pass costs what it evicts rather than
//! what the store holds.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use foldhash::fast::FixedState;

use super::{ImageEntry, ImageId};

/// Every index here is derived from the entries' `refcount`, `atime`,
/// frame count and byte length, so those change only through this
/// type's methods; [`Self::get_mut`] and [`Self::for_each_animated_mut`]
/// lend the rest.
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
    /// Boxed: the session futures hold their stores inline, under
    /// `clippy::large_futures`.
    marks: Box<Marks>,
}

#[derive(Debug, Clone, Default)]
struct Marks {
    /// Entries a virtual placement shows, as last reported through
    /// [`Entries::set_held`].
    held: HashSet<u64, FixedState>,
    /// Refcount-0 entries outside `held`, split by
    /// [`ImageEntry::is_anonymous`] (`loose[1]` the anonymous ones).
    loose: [BTreeSet<u64>; 2],
    /// Pinned entries with more than one frame, the only ones playback
    /// advances.
    animated: BTreeSet<u64>,
    /// Every taken id as maximal runs, `start -> end` inclusive, so the
    /// lowest free id is read off the first run. Built by the first
    /// [`Entries::lowest_free_id`]: a session that never sends `I=` would
    /// otherwise pay for it on every insert and removal.
    id_runs: Option<BTreeMap<u32, u32>>,
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

    /// As [`Self::get_mut`], every pinned entry with more than one
    /// frame, in order.
    pub(super) fn for_each_animated_mut(&mut self, mut f: impl FnMut(ImageId, &mut ImageEntry)) {
        for seq in &self.marks.animated {
            if let Some((id, entry)) = self.by_seq.get_mut(seq) {
                f(*id, entry);
            }
        }
    }

    pub(super) fn animated(&self) -> impl Iterator<Item = &ImageEntry> {
        self.marks
            .animated
            .iter()
            .filter_map(|seq| self.by_seq.get(seq))
            .map(|(_, entry)| entry)
    }

    /// Appends as the newest entry; `id` is absent.
    pub(super) fn push(&mut self, id: ImageId, entry: ImageEntry) {
        debug_assert!(!self.contains(id), "image {} pushed twice", id.0);
        let seq = self.next_seq;
        self.next_seq += 1;
        if entry.refcount == 0 {
            self.unpinned.insert(seq);
            self.unpinned_bytes += entry.byte_len();
            self.marks.loose[usize::from(entry.anonymous)].insert(seq);
        } else if entry.is_animated() {
            self.marks.animated.insert(seq);
        }
        self.by_atime.insert((entry.atime, seq));
        self.add_id(id.0);
        self.seq_of.insert(id, seq);
        self.by_seq.insert(seq, (id, entry));
        self.debug_check();
    }

    pub(super) fn remove(&mut self, id: ImageId) -> Option<ImageEntry> {
        let seq = self.seq_of.remove(&id)?;
        let (_, entry) = self.by_seq.remove(&seq)?;
        if self.unpinned.remove(&seq) {
            self.unpinned_bytes -= entry.byte_len();
            self.marks.loose[usize::from(entry.anonymous)].remove(&seq);
        }
        self.marks.held.remove(&seq);
        self.marks.animated.remove(&seq);
        self.by_atime.remove(&(entry.atime, seq));
        self.remove_id(id.0);
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
            self.marks.loose[usize::from(entry.anonymous)].remove(&seq);
            if entry.is_animated() {
                self.marks.animated.insert(seq);
            }
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
            if !self.marks.held.contains(&seq) {
                self.marks.loose[usize::from(entry.anonymous)].insert(seq);
            }
            self.marks.animated.remove(&seq);
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

    /// Accounts an entry whose frames [`Self::get_mut`] changed, from
    /// `old` bytes to `new`.
    pub(super) fn recharge(&mut self, id: ImageId, old: usize, new: usize) {
        let Some(&seq) = self.seq_of.get(&id) else {
            return;
        };
        if self.unpinned.contains(&seq) {
            self.unpinned_bytes = self.unpinned_bytes - old + new;
        } else if self
            .by_seq
            .get(&seq)
            .is_some_and(|(_, entry)| entry.is_animated())
        {
            self.marks.animated.insert(seq);
        } else {
            self.marks.animated.remove(&seq);
        }
        self.debug_check();
    }

    pub(super) fn set_held(&mut self, id: ImageId, held: bool) {
        let Some(&seq) = self.seq_of.get(&id) else {
            return;
        };
        let changed = if held {
            self.marks.held.insert(seq)
        } else {
            self.marks.held.remove(&seq)
        };
        if changed
            && self.unpinned.contains(&seq)
            && let Some((_, entry)) = self.by_seq.get(&seq)
        {
            let loose = &mut self.marks.loose[usize::from(entry.anonymous)];
            if held {
                loose.remove(&seq);
            } else {
                loose.insert(seq);
            }
        }
        self.debug_check();
    }

    /// Refcount-0 ids outside [`Self::set_held`]'s set, in order.
    pub(super) fn loose_ids(&self, anonymous_only: bool) -> Vec<ImageId> {
        let mut seqs: Vec<u64> = self.marks.loose[1].iter().copied().collect();
        if !anonymous_only {
            seqs.extend(&self.marks.loose[0]);
            seqs.sort_unstable();
        }
        seqs.into_iter().filter_map(|seq| self.id_at(seq)).collect()
    }

    /// The lowest id from 1 up that no entry holds.
    pub(super) fn lowest_free_id(&mut self) -> ImageId {
        let seq_of = &self.seq_of;
        let runs = self
            .marks
            .id_runs
            .get_or_insert_with(|| id_runs(seq_of.keys()));
        match runs.range(..=1).next_back() {
            Some((_, &end)) if end >= 1 => ImageId(end.saturating_add(1)),
            _ => ImageId(1),
        }
    }

    fn add_id(&mut self, id: u32) {
        let Some(runs) = &mut self.marks.id_runs else {
            return;
        };
        let below = runs
            .range(..id)
            .next_back()
            .filter(|&(_, &end)| end.checked_add(1) == Some(id))
            .map(|(&start, _)| start);
        let above = id.checked_add(1).and_then(|next| runs.remove(&next));
        runs.insert(below.unwrap_or(id), above.unwrap_or(id));
    }

    fn remove_id(&mut self, id: u32) {
        let Some(runs) = &mut self.marks.id_runs else {
            return;
        };
        let Some((&start, &end)) = runs.range(..=id).next_back() else {
            return;
        };
        if end < id {
            return;
        }
        runs.remove(&start);
        if start < id {
            runs.insert(start, id - 1);
        }
        if id < end {
            runs.insert(id + 1, end);
        }
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
        assert!(
            self.marks
                .held
                .iter()
                .all(|seq| self.by_seq.contains_key(seq)),
            "held index outlives its entry"
        );
        for anonymous in [false, true] {
            let loose: BTreeSet<u64> = unpinned
                .iter()
                .copied()
                .filter(|seq| !self.marks.held.contains(seq))
                .filter(|seq| self.by_seq[seq].1.anonymous == anonymous)
                .collect();
            assert_eq!(
                self.marks.loose[usize::from(anonymous)],
                loose,
                "loose index"
            );
        }
        let animated: BTreeSet<u64> = self
            .by_seq
            .iter()
            .filter(|(_, (_, entry))| entry.refcount > 0 && entry.is_animated())
            .map(|(&seq, _)| seq)
            .collect();
        assert_eq!(self.marks.animated, animated, "animated index");
        if let Some(runs) = &self.marks.id_runs {
            assert_eq!(*runs, id_runs(self.seq_of.keys()), "id runs");
        }
    }
}

fn id_runs<'a>(ids: impl Iterator<Item = &'a ImageId>) -> BTreeMap<u32, u32> {
    let mut ids: Vec<u32> = ids.map(|id| id.0).collect();
    ids.sort_unstable();
    let mut runs: BTreeMap<u32, u32> = BTreeMap::new();
    for id in ids {
        match runs.last_entry() {
            Some(mut run) if run.get().checked_add(1) == Some(id) => *run.get_mut() = id,
            _ => {
                runs.insert(id, id);
            }
        }
    }
    runs
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

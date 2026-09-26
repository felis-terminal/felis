//! Per-subscriber bookkeeping for the grid's two id registries (hyperlinks and clusters).
//!
//! Hyperlink and cluster entries are transmitted to subscribers ahead of
//! referencing rows (docs/explanation/data-model/grid-and-cells.md).

use std::collections::BTreeSet;
use std::num::{NonZeroU16, NonZeroU32};

use felis_grid::{Cell, Grapheme};

/// Ids of one append-only registry sent to a subscriber.
///
/// Tracks a contiguous prefix plus out-of-order ahead entries to handle
/// non-contiguous transmission during visible-first attach bursts.
#[derive(Debug, Default)]
pub(crate) struct SentIds {
    /// Every id in `1..=prefix` has been sent.
    prefix: usize,
    /// Ids above `prefix` sent out of order.
    ahead: BTreeSet<usize>,
}

impl SentIds {
    pub(crate) fn contains(&self, id: usize) -> bool {
        id <= self.prefix || self.ahead.contains(&id)
    }

    /// Record `id` as sent, absorbing whatever run it closes into the
    /// prefix.
    pub(crate) fn mark(&mut self, id: usize) {
        if id <= self.prefix {
            return;
        }
        self.ahead.insert(id);
        while self.ahead.remove(&(self.prefix + 1)) {
            self.prefix += 1;
        }
    }

    /// Whether a table spanning `count` ids has been sent whole.
    pub(crate) const fn all_sent(&self, count: usize) -> bool {
        self.prefix >= count
    }

    /// Lowest unsent id in `1..=count`, or `None` once caught up.
    pub(crate) const fn next_unsent(&self, count: usize) -> Option<usize> {
        if self.prefix >= count {
            None
        } else {
            Some(self.prefix + 1)
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct SentRegistries {
    pub(crate) links: SentIds,
    pub(crate) clusters: SentIds,
}

impl SentRegistries {
    /// The guard that keeps the per-cell handle walk off the steady
    /// state; trivially true of a session that interned neither a
    /// hyperlink nor a cluster.
    pub(crate) const fn caught_up(&self, links: usize, clusters: usize) -> bool {
        self.links.all_sent(links) && self.clusters.all_sent(clusters)
    }
}

/// Registry handles a set of rows references, collected alongside the
/// row encode so a fan-out cycle walks each row's cells once and
/// [`super::streaming::RowEncodeCache`] can hand the same answer to
/// every subscriber.
#[derive(Debug, Clone, Default)]
pub(crate) struct RowHandles {
    pub(crate) links: Vec<NonZeroU16>,
    pub(crate) clusters: Vec<NonZeroU32>,
}

impl RowHandles {
    /// Adjacent repeats are dropped (a linked run, a mark repeated across
    /// a row); non-adjacent repeats cost one set lookup each at emission.
    pub(crate) fn extend_from_cells(&mut self, cells: &[Cell]) {
        for cell in cells {
            if let Some(link) = cell.link
                && self.links.last() != Some(&link)
            {
                self.links.push(link);
            }
            if let Grapheme::Cluster(id) = cell.grapheme
                && self.clusters.last() != Some(&id)
            {
                self.clusters.push(id);
            }
        }
    }

    pub(crate) fn extend_from(&mut self, other: &Self) {
        self.links.extend_from_slice(&other.links);
        self.clusters.extend_from_slice(&other.clusters);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: std::ops::RangeInclusive<usize> = 1..=20;

    proptest::proptest! {
        /// Marks arrive out of order during a visible-first attach burst
        /// and repeat every browse cycle, so the answers must depend on
        /// the set of marked ids alone.
        #[test]
        fn sent_ids_answers_for_the_set_of_ids_marked_so_far(
            marks in proptest::collection::vec(IDS, 0..32),
        ) {
            let mut sent = SentIds::default();
            let mut marked = BTreeSet::new();
            for id in marks {
                sent.mark(id);
                marked.insert(id);
                for count in IDS {
                    proptest::prop_assert_eq!(sent.contains(count), marked.contains(&count));
                    proptest::prop_assert_eq!(
                        sent.next_unsent(count),
                        (1..=count).find(|id| !marked.contains(id)),
                    );
                    proptest::prop_assert_eq!(
                        sent.all_sent(count),
                        (1..=count).all(|id| marked.contains(&id)),
                    );
                }
                // The exception set must not keep what the prefix has
                // absorbed, or an attach burst grows it without bound.
                proptest::prop_assert_eq!(
                    sent.prefix,
                    (1..=*IDS.end() + 1)
                        .take_while(|id| marked.contains(id))
                        .count(),
                );
                proptest::prop_assert!(sent.ahead.iter().all(|id| *id > sent.prefix));
            }
        }
    }

    /// The collector wants each referenced id once per run, not once
    /// per cell: an OSC 8 anchor spans a whole row and a combining mark
    /// repeats down a column.
    #[test]
    fn collecting_handles_collapses_a_run_to_one_entry() {
        let link = NonZeroU16::new(7).expect("nonzero");
        let cluster = NonZeroU32::new(9).expect("nonzero");
        let cells: Vec<Cell> = (0..4)
            .map(|_| Cell {
                grapheme: Grapheme::Cluster(cluster),
                link: Some(link),
                ..Cell::default()
            })
            .collect();
        let mut handles = RowHandles::default();
        handles.extend_from_cells(&cells);
        assert_eq!(handles.links, vec![link]);
        assert_eq!(handles.clusters, vec![cluster]);
    }

    /// The composer skips the per-cell walk on `caught_up`, so a hole in
    /// either registry must keep it false.
    #[test]
    fn caught_up_needs_both_registries_whole() {
        let mut sent = SentRegistries::default();
        assert!(sent.caught_up(0, 0), "nothing interned, nothing owed");
        sent.links.mark(1);
        assert!(sent.caught_up(1, 0));
        assert!(!sent.caught_up(1, 1), "the cluster table is still owed");
        sent.clusters.mark(2);
        assert!(!sent.caught_up(1, 2), "cluster 1 is still a hole");
        sent.clusters.mark(1);
        assert!(sent.caught_up(1, 2));
    }
}

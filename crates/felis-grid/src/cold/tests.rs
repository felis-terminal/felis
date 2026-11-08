use std::collections::VecDeque;

use proptest::prelude::*;

use super::*;
use crate::Grapheme;

fn row(seed: usize, len: usize) -> Vec<Cell> {
    (0..len)
        .map(|i| Cell {
            grapheme: Grapheme::Ascii(b'a' + ((seed + i) % 26) as u8),
            ..Cell::default()
        })
        .collect()
}

fn contents(store: &ColdStore) -> Vec<(Vec<Cell>, bool)> {
    (0..store.len())
        .map(|i| (store.row(i).unwrap().to_vec(), store.soft_wrap(i).unwrap()))
        .collect()
}

#[test]
fn rows_read_back_in_fifo_order_with_their_wrap_bits() {
    let mut store = ColdStore::default();
    store.push_back(&row(0, 5), true, 80);
    store.push_back(&[], false, 80);
    store.push_back(&row(7, 80), false, 80);
    assert_eq!(
        contents(&store),
        vec![(row(0, 5), true), (vec![], false), (row(7, 80), false)]
    );
    store.pop_front();
    assert_eq!(store.row(0), Some(&[][..]));
    assert_eq!(store.row(2), None);
}

#[test]
fn a_row_that_does_not_fit_the_chunk_tail_starts_the_next_chunk() {
    let mut store = ColdStore::default();
    let size = 1usize << chunk_shift(80);
    let fill = size / 80;
    for i in 0..=fill {
        store.push_back(&row(i, 80), false, 80);
    }
    assert_eq!(store.chunks.len(), 2);
    assert_eq!(store.row(fill), Some(&row(fill, 80)[..]));
}

#[test]
fn eviction_recycles_a_chunk_once_no_row_reaches_it() {
    let mut store = ColdStore::default();
    let per_chunk = (1usize << chunk_shift(80)) / 80;
    for i in 0..3 * per_chunk {
        store.push_back(&row(i, 80), false, 80);
    }
    assert_eq!(store.chunks.len(), 3);
    for _ in 0..per_chunk {
        store.pop_front();
    }
    assert_eq!(store.chunks.len(), 2, "the drained front chunk is gone");
    assert!(store.spare.is_some(), "and kept for the next fill");
    let before = store.heap_bytes();
    for i in 0..per_chunk {
        store.push_back(&row(i, 80), false, 80);
        store.pop_front();
    }
    assert_eq!(
        store.heap_bytes(),
        before,
        "a steady cycle allocates nothing"
    );
}

#[test]
fn rows_are_packed_at_their_length() {
    let mut store = ColdStore::default();
    for i in 0..1000 {
        store.push_back(&row(i, 10), false, 200);
    }
    let cells = 1000 * 10;
    assert!(
        store.heap_bytes() < 2 * cells * size_of::<Cell>(),
        "{} bytes for {cells} cells",
        store.heap_bytes()
    );
}

proptest! {
    /// The store against a deque of owned rows, through pushes of any
    /// width up to the ring's and evictions down to empty and back; the
    /// store indexes at most twice the most rows it held at once.
    #[test]
    fn the_store_matches_a_deque_of_rows(
        cols in 1usize..300,
        ops in prop::collection::vec((any::<bool>(), 0usize..300, any::<bool>()), 0..400),
    ) {
        let mut store = ColdStore::default();
        let mut model: VecDeque<(Vec<Cell>, bool)> = VecDeque::new();
        let mut peak = 0;
        for (i, (push, len, wrap)) in ops.into_iter().enumerate() {
            if push {
                let cells = row(i, len % (cols + 1));
                store.push_back(&cells, wrap, cols);
                model.push_back((cells, wrap));
            } else {
                store.pop_front();
                model.pop_front();
            }
            peak = peak.max(model.len());
            prop_assert!(store.rows.len() <= 2 * peak);
            prop_assert_eq!(contents(&store), model.iter().cloned().collect::<Vec<_>>());
            let mut swept = Vec::new();
            store.for_each_row_mut(|cells| swept.push(cells.to_vec()));
            prop_assert_eq!(swept, model.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>());
        }
    }
}

//! Property: arbitrary insert / retain / release / delete sequences
//! preserve [`ImageStore`] invariants (`docs/reference/protocols/kitty-graphics.md`
//! "Lifecycle", `docs/explanation/security-model.md` "Kitty graphics").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::images::{Frame, ImageEntry, ImageFormat, ImageId, ImageStore};
use proptest::prelude::*;

const IDS: u32 = 6;

#[derive(Debug, Clone)]
enum Op {
    Insert { id: u32, size: usize },
    Retain { id: u32 },
    Release { id: u32 },
    Delete { id: u32 },
    PushFrame { id: u32, size: usize },
    ReplaceFrame { id: u32, idx: usize, size: usize },
    RemoveFrame { id: u32, idx: usize },
}

fn op_strategy(max_size: usize) -> impl Strategy<Value = Op> {
    let id_strategy = 0u32..IDS;
    let size_strategy = 1usize..=max_size;
    let idx_strategy = 0usize..4;
    prop_oneof![
        (id_strategy.clone(), size_strategy.clone()).prop_map(|(id, size)| Op::Insert { id, size }),
        id_strategy.clone().prop_map(|id| Op::Retain { id }),
        id_strategy.clone().prop_map(|id| Op::Release { id }),
        id_strategy.clone().prop_map(|id| Op::Delete { id }),
        (id_strategy.clone(), size_strategy.clone())
            .prop_map(|(id, size)| Op::PushFrame { id, size }),
        (id_strategy.clone(), idx_strategy.clone(), size_strategy)
            .prop_map(|(id, idx, size)| Op::ReplaceFrame { id, idx, size }),
        (id_strategy, idx_strategy).prop_map(|(id, idx)| Op::RemoveFrame { id, idx }),
    ]
}

fn entry(size: usize) -> ImageEntry {
    ImageEntry::new(1, 1, ImageFormat::Rgb24, vec![0u8; size])
}

fn frame(size: usize) -> Frame {
    Frame {
        pixels: vec![0u8; size].into(),
        gap_ms: 40,
    }
}

fn recomputed_total(store: &ImageStore) -> usize {
    (0..IDS)
        .filter_map(|id| store.get(ImageId(id)))
        .map(ImageEntry::byte_len)
        .sum()
}

proptest! {
    #[test]
    fn invariants_hold_across_arbitrary_op_sequences(
        ops in proptest::collection::vec(op_strategy(40), 0..64),
        // Below one entry's fixed overhead no insert succeeds and the
        // property is vacuous.
        cap in 256usize..4096,
    ) {
        let mut store = ImageStore::new(cap);
        for op in &ops {
            match *op {
                Op::Insert { id, size } => {
                    drop(store.insert(ImageId(id), entry(size)));
                }
                Op::Retain { id } => {
                    let _ = store.retain(ImageId(id));
                }
                Op::Release { id } => {
                    let _ = store.release(ImageId(id));
                }
                Op::Delete { id } => {
                    drop(store.delete(ImageId(id)));
                }
                Op::PushFrame { id, size } => {
                    drop(store.push_frame(ImageId(id), frame(size)));
                }
                Op::ReplaceFrame { id, idx, size } => {
                    drop(store.replace_frame(ImageId(id), idx, frame(size)));
                }
                Op::RemoveFrame { id, idx } => {
                    let _ = store.remove_frame(ImageId(id), idx);
                }
            }
            prop_assert!(
                store.bytes_used() <= store.bytes_cap(),
                "bytes_used {} exceeded cap {}",
                store.bytes_used(),
                store.bytes_cap(),
            );
            prop_assert_eq!(
                store.bytes_used(),
                recomputed_total(&store),
                "cached byte total drifted from the live entries",
            );
        }
    }

    #[test]
    fn pinned_entries_never_evicted(
        size in 1usize..40,
        push_sizes in proptest::collection::vec(1usize..40, 0..20),
    ) {
        // Derived from `byte_len` rather than a literal: the entry's fixed
        // overhead dwarfs these pixel counts, so a literal cap fits nothing.
        let cap = 4 * entry(40).byte_len();
        let mut store = ImageStore::new(cap);
        prop_assume!(store.insert(ImageId(0), entry(size)).is_ok());
        prop_assert!(store.retain(ImageId(0)));

        for (i, sz) in push_sizes.iter().copied().enumerate() {
            drop(store.insert(ImageId(i as u32 + 1), entry(sz)));
            prop_assert!(
                store.get(ImageId(0)).is_some(),
                "pinned image disappeared after pushing entry of size {sz}",
            );
        }
    }
}

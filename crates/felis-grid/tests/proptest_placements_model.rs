//! `Placements` against a model that scans plain vectors: the keyed
//! table must give the same answers, keep the same order, and evict the
//! same placement past its cap
//! (`docs/reference/protocols/kitty-graphics.md` "Limits").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::num::NonZeroU16;

use felis_grid::images::{
    AnyPlacement, CellPos, Displaced, Extent, ImageId, Placement, PlacementId, Placements,
    VirtualPlacement,
};
use proptest::prelude::*;

const CAP: usize = 4;

#[derive(Debug, Clone)]
enum Op {
    Put {
        image: u32,
        id: Option<u32>,
        row: i32,
    },
    PutVirtual {
        image: u32,
        id: Option<u32>,
        cols: u16,
    },
    Remove {
        image: u32,
        id: u32,
    },
    RemoveVirtual {
        image: u32,
        id: u32,
    },
    Rekey {
        image: u32,
        id: u32,
    },
    DeleteImage(u32),
    ShiftUp(u32),
}

fn op() -> impl Strategy<Value = Op> {
    let image = 1u32..4;
    let id = proptest::option::of(1u32..4);
    prop_oneof![
        (image.clone(), id.clone(), -2i32..4).prop_map(|(image, id, row)| Op::Put {
            image,
            id,
            row
        }),
        (image.clone(), id, 1u16..4).prop_map(|(image, id, cols)| Op::PutVirtual {
            image,
            id,
            cols
        }),
        (image.clone(), 1u32..4).prop_map(|(image, id)| Op::Remove { image, id }),
        (image.clone(), 1u32..4).prop_map(|(image, id)| Op::RemoveVirtual { image, id }),
        // Rekeys only ever hit anonymous ids, which count down from the top.
        (image.clone(), 0u32..4).prop_map(|(image, n)| Op::Rekey {
            image,
            id: u32::MAX - n
        }),
        image.prop_map(Op::DeleteImage),
        (0u32..3).prop_map(Op::ShiftUp),
    ]
}

/// The pre-index table: linear scans, one sequence number per insertion.
#[derive(Default)]
struct Model {
    entries: Vec<(u64, Placement)>,
    virtuals: Vec<(u64, VirtualPlacement)>,
    seq: u64,
}

impl Model {
    const fn next(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn evict(&mut self) -> Option<AnyPlacement> {
        if self.entries.len() + self.virtuals.len() <= CAP {
            return None;
        }
        let direct = self.entries.first().map(|(s, _)| *s);
        let virt = self.virtuals.first().map(|(s, _)| *s);
        if virt.is_some_and(|v| direct.is_none_or(|d| v < d)) {
            Some(AnyPlacement::Virtual(self.virtuals.remove(0).1))
        } else {
            Some(AnyPlacement::Anchored(self.entries.remove(0).1))
        }
    }

    fn upsert(&mut self, p: Placement) -> Option<Displaced> {
        if let Some(slot) = self.entries.iter_mut().find(|(_, q)| {
            !p.anonymous
                && !q.anonymous
                && q.image_id == p.image_id
                && q.placement_id == p.placement_id
        }) {
            return Some(Displaced::Replaced(std::mem::replace(&mut slot.1, p)));
        }
        let seq = self.next();
        self.entries.push((seq, p));
        self.evict().map(Displaced::Evicted)
    }

    fn upsert_virtual(&mut self, v: VirtualPlacement) -> Option<AnyPlacement> {
        if let Some(slot) = self.virtuals.iter_mut().find(|(_, w)| {
            !v.anonymous
                && !w.anonymous
                && w.image_id == v.image_id
                && w.placement_id == v.placement_id
        }) {
            slot.1 = v;
            return None;
        }
        let seq = self.next();
        self.virtuals.push((seq, v));
        self.evict()
    }
}

const fn placement(image: u32, id: PlacementId, anonymous: bool, row: i32) -> Placement {
    Placement {
        image_id: ImageId(image),
        placement_id: Some(id),
        anchor: CellPos { row, col: 1 },
        cols: Extent::Requested(NonZeroU16::MIN),
        rows: Extent::Requested(NonZeroU16::MIN),
        source: None,
        z_index: 0,
        no_cursor_move: false,
        quiet: 0,
        anonymous,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn the_keyed_table_answers_as_the_scanning_one_does(
        ops in proptest::collection::vec(op(), 0..40),
    ) {
        let mut table = Placements::with_cap(CAP);
        let mut model = Model::default();
        for op in &ops {
            match *op {
                Op::Put { image, id, row } => {
                    let pid = id.map_or_else(|| table.allocate_anonymous_id(ImageId(image)), PlacementId);
                    if let Some(moved) = id.and_then(|_| table.rekey_anonymous(ImageId(image), pid)) {
                        model_rekey(&mut model, ImageId(image), pid, &moved);
                    }
                    let p = placement(image, pid, id.is_none(), row);
                    prop_assert_eq!(table.upsert(p.clone()), model.upsert(p), "{:?}", op);
                }
                Op::PutVirtual { image, id, cols } => {
                    let pid = id.map_or_else(|| table.allocate_anonymous_id(ImageId(image)), PlacementId);
                    if let Some(moved) = id.and_then(|_| table.rekey_anonymous(ImageId(image), pid)) {
                        model_rekey(&mut model, ImageId(image), pid, &moved);
                    }
                    let v = VirtualPlacement {
                        image_id: ImageId(image),
                        placement_id: Some(pid),
                        anonymous: id.is_none(),
                        cols,
                        rows: 1,
                        z_index: 0,
                    };
                    prop_assert_eq!(table.upsert_virtual(v), model.upsert_virtual(v), "{:?}", op);
                }
                Op::Remove { image, id } => {
                    let key = (ImageId(image), Some(PlacementId(id)));
                    let idx = model.entries.iter().position(|(_, p)| !p.anonymous && (p.image_id, p.placement_id) == key);
                    let want = idx.map(|i| model.entries.remove(i).1);
                    prop_assert_eq!(table.remove(key.0, key.1), want, "{:?}", op);
                }
                Op::RemoveVirtual { image, id } => {
                    let idx = model.virtuals.iter().position(|(_, v)| {
                        !v.anonymous && v.image_id == ImageId(image) && v.placement_id == Some(PlacementId(id))
                    });
                    let want = idx.map(|i| model.virtuals.remove(i).1);
                    prop_assert_eq!(table.remove_virtual_placement(ImageId(image), PlacementId(id)), want, "{:?}", op);
                }
                Op::Rekey { image, id } => {
                    if let Some(moved) = table.rekey_anonymous(ImageId(image), PlacementId(id)) {
                        model_rekey(&mut model, ImageId(image), PlacementId(id), &moved);
                    } else {
                        prop_assert!(!model.entries.iter().any(|(_, p)| p.anonymous && p.image_id == ImageId(image) && p.placement_id == Some(PlacementId(id))));
                        prop_assert!(!model.virtuals.iter().any(|(_, v)| v.anonymous && v.image_id == ImageId(image) && v.placement_id == Some(PlacementId(id))));
                    }
                }
                Op::DeleteImage(image) => {
                    let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut model.entries)
                        .into_iter()
                        .partition(|(_, p)| p.image_id == ImageId(image));
                    model.entries = kept;
                    let gone: Vec<_> = gone.into_iter().map(|(_, p)| p).collect();
                    prop_assert_eq!(table.delete_image(ImageId(image)), gone, "{:?}", op);
                }
                Op::ShiftUp(lines) => {
                    let mut gone = Vec::new();
                    let lines = i32::try_from(lines).unwrap();
                    model.entries.retain_mut(|(_, p)| {
                        let row = p.anchor.row - lines;
                        if lines > 0 && row < -1 {
                            gone.push(p.clone());
                            return false;
                        }
                        p.anchor.row = row;
                        true
                    });
                    prop_assert_eq!(table.shift_up(lines.unsigned_abs(), 2), gone, "{:?}", op);
                }
            }
            let entries: Vec<_> = model.entries.iter().map(|(_, p)| p.clone()).collect();
            prop_assert_eq!(table.iter().cloned().collect::<Vec<_>>(), entries, "after {:?}", op);
            let virtuals: Vec<_> = model.virtuals.iter().map(|(_, v)| *v).collect();
            prop_assert_eq!(table.iter_virtual().copied().collect::<Vec<_>>(), virtuals, "after {:?}", op);
            prop_assert!(table.len() + table.iter_virtual().count() <= CAP);
            for image in (1..4).map(ImageId) {
                let mine: Vec<_> = model.entries.iter().map(|(_, p)| p).filter(|p| p.image_id == image).cloned().collect();
                prop_assert_eq!(table.for_image(image).cloned().collect::<Vec<_>>(), mine, "after {:?}", op);
                let shown = table.for_image(image).next().is_some() || model.virtuals.iter().any(|(_, v)| v.image_id == image);
                prop_assert_eq!(table.references(image), shown, "after {:?}", op);
            }
        }
    }
}

/// Mirrors a rekey the table reported: the placement leaves its slot and
/// rejoins as the newest under its new id.
fn model_rekey(model: &mut Model, image: ImageId, from: PlacementId, moved: &AnyPlacement) {
    let seq = model.next();
    match moved {
        AnyPlacement::Anchored(p) => {
            let idx = model
                .entries
                .iter()
                .position(|(_, q)| {
                    q.anonymous && q.image_id == image && q.placement_id == Some(from)
                })
                .expect("the model holds the anonymous placement the table moved");
            model.entries.remove(idx);
            model.entries.push((seq, p.clone()));
        }
        AnyPlacement::Virtual(v) => {
            let idx = model
                .virtuals
                .iter()
                .position(|(_, w)| {
                    w.anonymous && w.image_id == image && w.placement_id == Some(from)
                })
                .expect("the model holds the anonymous placement the table moved");
            model.virtuals.remove(idx);
            model.virtuals.push((seq, *v));
        }
    }
}

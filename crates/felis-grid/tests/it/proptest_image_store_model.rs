//! Property: [`ImageStore`] evicts, frees, numbers and animates exactly
//! what a whole-store scan would. The model below keeps every entry in
//! one insertion-ordered list and answers each question by walking all
//! of it, which is the store's contract stated without its indexes
//! (`docs/explanation/data-model/image-store.md` "Limits").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::images::{
    AnimationMode, Evicted, Frame, FrameError, ImageEntry, ImageFormat, ImageId, ImageStore,
    InsertError, MAX_IMAGE_FRAMES,
};
use proptest::prelude::*;

const IDS: u32 = 8;

#[derive(Debug, Clone)]
enum Op {
    Insert {
        id: u32,
        size: usize,
        anonymous: bool,
    },
    Hold {
        id: u32,
        held: bool,
    },
    Animate {
        id: u32,
        mode: AnimationMode,
    },
    Tick {
        ms: u64,
    },
    LowestFree,
    Touch {
        id: u32,
    },
    Retain {
        id: u32,
    },
    Release {
        id: u32,
    },
    Delete {
        id: u32,
    },
    PushFrame {
        id: u32,
        size: usize,
    },
    ReplaceFrame {
        id: u32,
        idx: usize,
        size: usize,
    },
    RemoveFrame {
        id: u32,
        idx: usize,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let id = 0u32..IDS;
    let size = 1usize..=48;
    let idx = 0usize..3;
    prop_oneof![
        3 => (id.clone(), size.clone(), any::<bool>()).prop_map(|(id, size, anonymous)| Op::Insert { id, size, anonymous }),
        1 => (id.clone(), any::<bool>()).prop_map(|(id, held)| Op::Hold { id, held }),
        1 => (id.clone(), prop_oneof![
            Just(AnimationMode::Stopped),
            Just(AnimationMode::Loading),
            Just(AnimationMode::Running),
        ]).prop_map(|(id, mode)| Op::Animate { id, mode }),
        2 => (0u64..100).prop_map(|ms| Op::Tick { ms }),
        1 => Just(Op::LowestFree),
        2 => id.clone().prop_map(|id| Op::Touch { id }),
        2 => id.clone().prop_map(|id| Op::Retain { id }),
        2 => id.clone().prop_map(|id| Op::Release { id }),
        1 => id.clone().prop_map(|id| Op::Delete { id }),
        1 => (id.clone(), size.clone()).prop_map(|(id, size)| Op::PushFrame { id, size }),
        1 => (id.clone(), idx.clone(), size).prop_map(|(id, idx, size)| Op::ReplaceFrame { id, idx, size }),
        1 => (id, idx).prop_map(|(id, idx)| Op::RemoveFrame { id, idx }),
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

fn frame_bytes(size: usize) -> usize {
    frame(size).byte_len()
}

/// What an entry costs beyond its frames.
fn overhead() -> usize {
    entry(0).byte_len() - frame_bytes(0)
}

#[derive(Debug, Clone)]
struct ModelEntry {
    id: ImageId,
    frames: Vec<usize>,
    refcount: u32,
    atime: u64,
    anonymous: bool,
    held: bool,
}

impl ModelEntry {
    fn bytes(&self) -> usize {
        overhead() + self.frames.iter().map(|&s| frame_bytes(s)).sum::<usize>()
    }
}

#[derive(Debug)]
struct Model {
    entries: Vec<ModelEntry>,
    total: usize,
    cap: usize,
    clock: u64,
}

impl Model {
    fn position(&self, id: ImageId) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    fn entry_mut(&mut self, id: ImageId) -> Option<&mut ModelEntry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    fn freeable(&self, except: Option<ImageId>) -> usize {
        self.entries
            .iter()
            .filter(|e| e.refcount == 0 && Some(e.id) != except)
            .map(ModelEntry::bytes)
            .sum()
    }

    fn evict_to_fit(
        &mut self,
        needed: usize,
        except: Option<ImageId>,
    ) -> Result<Evicted, InsertError> {
        if self.total - self.freeable(except) + needed > self.cap {
            return Err(InsertError::OverCapacity {
                wanted: self.total + needed,
                cap: self.cap,
            });
        }
        let mut evicted = Evicted::new();
        while self.total + needed > self.cap {
            let Some(at) = self
                .entries
                .iter()
                .position(|e| e.refcount == 0 && Some(e.id) != except)
            else {
                break;
            };
            let gone = self.entries.remove(at);
            self.total -= gone.bytes();
            evicted.push(gone.id);
        }
        Ok(evicted)
    }

    fn touch(&mut self, id: ImageId) {
        if self.position(id).is_some() {
            self.clock += 1;
            let clock = self.clock;
            self.entry_mut(id).unwrap().atime = clock;
        }
    }

    fn insert(
        &mut self,
        id: ImageId,
        size: usize,
        anonymous: bool,
    ) -> Result<Evicted, InsertError> {
        let needed = overhead() + frame_bytes(size);
        if needed > self.cap {
            return Err(InsertError::OverCapacity {
                wanted: needed,
                cap: self.cap,
            });
        }
        let displaced = self.position(id).map_or(0, |at| self.entries[at].bytes());
        if self.total - (self.freeable(Some(id)) + displaced) + needed > self.cap {
            return Err(InsertError::OverCapacity {
                wanted: self.total - displaced + needed,
                cap: self.cap,
            });
        }
        if let Some(at) = self.position(id) {
            let old = self.entries.remove(at);
            self.total -= old.bytes();
        }
        let evicted = self.evict_to_fit(needed, None)?;
        self.total += needed;
        self.entries.push(ModelEntry {
            id,
            frames: vec![size],
            refcount: 0,
            atime: 0,
            anonymous,
            held: false,
        });
        self.touch(id);
        Ok(evicted)
    }

    fn push_frame(&mut self, id: ImageId, size: usize) -> Result<(usize, Evicted), FrameError> {
        let held = self
            .entry_mut(id)
            .ok_or(FrameError::NoSuchImage)?
            .frames
            .len();
        if held >= MAX_IMAGE_FRAMES {
            return Err(FrameError::TooManyFrames {
                cap: MAX_IMAGE_FRAMES,
            });
        }
        let needed = frame_bytes(size);
        let evicted = self
            .evict_to_fit(needed, Some(id))
            .map_err(frame_capacity_error)?;
        let e = self.entry_mut(id).unwrap();
        e.frames.push(size);
        let index = e.frames.len() - 1;
        self.total += needed;
        Ok((index, evicted))
    }

    fn replace_frame(
        &mut self,
        id: ImageId,
        idx: usize,
        size: usize,
    ) -> Result<Evicted, FrameError> {
        let e = self.entry_mut(id).ok_or(FrameError::NoSuchImage)?;
        let old_len = frame_bytes(*e.frames.get(idx).ok_or(FrameError::NoSuchFrame)?);
        let new_len = frame_bytes(size);
        let evicted = if new_len > old_len {
            self.evict_to_fit(new_len - old_len, Some(id))
                .map_err(frame_capacity_error)?
        } else {
            Evicted::new()
        };
        self.entry_mut(id).unwrap().frames[idx] = size;
        self.total = self.total - old_len + new_len;
        Ok(evicted)
    }

    fn remove_frame(&mut self, id: ImageId, idx: usize) -> Result<usize, FrameError> {
        let e = self.entry_mut(id).ok_or(FrameError::NoSuchImage)?;
        if idx >= e.frames.len() || e.frames.len() == 1 {
            return Err(FrameError::NoSuchFrame);
        }
        let freed = frame_bytes(e.frames.remove(idx));
        self.total -= freed;
        Ok(freed)
    }

    fn least_recently_used(&self, except: Option<ImageId>) -> Option<ImageId> {
        self.entries
            .iter()
            .filter(|e| Some(e.id) != except)
            .min_by_key(|e| e.atime)
            .map(|e| e.id)
    }

    fn lowest_free_id(&self) -> ImageId {
        (1..=u32::MAX)
            .map(ImageId)
            .find(|&id| self.position(id).is_none())
            .unwrap()
    }

    fn loose_ids(&self, anonymous_only: bool) -> Vec<ImageId> {
        self.entries
            .iter()
            .filter(|e| e.refcount == 0 && !e.held && (e.anonymous || !anonymous_only))
            .map(|e| e.id)
            .collect()
    }

    /// Each entry as the store holds it, in store order.
    fn held<'s>(&self, store: &'s ImageStore) -> Vec<(&ModelEntry, &'s ImageEntry)> {
        self.entries
            .iter()
            .map(|e| (e, store.get(e.id).unwrap()))
            .collect()
    }
}

const fn frame_capacity_error(err: InsertError) -> FrameError {
    match err {
        InsertError::OverCapacity { wanted, cap } => FrameError::OverCapacity { wanted, cap },
        _ => unreachable!(),
    }
}

proptest! {
    #[test]
    fn the_store_evicts_what_a_whole_store_scan_would(
        ops in proptest::collection::vec(op(), 0..160),
        cap_entries in 2usize..8,
    ) {
        let cap = cap_entries * (overhead() + frame_bytes(24));
        let mut store = ImageStore::new(cap);
        let mut model = Model { entries: Vec::new(), total: 0, cap, clock: 0 };
        let mut now = 0;
        for op in &ops {
            match *op {
                Op::Insert { id, size, anonymous } => {
                    let e = if anonymous { entry(size).into_anonymous() } else { entry(size) };
                    prop_assert_eq!(store.insert(ImageId(id), e), model.insert(ImageId(id), size, anonymous), "{:?}", op);
                }
                Op::Hold { id, held } => {
                    store.set_virtually_held(ImageId(id), held);
                    if let Some(e) = model.entry_mut(ImageId(id)) {
                        e.held = held;
                    }
                }
                Op::Animate { id, mode } => {
                    let found = store.animation_control(ImageId(id)).map(|mut c| c.set_mode(mode)).is_some();
                    prop_assert_eq!(found, model.position(ImageId(id)).is_some());
                }
                Op::LowestFree => {
                    prop_assert_eq!(store.lowest_free_id(), model.lowest_free_id());
                }
                Op::Tick { ms } => {
                    now += ms;
                    let mut want = Vec::new();
                    let mut after = Vec::new();
                    for (e, entry) in model.held(&store) {
                        let mut entry = entry.clone();
                        if e.refcount > 0 && entry.is_animated() && let Some(idx) = entry.advance(now) {
                            want.push((e.id, idx));
                        }
                        after.push((e.id, entry));
                    }
                    prop_assert_eq!(store.advance_animations(now), want, "{:?}", op);
                    for (id, entry) in &after {
                        prop_assert_eq!(store.get(*id), Some(entry), "{:?}", op);
                    }
                }
                Op::Touch { id } => {
                    store.touch(ImageId(id));
                    model.touch(ImageId(id));
                }
                Op::Retain { id } => {
                    let held = model.entry_mut(ImageId(id)).map(|e| e.refcount += 1).is_some();
                    prop_assert_eq!(store.retain(ImageId(id)), held);
                }
                Op::Release { id } => {
                    let released = model
                        .entry_mut(ImageId(id))
                        .filter(|e| e.refcount > 0)
                        .map(|e| e.refcount -= 1)
                        .is_some();
                    prop_assert_eq!(store.release(ImageId(id)), released);
                }
                Op::Delete { id } => {
                    let gone = model.position(ImageId(id)).map(|at| model.entries.remove(at));
                    if let Some(gone) = &gone {
                        model.total -= gone.bytes();
                    }
                    prop_assert_eq!(store.delete(ImageId(id)).is_some(), gone.is_some());
                }
                Op::PushFrame { id, size } => {
                    let got = store.push_frame(ImageId(id), frame(size)).map(|p| (p.index, p.evicted));
                    prop_assert_eq!(got, model.push_frame(ImageId(id), size), "{:?}", op);
                }
                Op::ReplaceFrame { id, idx, size } => {
                    prop_assert_eq!(
                        store.replace_frame(ImageId(id), idx, frame(size)),
                        model.replace_frame(ImageId(id), idx, size),
                        "{:?}", op
                    );
                }
                Op::RemoveFrame { id, idx } => {
                    prop_assert_eq!(store.remove_frame(ImageId(id), idx), model.remove_frame(ImageId(id), idx));
                }
            }
            prop_assert_eq!(store.bytes_used(), model.total, "byte total after {:?}", op);
            let ids: Vec<ImageId> = model.entries.iter().map(|e| e.id).collect();
            prop_assert_eq!(store.iter_ids().collect::<Vec<_>>(), ids, "order after {:?}", op);
            let unpinned: Vec<ImageId> = model.entries.iter().filter(|e| e.refcount == 0).map(|e| e.id).collect();
            prop_assert_eq!(store.iter_unpinned_ids().collect::<Vec<_>>(), unpinned, "unpinned after {:?}", op);
            for except in std::iter::once(None).chain((0..IDS).map(|id| Some(ImageId(id)))) {
                prop_assert_eq!(
                    store.least_recently_used(except),
                    model.least_recently_used(except),
                    "LRU except {:?} after {:?}", except, op
                );
            }
            for anonymous_only in [false, true] {
                prop_assert_eq!(store.loose_ids(anonymous_only), model.loose_ids(anonymous_only), "loose after {:?}", op);
            }
            let due = model
                .held(&store)
                .into_iter()
                .filter(|(e, entry)| e.refcount > 0 && entry.is_animatable())
                .map(|(_, entry)| entry.next_due_ms().unwrap_or(now))
                .min();
            prop_assert_eq!(store.next_animation_due_ms(now), due, "due after {:?}", op);
            for e in &model.entries {
                let held = store.get(e.id).unwrap();
                prop_assert_eq!((held.refcount(), held.atime(), held.byte_len()), (e.refcount, e.atime, e.bytes()));
            }
        }
    }
}

//! Daemon-side image store for the Kitty graphics protocol: decoded
//! pixel buffers under a per-session byte cap
//! (`docs/explanation/security-model.md` "Kitty graphics").

use std::num::NonZeroU16;

use bytes::Bytes;
use indexmap::IndexMap;

mod table;
use table::{Keyed, Table};

pub use felis_protocol::{
    ImageId, PlacementId,
    messages::{ImageFormat, MAX_IMAGE_FRAMES, SourceRect},
};

/// One animation frame, already coalesced: the daemon composites the
/// Kitty base frame (`c=`) and transmitted rectangle into a complete
/// `width × height` image before it reaches the store
/// (`docs/reference/protocols/kitty-graphics.md` "Animation").
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct Frame {
    #[cfg_attr(feature = "state-dump", serde(with = "crate::state::shared_b64"))]
    /// `pixels.len()` must equal `width * height * bytes_per_pixel(format)`
    /// for the owning entry.
    pub pixels: Bytes,
    /// Display duration in milliseconds. `0` is gapless: a compositing
    /// base that playback skips instantly (Kitty `z<0`). The daemon
    /// applies the 40 ms default (`z=0`/absent) before inserting.
    pub gap_ms: u32,
}

impl Frame {
    #[must_use]
    pub const fn byte_len(&self) -> usize {
        size_of::<Self>() + self.pixels.len()
    }
}

/// Playback state (`a=a` `s=`). A still is never advanced regardless
/// of mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub enum AnimationMode {
    /// `s=1`.
    Stopped,
    /// `s=2`: advancing, but halts on the last frame to await more
    /// (Kitty "loading"). The default, so streamed frames auto-play.
    Loading,
    /// `s=3`: looping, bounded by `ImageEntry::set_max_loops`.
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct ImageEntry {
    pub width: u32,
    pub height: u32,
    /// Every frame shares it.
    pub format: ImageFormat,
    /// `frames[0]` is the root (Kitty frame number 1). Always non-empty.
    frames: Vec<Frame>,
    current: usize,
    /// `None` until the first [`Self::advance`] anchors it. The daemon
    /// passes elapsed milliseconds rather than the store reading a
    /// clock, so playback is testable without one.
    shown_at_ms: Option<u64>,
    mode: AnimationMode,
    /// `0` = infinite. The daemon converts Kitty's `v=` (where `v=1`
    /// means infinite) to `v - 1` before setting it.
    max_loops: u32,
    current_loop: u32,
    /// Pinned (refcount > 0) entries are never evicted.
    refcount: u32,
    /// Sent without `i=` or `I=`, so no later command can name it.
    #[cfg_attr(feature = "state-dump", serde(default))]
    anonymous: bool,
    /// The store's clock at the last transmit or placement; the byte
    /// cap evicts the smallest first.
    #[cfg_attr(feature = "state-dump", serde(default))]
    atime: u64,
    /// The producer's `I=`.
    #[cfg_attr(feature = "state-dump", serde(default))]
    number: Option<u32>,
    /// Creation order, which sending the image again keeps.
    #[cfg_attr(feature = "state-dump", serde(default))]
    serial: u64,
}

impl ImageEntry {
    #[must_use]
    pub fn new(width: u32, height: u32, format: ImageFormat, pixels: impl Into<Bytes>) -> Self {
        Self {
            width,
            height,
            format,
            frames: vec![Frame {
                pixels: pixels.into(),
                gap_ms: 0,
            }],
            current: 0,
            shown_at_ms: None,
            // Kitty's default (ANIMATION_LOADING): a producer that
            // streams frames and never sends `a=a,s=3` still sees them
            // play once.
            mode: AnimationMode::Loading,
            max_loops: 0,
            current_loop: 0,
            refcount: 0,
            anonymous: false,
            atime: 0,
            number: None,
            serial: 0,
        }
    }

    #[must_use]
    pub const fn into_anonymous(mut self) -> Self {
        self.anonymous = true;
        self
    }

    #[must_use]
    pub const fn is_anonymous(&self) -> bool {
        self.anonymous
    }

    #[must_use]
    pub const fn atime(&self) -> u64 {
        self.atime
    }

    #[must_use]
    pub const fn with_number(mut self, number: u32) -> Self {
        self.number = Some(number);
        self
    }

    #[must_use]
    pub const fn number(&self) -> Option<u32> {
        self.number
    }

    /// Pixels of the currently-displayed frame.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.frames[self.current].pixels
    }

    #[must_use]
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    #[must_use]
    pub fn frame(&self, idx: usize) -> Option<&Frame> {
        self.frames.get(idx)
    }

    #[must_use]
    pub const fn frame_count(&self) -> usize {
        self.frames.len()
    }

    #[must_use]
    pub const fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    #[must_use]
    pub const fn current_frame(&self) -> usize {
        self.current
    }

    #[must_use]
    pub const fn mode(&self) -> AnimationMode {
        self.mode
    }

    #[must_use]
    pub const fn refcount(&self) -> u32 {
        self.refcount
    }

    const OVERHEAD: usize = size_of::<Self>() + size_of::<ImageId>();

    /// Counts the per-entry and per-frame overhead, not just pixels:
    /// charged 4 bytes each, a flood of 1x1 images escapes the budget
    /// (`docs/explanation/protocols/kitty-graphics.md` "Why the budget
    /// counts more than pixels").
    #[must_use]
    pub fn byte_len(&self) -> usize {
        Self::OVERHEAD + self.frames.iter().map(Frame::byte_len).sum::<usize>()
    }

    fn current_gap(&self) -> u32 {
        self.frames[self.current].gap_ms
    }

    fn total_gap_ms(&self) -> u64 {
        self.frames.iter().map(|f| u64::from(f.gap_ms)).sum()
    }

    /// Mirrors kitty's `image_is_animatable`.
    fn is_animatable(&self) -> bool {
        self.mode != AnimationMode::Stopped
            && self.frames.len() > 1
            && self.total_gap_ms() > 0
            && (self.max_loops == 0 || self.current_loop < self.max_loops)
    }

    /// Any `s=` zeroes the loop counter, as in kitty; resuming from
    /// `Stopped` re-anchors the frame clock so the current frame gets
    /// its full gap.
    pub(crate) fn set_mode(&mut self, mode: AnimationMode) {
        if self.mode == AnimationMode::Stopped && mode != AnimationMode::Stopped {
            self.shown_at_ms = None;
        }
        self.mode = mode;
        self.current_loop = 0;
    }

    pub(crate) const fn set_max_loops(&mut self, max_loops: u32) {
        self.max_loops = max_loops;
    }

    /// Out-of-range indices are ignored.
    pub(crate) fn set_gap(&mut self, idx: usize, gap_ms: u32) {
        if let Some(f) = self.frames.get_mut(idx) {
            f.gap_ms = gap_ms;
        }
    }

    /// `true` when the displayed frame changed. Out-of-range indices
    /// are ignored.
    pub(crate) const fn jump_to(&mut self, idx: usize) -> bool {
        if idx >= self.frames.len() || idx == self.current {
            return false;
        }
        self.current = idx;
        self.shown_at_ms = None;
        true
    }

    /// `None` when the entry is not animatable or not yet anchored.
    #[must_use]
    pub fn next_due_ms(&self) -> Option<u64> {
        if !self.is_animatable() {
            return None;
        }
        let shown_at = self.shown_at_ms?;
        Some(shown_at.saturating_add(u64::from(self.current_gap())))
    }

    /// Advance playback to the frame that should show at `now_ms`, per
    /// the kitty playback model: gapless frames are skipped instantly,
    /// `Loading` halts on the last frame, `Running` loops until
    /// `max_loops`. Returns the new index when the displayed frame
    /// changed.
    pub(crate) fn advance(&mut self, now_ms: u64) -> Option<usize> {
        if !self.is_animatable() {
            return None;
        }
        let Some(shown_at) = self.shown_at_ms else {
            self.shown_at_ms = Some(now_ms);
            return None;
        };
        if now_ms < shown_at.saturating_add(u64::from(self.current_gap())) {
            return None;
        }
        let start = self.current;
        loop {
            let next = (self.current + 1) % self.frames.len();
            if next == 0 {
                match self.mode {
                    AnimationMode::Running => {
                        self.current_loop = self.current_loop.saturating_add(1);
                        if self.max_loops != 0 && self.current_loop >= self.max_loops {
                            return None;
                        }
                    }
                    _ => return None,
                }
            }
            self.current = next;
            if self.current_gap() != 0 {
                break;
            }
            if self.current == start {
                break;
            }
        }
        self.shown_at_ms = Some(now_ms);
        (self.current != start).then_some(self.current)
    }
}

/// The byte-neutral mutations `a=a` may make to a stored image. The
/// store hands out this rather than `&mut ImageEntry` so no mutation
/// reachable from outside can change a byte total without going
/// through a store method that charges the cap for it.
#[derive(Debug)]
pub struct AnimationControl<'a> {
    entry: &'a mut ImageEntry,
}

impl<'a> AnimationControl<'a> {
    const fn new(entry: &'a mut ImageEntry) -> Self {
        Self { entry }
    }

    pub fn set_mode(&mut self, mode: AnimationMode) {
        self.entry.set_mode(mode);
    }

    pub const fn set_max_loops(&mut self, max_loops: u32) {
        self.entry.set_max_loops(max_loops);
    }

    /// Out-of-range indices are ignored.
    pub fn set_gap(&mut self, idx: usize, gap_ms: u32) {
        self.entry.set_gap(idx, gap_ms);
    }

    /// `true` when the displayed frame changed.
    pub const fn jump_to(&mut self, idx: usize) -> bool {
        self.entry.jump_to(idx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    NoSuchImage,
    NoSuchFrame,
    /// Over the byte cap even after evicting every other refcount-0
    /// image.
    OverCapacity {
        wanted: usize,
        cap: usize,
    },
    /// At [`MAX_IMAGE_FRAMES`] already. Separate from `OverCapacity`
    /// because the byte cap does not imply it: 1×1 frames are 4 bytes
    /// each, so the store would admit millions of them.
    TooManyFrames {
        cap: usize,
    },
}

const fn frame_capacity_error(err: InsertError) -> FrameError {
    match err {
        InsertError::OverCapacity { wanted, cap } => FrameError::OverCapacity { wanted, cap },
    }
}

/// IDs the store dropped, oldest first, to fit new entries. Returned so the
/// caller can announce removals to peers tracking stored byte totals.
pub type Evicted = Vec<ImageId>;

/// [`ImageStore::push_frame`]'s result: the new frame's 0-based index,
/// plus whatever the store dropped to fit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedFrame {
    pub index: usize,
    pub evicted: Evicted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InsertError {
    /// Won't fit even after evicting every refcount-0 entry. The
    /// dispatcher replies with Kitty `ENOTSUP` so the producer can
    /// downscale or split.
    OverCapacity { wanted: usize, cap: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct ImageStore {
    /// Insertion order is the eviction order, front first.
    #[cfg_attr(feature = "state-dump", serde(with = "crate::state::image_entries"))]
    entries: IndexMap<ImageId, ImageEntry>,
    bytes_total: usize,
    bytes_cap: usize,
    next_anonymous_id: u32,
    #[cfg_attr(feature = "state-dump", serde(default))]
    clock: u64,
    #[cfg_attr(feature = "state-dump", serde(default))]
    next_serial: u64,
}

impl ImageStore {
    #[must_use]
    pub fn new(bytes_cap: usize) -> Self {
        Self {
            entries: IndexMap::new(),
            bytes_total: 0,
            bytes_cap,
            next_anonymous_id: u32::MAX,
            clock: 0,
            next_serial: 0,
        }
    }

    /// A store already over `bytes_cap`, which no mutator can produce;
    /// the witness for [`Self::replace_frame`]'s eviction guard being
    /// `>` rather than `>=`.
    #[cfg(test)]
    pub(crate) fn over_cap_for_test(
        entries: IndexMap<ImageId, ImageEntry>,
        bytes_cap: usize,
    ) -> Self {
        Self {
            bytes_total: entries.values().map(ImageEntry::byte_len).sum(),
            entries,
            bytes_cap,
            next_anonymous_id: u32::MAX,
            clock: 0,
            next_serial: 0,
        }
    }

    /// Id for an image sent without `i=`/`I=` (the Kitty spec allows
    /// it; yazi's direct old-Kitty path does). Counts down from
    /// `u32::MAX`, clear of the low ids producers pick, skipping live
    /// ids.
    pub fn allocate_anonymous_id(&mut self) -> ImageId {
        loop {
            let id = ImageId(self.next_anonymous_id);
            self.next_anonymous_id = self.next_anonymous_id.checked_sub(1).unwrap_or(u32::MAX);
            if !self.entries.contains_key(&id) {
                return id;
            }
        }
    }

    /// Replacing an entry under the same id resets its refcount to
    /// zero; the dispatcher handles the now-invalid placements.
    pub fn insert(&mut self, id: ImageId, entry: ImageEntry) -> Result<Evicted, InsertError> {
        let needed = entry.byte_len();
        if needed > self.bytes_cap {
            return Err(InsertError::OverCapacity {
                wanted: needed,
                cap: self.bytes_cap,
            });
        }
        // Priced while the existing entry remains in place. Its bytes count
        // as room because the replacement takes its slot; dropping it
        // first would lose it if insertion fails with `Err`.
        let displaced = self.entries.get(&id).map_or(0, ImageEntry::byte_len);
        let freeable = self.freeable_bytes(Some(id)) + displaced;
        if self.bytes_total - freeable + needed > self.bytes_cap {
            return Err(InsertError::OverCapacity {
                wanted: self.bytes_total - displaced + needed,
                cap: self.bytes_cap,
            });
        }
        let mut entry = entry;
        // shift_remove rather than swap_remove keeps the eviction order.
        if let Some(old) = self.entries.shift_remove(&id) {
            self.bytes_total -= old.byte_len();
            entry.serial = old.serial;
        } else {
            self.next_serial += 1;
            entry.serial = self.next_serial;
        }
        let evicted = self.evict_to_fit(needed, None)?;
        self.bytes_total += needed;
        self.entries.insert(id, entry);
        self.touch(id);
        Ok(evicted)
    }

    /// Mark the image as just used, for [`ImageEntry::atime`].
    pub fn touch(&mut self, id: ImageId) {
        if let Some(entry) = self.entries.get_mut(&id) {
            self.clock += 1;
            entry.atime = self.clock;
        }
    }

    /// The target image is never evicted to make room for its own
    /// frame, even at refcount 0.
    pub fn push_frame(&mut self, id: ImageId, frame: Frame) -> Result<PushedFrame, FrameError> {
        let held = self
            .entries
            .get(&id)
            .ok_or(FrameError::NoSuchImage)?
            .frames
            .len();
        if held >= MAX_IMAGE_FRAMES {
            return Err(FrameError::TooManyFrames {
                cap: MAX_IMAGE_FRAMES,
            });
        }
        let needed = frame.byte_len();
        let evicted = self
            .evict_to_fit(needed, Some(id))
            .map_err(frame_capacity_error)?;
        let entry = self.entries.get_mut(&id).ok_or(FrameError::NoSuchImage)?;
        entry.frames.push(frame);
        self.bytes_total += needed;
        Ok(PushedFrame {
            index: entry.frames.len() - 1,
            evicted,
        })
    }

    pub fn replace_frame(
        &mut self,
        id: ImageId,
        frame_idx: usize,
        frame: Frame,
    ) -> Result<Evicted, FrameError> {
        let old_len = {
            let entry = self.entries.get(&id).ok_or(FrameError::NoSuchImage)?;
            entry
                .frames
                .get(frame_idx)
                .ok_or(FrameError::NoSuchFrame)?
                .byte_len()
        };
        let new_len = frame.byte_len();
        let evicted = if new_len > old_len {
            self.evict_to_fit(new_len - old_len, Some(id))
                .map_err(frame_capacity_error)?
        } else {
            Evicted::new()
        };
        let entry = self.entries.get_mut(&id).ok_or(FrameError::NoSuchImage)?;
        let slot = entry
            .frames
            .get_mut(frame_idx)
            .ok_or(FrameError::NoSuchFrame)?;
        *slot = frame;
        self.bytes_total = self.bytes_total - old_len + new_len;
        Ok(evicted)
    }

    /// Returns the freed byte count. The last frame cannot be removed
    /// (the caller deletes the image instead); unlike Kitty, which
    /// promotes the next frame to root, felis does not special-case
    /// index 0.
    pub fn remove_frame(&mut self, id: ImageId, frame_idx: usize) -> Result<usize, FrameError> {
        let entry = self.entries.get_mut(&id).ok_or(FrameError::NoSuchImage)?;
        if frame_idx >= entry.frames.len() {
            return Err(FrameError::NoSuchFrame);
        }
        if entry.frames.len() == 1 {
            return Err(FrameError::NoSuchFrame);
        }
        let removed = entry.frames.remove(frame_idx);
        if entry.current >= entry.frames.len() {
            entry.current = entry.frames.len() - 1;
        } else if frame_idx < entry.current {
            entry.current -= 1;
        }
        let freed = removed.byte_len();
        self.bytes_total -= freed;
        Ok(freed)
    }

    /// `(id, new_frame_index)` of each placed image whose displayed
    /// frame changed. Unplaced images are skipped: ticking them would
    /// wake an idle client for no visible change.
    pub fn advance_animations(&mut self, now_ms: u64) -> Vec<(ImageId, usize)> {
        let mut changed = Vec::new();
        for (id, entry) in &mut self.entries {
            if entry.refcount > 0
                && entry.is_animated()
                && let Some(idx) = entry.advance(now_ms)
            {
                changed.push((*id, idx));
            }
        }
        changed
    }

    /// Earliest instant a placed image is next due, or `None` when
    /// nothing is animating. An image that has not yet anchored its
    /// frame clock is due now, so the first tick anchors it.
    #[must_use]
    pub fn next_animation_due_ms(&self, now_ms: u64) -> Option<u64> {
        let mut soonest: Option<u64> = None;
        for entry in self.entries.values() {
            if entry.refcount > 0 && entry.is_animatable() {
                let due = entry.next_due_ms().unwrap_or(now_ms);
                soonest = Some(soonest.map_or(due, |s| s.min(due)));
            }
        }
        soonest
    }

    pub fn animation_control(&mut self, id: ImageId) -> Option<AnimationControl<'_>> {
        self.entries.get_mut(&id).map(AnimationControl::new)
    }

    #[must_use]
    pub fn get(&self, id: ImageId) -> Option<&ImageEntry> {
        self.entries.get(&id)
    }

    /// `false` when the id is unknown (the dispatcher emits `ENOENT`).
    pub fn retain(&mut self, id: ImageId) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.refcount = entry.refcount.saturating_add(1);
                true
            }
            None => false,
        }
    }

    /// `false` when the id is unknown or the refcount is already zero.
    /// Reaching zero does not delete the entry; eviction does, when the
    /// cap is hit.
    pub fn release(&mut self, id: ImageId) -> bool {
        match self.entries.get_mut(&id) {
            Some(entry) if entry.refcount > 0 => {
                entry.refcount -= 1;
                true
            }
            _ => false,
        }
    }

    /// Removes regardless of refcount.
    pub fn delete(&mut self, id: ImageId) -> Option<ImageEntry> {
        let entry = self.entries.shift_remove(&id)?;
        self.bytes_total -= entry.byte_len();
        Some(entry)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub const fn bytes_used(&self) -> usize {
        self.bytes_total
    }

    pub fn iter_ids(&self) -> impl Iterator<Item = ImageId> + '_ {
        self.entries.keys().copied()
    }

    /// kitty's `img_by_client_number`: of the images sent with `I=number`,
    /// the last created.
    #[must_use]
    pub fn newest_with_number(&self, number: u32) -> Option<ImageId> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.number == Some(number))
            .max_by_key(|(_, entry)| entry.serial)
            .map(|(id, _)| *id)
    }

    /// kitty's `get_free_client_id`: the lowest id from 1 up that no
    /// image holds, which an `I=` transmission is given.
    #[must_use]
    pub fn lowest_free_id(&self) -> ImageId {
        let mut taken: Vec<u32> = self.entries.keys().map(|id| id.0).collect();
        taken.sort_unstable();
        let mut free = 1;
        for id in taken {
            if id == free {
                free += 1;
            } else if id > free {
                break;
            }
        }
        ImageId(free)
    }

    #[must_use]
    pub const fn bytes_cap(&self) -> usize {
        self.bytes_cap
    }

    /// Bytes the store could reclaim without dropping anything a
    /// placement still holds, `except` aside.
    fn freeable_bytes(&self, except: Option<ImageId>) -> usize {
        self.entries
            .iter()
            .filter(|(id, entry)| entry.refcount == 0 && except != Some(**id))
            .map(|(_, entry)| entry.byte_len())
            .sum()
    }

    fn evict_to_fit(
        &mut self,
        needed: usize,
        except: Option<ImageId>,
    ) -> Result<Evicted, InsertError> {
        // Priced before anything is dropped: an eviction the caller
        // never hears about is one it cannot announce, and the `Err`
        // arm carries no id list. Refusing first keeps the failed
        // mutation total: the store the caller sees on `Err` is unchanged.
        let freeable = self.freeable_bytes(except);
        if self.bytes_total - freeable + needed > self.bytes_cap {
            return Err(InsertError::OverCapacity {
                wanted: self.bytes_total + needed,
                cap: self.bytes_cap,
            });
        }
        let mut evicted = Evicted::new();
        let mut idx = 0;
        while self.bytes_total + needed > self.bytes_cap && idx < self.entries.len() {
            let Some((&id, entry)) = self.entries.get_index(idx) else {
                break;
            };
            if entry.refcount == 0 && except != Some(id) {
                let size = entry.byte_len();
                self.entries.shift_remove_index(idx);
                self.bytes_total -= size;
                evicted.push(id);
            } else {
                idx += 1;
            }
        }
        Ok(evicted)
    }
}

#[cfg(feature = "state-dump")]
impl ImageStore {
    /// Rejects a restored store no sequence of commands could build.
    ///
    /// # Errors
    /// The first inconsistency found.
    pub fn check_restored(&self) -> Result<(), crate::state::StateError> {
        for (id, entry) in &self.entries {
            let expected = (entry.width as usize)
                .checked_mul(entry.height as usize)
                .and_then(|px| px.checked_mul(entry.format.bytes_per_pixel()));
            let frames_ok = !entry.frames.is_empty()
                && entry.frames.len() <= MAX_IMAGE_FRAMES
                && entry.current < entry.frames.len()
                && entry
                    .frames
                    .iter()
                    .all(|frame| Some(frame.pixels.len()) == expected);
            if !frames_ok {
                return Err(crate::state::StateError(format!(
                    "frames of image {}",
                    id.0
                )));
            }
        }
        let charged: usize = self.entries.values().map(ImageEntry::byte_len).sum();
        if charged != self.bytes_total || charged > self.bytes_cap {
            return Err(crate::state::StateError(
                "image store byte total".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The part of an image a placement draws: `source` clipped to the
/// image, else the whole image. `None` when nothing is left to draw.
#[must_use]
pub fn clip_source(
    image_width: u32,
    image_height: u32,
    source: Option<SourceRect>,
) -> Option<SourceRect> {
    let (x, y, width, height) = match source {
        Some(r) if r.width > 0 && r.height > 0 => (r.x, r.y, r.width, r.height),
        _ => (0, 0, image_width, image_height),
    };
    let x = x.min(image_width);
    let y = y.min(image_height);
    let width = width.min(image_width - x);
    let height = height.min(image_height - y);
    (width > 0 && height > 0).then_some(SourceRect {
        x,
        y,
        width,
        height,
    })
}

/// One axis of a placement's cell extent, never zero: every geometry
/// query reads [`Self::cells`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub enum Extent {
    /// `c=` / `r=` as sent.
    Requested(NonZeroU16),
    /// Omitted or zero `c=` / `r=`: `ceil(source_px / cell_px)`,
    /// re-resolved by [`Placements::rescale`] when the cell size changes.
    Natural(NonZeroU16),
}

impl Extent {
    /// A zero `requested` is auto. An unknown (zero) cell size counts as
    /// 1 px, so the extent, and the cursor advance past it, cover the
    /// image at any real cell size; the first real size shrinks it.
    #[must_use]
    pub fn resolve(requested: u16, source_px: u32, cell_px: u16) -> Self {
        NonZeroU16::new(requested).map_or_else(
            || Self::Natural(natural_cells(source_px, cell_px)),
            Self::Requested,
        )
    }

    #[must_use]
    pub const fn cells(self) -> NonZeroU16 {
        match self {
            Self::Requested(n) | Self::Natural(n) => n,
        }
    }

    /// Re-resolves a natural axis at `cell_px` and reports whether it
    /// moved. An unknown (zero) cell size keeps the axis: the 1 px rule
    /// would grow the extent over text already printed past it.
    fn rescale(&mut self, source_px: u32, cell_px: u16) -> bool {
        let Self::Natural(n) = self else {
            return false;
        };
        if cell_px == 0 {
            return false;
        }
        let resolved = natural_cells(source_px, cell_px);
        let moved = *n != resolved;
        *n = resolved;
        moved
    }
}

fn natural_cells(source_px: u32, cell_px: u16) -> NonZeroU16 {
    let cells = u16::try_from(source_px.div_ceil(u32::from(cell_px.max(1)))).unwrap_or(u16::MAX);
    NonZeroU16::new(cells).unwrap_or(NonZeroU16::MIN)
}

/// 1-based cell coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct CellPos {
    /// 1-based live row, signed because an anchor keeps its coordinate
    /// as its text scrolls into history: row `1` is the top live row,
    /// `0` the youngest scrollback line, `-(n-1)` the `n`-th
    /// (`docs/reference/protocols/kitty-graphics.md` "Scrollback-anchored
    /// placements").
    pub row: i32,
    pub col: u16,
}

/// One direct image placement (`a=T` / `a=p`). Unicode-placeholder
/// placements live in the cell grid and do not appear here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct Placement {
    /// Must already exist in the [`ImageStore`]: the dispatcher inserts
    /// before recording the placement.
    pub image_id: ImageId,
    /// `Some` for every placement the dispatcher records; one sent
    /// without `p=` gets [`Placements::allocate_anonymous_id`]'s. `None`
    /// arrives only in a dump from a build that kept one id-less slot
    /// per image.
    pub placement_id: Option<PlacementId>,
    pub anchor: CellPos,
    pub cols: Extent,
    pub rows: Extent,
    /// `None` paints the whole image scaled to `cols × rows`.
    pub source: Option<SourceRect>,
    /// Negative renders behind text. The renderer sorts by this; the
    /// table preserves insertion order.
    pub z_index: i32,
    /// `C=1`.
    pub no_cursor_move: bool,
    /// `q=` (0/1/2).
    pub quiet: u8,
    /// Its id came from [`Placements::allocate_anonymous_id`], not a
    /// producer's `p=`, so no `p=` addresses it.
    #[cfg_attr(feature = "state-dump", serde(default))]
    pub anonymous: bool,
}

impl Placement {
    /// `(cols, rows)` in cells.
    #[must_use]
    pub const fn extent(&self) -> (u16, u16) {
        (self.cols.cells().get(), self.rows.cells().get())
    }

    #[must_use]
    pub const fn contains_cell(&self, row_1based: u16, col_1based: u16) -> bool {
        self.contains_row(row_1based) && self.contains_col(col_1based)
    }

    /// The query row is a live coordinate; a placement anchored in
    /// scrollback matches the live rows its body still covers.
    #[must_use]
    pub const fn contains_row(&self, row_1based: u16) -> bool {
        let top = self.anchor.row;
        let bottom = top.saturating_add(self.rows.cells().get() as i32 - 1);
        let row = row_1based as i32;
        row >= top && row <= bottom
    }

    #[must_use]
    pub const fn contains_col(&self, col_1based: u16) -> bool {
        let left = self.anchor.col;
        let right = left.saturating_add(self.cols.cells().get() - 1);
        col_1based >= left && col_1based <= right
    }
}

/// A [`Placement`] mirrored for the client, omitting daemon-only fields.
/// Lives here so the renderer can consume it directly without per-placement
/// conversions on redraw (`docs/explanation/architecture/overview.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPlacement {
    pub image_id: ImageId,
    /// `None` for the image's default placement.
    pub placement_id: Option<PlacementId>,
    /// 1-based; rows ≤ 0 anchor in the scrollback (row 0 is the
    /// youngest scrollback line), kept in sync with the daemon by
    /// replaying `ImageMsg::PlacementsShifted`
    /// (`docs/reference/protocols/kitty-graphics.md` "Scrollback-anchored
    /// placements").
    pub anchor_row: i32,
    pub anchor_col: u16,
    /// `0` = natural width from the image pixel size.
    pub cols: u16,
    /// `0` = natural height.
    pub rows: u16,
    /// `None` paints the whole image.
    pub source: Option<SourceRect>,
    /// Negative renders under the cell layer; ties resolve by insertion
    /// order.
    pub z_index: i32,
}

/// A Kitty Unicode-placeholder virtual placement (`U=1`): the image's
/// cell extent only, since its screen position is wherever the producer
/// paints the `U+10EEEE` cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct VirtualPlacement {
    pub image_id: ImageId,
    /// `p=`, or [`Placements::allocate_anonymous_id`]'s for a put
    /// without one. `None` only in a client shadow fed by a minor-0
    /// daemon, which keeps one virtual placement per image.
    #[cfg_attr(feature = "state-dump", serde(default))]
    pub placement_id: Option<PlacementId>,
    /// Its id came from [`Placements::allocate_anonymous_id`]; no
    /// command can name it.
    #[cfg_attr(feature = "state-dump", serde(default))]
    pub anonymous: bool,
    /// Total cells the whole image spans.
    pub cols: u16,
    pub rows: u16,
    pub z_index: i32,
}

/// A placement of either kind, as a rekey or an eviction hands it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnyPlacement {
    Anchored(Placement),
    Virtual(VirtualPlacement),
}

/// What left the table to make way for [`Placements::upsert`]'s
/// placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Displaced {
    /// The placement under the same key; its ref is the caller's to
    /// release.
    Replaced(Placement),
    /// The screen's oldest placement, over [`MAX_PLACEMENTS`].
    Evicted(AnyPlacement),
}

/// Placements one screen holds, direct and virtual together. A session
/// outlives its window and every client mirrors the table, so a producer
/// repeating puts must not grow it without bound; kitty has no cap.
pub const MAX_PLACEMENTS: usize = 16_384;

/// Side-table of [`Placement`]s keyed by `(image_id, placement_id)`.
/// Insertion order is the renderer's tiebreaker among equal z-indices.
#[derive(Debug, Clone)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize),
    serde(from = "PlacementsDump", into = "PlacementsDump")
)]
pub struct Placements {
    entries: Table<Placement>,
    /// Keyed like `entries`, which share the key space: one
    /// `(image_id, placement_id)` names a direct or a virtual placement,
    /// never both. Kept apart because nothing that moves or clears
    /// anchored placements touches them.
    virtuals: Table<VirtualPlacement>,
    /// Both tables draw from it, so the oldest placement of either kind
    /// is the one with the lowest.
    next_seq: u64,
    /// Ids handed out by [`Self::allocate_anonymous_id`], counted down
    /// from `u32::MAX`.
    anonymous_issued: u32,
    cap: usize,
}

#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
#[derive(Default)]
struct PlacementsDump {
    entries: Vec<Placement>,
    virtuals: Vec<VirtualPlacement>,
    anonymous_issued: u32,
    /// Each entry's and virtual's sequence number, which orders the two
    /// kinds against each other for eviction. Empty in a dump that
    /// predates them: entries are then older than virtuals.
    entry_seqs: Vec<u64>,
    virtual_seqs: Vec<u64>,
}

impl From<PlacementsDump> for Placements {
    fn from(dump: PlacementsDump) -> Self {
        let mut table = Self::new();
        table.anonymous_issued = dump.anonymous_issued;
        let (entry_seqs, virtual_seqs) = if dump.entry_seqs.len() == dump.entries.len()
            && dump.virtual_seqs.len() == dump.virtuals.len()
        {
            (dump.entry_seqs, dump.virtual_seqs)
        } else {
            let n = dump.entries.len() as u64;
            (
                (0..n).collect(),
                (n..n + dump.virtuals.len() as u64).collect(),
            )
        };
        for (seq, p) in entry_seqs.into_iter().zip(dump.entries) {
            table.entries.insert(seq, p);
        }
        for (seq, v) in virtual_seqs.into_iter().zip(dump.virtuals) {
            table.virtuals.insert(seq, v);
        }
        table.next_seq = table
            .entries
            .iter_seq()
            .map(|(seq, _)| seq)
            .chain(table.virtuals.iter_seq().map(|(seq, _)| seq))
            .max()
            .map_or(0, |seq| seq + 1);
        table
    }
}

impl From<Placements> for PlacementsDump {
    fn from(table: Placements) -> Self {
        Self {
            entries: table.entries.iter().cloned().collect(),
            virtuals: table.virtuals.iter().copied().collect(),
            anonymous_issued: table.anonymous_issued,
            entry_seqs: table.entries.iter_seq().map(|(seq, _)| seq).collect(),
            virtual_seqs: table.virtuals.iter_seq().map(|(seq, _)| seq).collect(),
        }
    }
}

impl Default for Placements {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for Placements {
    fn eq(&self, other: &Self) -> bool {
        self.entries.iter().eq(other.entries.iter())
            && self.virtuals.iter().eq(other.virtuals.iter())
            && self.anonymous_issued == other.anonymous_issued
            && self.cap == other.cap
    }
}

impl Eq for Placements {}

impl Placements {
    #[must_use]
    pub fn new() -> Self {
        Self::with_cap(MAX_PLACEMENTS)
    }

    /// # Panics
    ///
    /// If `cap` is zero: the placement just put must stay.
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        assert!(cap > 0, "a placement table holds at least one placement");
        Self {
            entries: Table::new(),
            virtuals: Table::new(),
            next_seq: 0,
            anonymous_issued: 0,
            cap,
        }
    }

    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }

    const fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// Id for a placement sent without `p=`, which kitty never replaces:
    /// each such put is a placement of its own. Counts down from
    /// `u32::MAX`, clear of the low ids producers pick, skipping the
    /// image's live ids.
    pub fn allocate_anonymous_id(&mut self, image_id: ImageId) -> PlacementId {
        loop {
            let id = PlacementId(u32::MAX - self.anonymous_issued);
            self.anonymous_issued = self.anonymous_issued.wrapping_add(1);
            if !self.holds(image_id, id) {
                return id;
            }
        }
    }

    fn holds(&self, image_id: ImageId, id: PlacementId) -> bool {
        let key = (image_id, Some(id));
        self.entries.contains(key) || self.virtuals.contains(key)
    }

    fn evict_over_cap(&mut self) -> Option<AnyPlacement> {
        if self.entries.len() + self.virtuals.len() <= self.cap {
            return None;
        }
        match (self.entries.oldest_seq(), self.virtuals.oldest_seq()) {
            (Some(direct), Some(virt)) if virt < direct => {
                self.virtuals.pop_oldest().map(AnyPlacement::Virtual)
            }
            (Some(_), _) => self.entries.pop_oldest().map(AnyPlacement::Anchored),
            (None, _) => self.virtuals.pop_oldest().map(AnyPlacement::Virtual),
        }
    }

    /// Last write wins under one producer `(image_id, placement_id)`, as
    /// Kitty specifies; an anonymous placement is never replaced.
    pub fn upsert(&mut self, placement: Placement) -> Option<Displaced> {
        let key = placement.key();
        if !placement.anonymous && self.entries.get(key).is_some_and(|p| !p.anonymous) {
            return self
                .entries
                .replace(key, placement)
                .map(Displaced::Replaced);
        }
        let seq = self.take_seq();
        self.entries.insert(seq, placement);
        self.evict_over_cap().map(Displaced::Evicted)
    }

    /// Only a producer-addressed placement matches.
    pub fn remove(
        &mut self,
        image_id: ImageId,
        placement_id: Option<PlacementId>,
    ) -> Option<Placement> {
        let key = (image_id, placement_id);
        if self.entries.get(key)?.anonymous {
            return None;
        }
        self.entries.remove(key)
    }

    /// Moves an anonymous placement off `id` before a producer's `p=`
    /// takes it, since the wire names both by `(image_id, placement_id)`.
    /// It goes to the end of its table, where the client's remove and
    /// re-add puts it, so both sides keep one order. Returns the id it
    /// left and the placement under its new id.
    pub fn rekey_anonymous(&mut self, image_id: ImageId, id: PlacementId) -> Option<AnyPlacement> {
        let key = (image_id, Some(id));
        if self.entries.get(key).is_some_and(|p| p.anonymous) {
            let fresh = self.allocate_anonymous_id(image_id);
            let mut entry = self.entries.remove(key)?;
            entry.placement_id = Some(fresh);
            let seq = self.take_seq();
            self.entries.insert(seq, entry.clone());
            return Some(AnyPlacement::Anchored(entry));
        }
        if !self.virtuals.get(key).is_some_and(|v| v.anonymous) {
            return None;
        }
        let fresh = self.allocate_anonymous_id(image_id);
        let mut entry = self.virtuals.remove(key)?;
        entry.placement_id = Some(fresh);
        let seq = self.take_seq();
        self.virtuals.insert(seq, entry);
        Some(AnyPlacement::Virtual(entry))
    }

    /// Removed entries come back in insertion order; kept entries keep
    /// their order.
    pub fn remove_where(&mut self, mut pred: impl FnMut(&Placement) -> bool) -> Vec<Placement> {
        self.entries.extract(|p| pred(p))
    }

    pub fn delete_image(&mut self, image_id: ImageId) -> Vec<Placement> {
        self.entries.extract_image(image_id)
    }

    /// Whether any placement of either kind shows the image.
    #[must_use]
    pub fn references(&self, image_id: ImageId) -> bool {
        self.entries.has_image(image_id) || self.virtuals.has_image(image_id)
    }

    /// Overwritten in place under its `(image_id, placement_id)`, or by
    /// image alone when it has no id; an anonymous placement is always a
    /// new one. The client-side shadow applies the identical rule.
    /// Returns the placement evicted over [`MAX_PLACEMENTS`].
    pub fn upsert_virtual(&mut self, placement: VirtualPlacement) -> Option<AnyPlacement> {
        if !placement.anonymous {
            let held = match placement.placement_id {
                Some(_) => self.virtuals.get(placement.key()).filter(|v| !v.anonymous),
                None => self
                    .virtuals
                    .iter()
                    .find(|v| !v.anonymous && v.image_id == placement.image_id),
            }
            .map(Keyed::key);
            if let Some(key) = held {
                self.virtuals.replace(key, placement);
                return None;
            }
        }
        let seq = self.take_seq();
        self.virtuals.insert(seq, placement);
        self.evict_over_cap()
    }

    /// Only a producer-addressed virtual placement matches.
    pub fn remove_virtual_placement(
        &mut self,
        image_id: ImageId,
        placement_id: PlacementId,
    ) -> Option<VirtualPlacement> {
        let key = (image_id, Some(placement_id));
        if self.virtuals.get(key)?.anonymous {
            return None;
        }
        self.virtuals.remove(key)
    }

    /// Every virtual placement of the image, in table order.
    pub fn remove_virtual(&mut self, image_id: ImageId) -> Vec<VirtualPlacement> {
        self.virtuals.extract_image(image_id)
    }

    /// kitty's `grman_put_cell_image`: a non-zero id picks the virtual
    /// placement with that id, zero the image's first.
    pub fn virtual_for_cell(
        &self,
        image_id: ImageId,
        placement_id: u32,
    ) -> Option<&VirtualPlacement> {
        self.virtuals.iter().find(|v| {
            v.image_id == image_id
                && (placement_id == 0 || v.placement_id.is_none_or(|p| p.0 == placement_id))
        })
    }

    pub fn retain_virtual(&mut self, mut pred: impl FnMut(&VirtualPlacement) -> bool) {
        self.virtuals.extract(|v| !pred(v));
    }

    pub fn iter_virtual(&self) -> impl Iterator<Item = &VirtualPlacement> {
        self.virtuals.iter()
    }

    /// Insertion order; z-index sorting is the renderer's.
    pub fn iter(&self) -> impl Iterator<Item = &Placement> {
        self.entries.iter()
    }

    pub fn for_image(&self, image_id: ImageId) -> impl Iterator<Item = &Placement> {
        self.entries.of_image(image_id)
    }

    /// Anchored entries only; virtual placements are not counted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.virtuals.is_empty()
    }

    /// Remove the placements a screen clear drops (kitty's
    /// `grman_clear`); virtual placements stay.
    pub fn clear(&mut self, scope: crate::ImageClear) -> Vec<Placement> {
        self.remove_where(|p| match scope {
            crate::ImageClear::Screen => p.anchor.row + i32::from(p.rows.cells().get()) > 1,
            crate::ImageClear::All => true,
        })
    }

    /// Shift every anchor up by `lines`. Anchors that leave the
    /// `retain_rows` lines of retained history can never scroll back
    /// into view, so those placements are removed and returned for
    /// refcount release and `PlacementRemoved`. `retain_rows` is `0` on
    /// the alternate screen, which has no history.
    pub fn shift_up(&mut self, lines: u32, retain_rows: u32) -> Vec<Placement> {
        if lines == 0 || self.entries.is_empty() {
            return Vec::new();
        }
        let lines_i32 = i32::try_from(lines).unwrap_or(i32::MAX);
        let horizon = 1i32.saturating_sub(i32::try_from(retain_rows).unwrap_or(i32::MAX));
        self.entries.extract(|p| {
            let shifted = p.anchor.row.saturating_sub(lines_i32);
            if shifted < horizon {
                return true;
            }
            p.anchor.row = shifted;
            false
        })
    }

    /// Re-resolves the natural axes of every placement at `cell_px`, as
    /// kitty's `grman_rescale` does; `on_change` sees each placement
    /// whose extent moved. A placement whose image is gone is left as
    /// is.
    pub fn rescale(
        &mut self,
        images: &ImageStore,
        cell_px: (u16, u16),
        mut on_change: impl FnMut(&Placement),
    ) {
        for p in self.entries.values_mut() {
            let Some(entry) = images.get(p.image_id) else {
                continue;
            };
            let source_px = clip_source(entry.width, entry.height, p.source)
                .map_or((0, 0), |r| (r.width, r.height));
            let cols_moved = p.cols.rescale(source_px.0, cell_px.0);
            let rows_moved = p.rows.rescale(source_px.1, cell_px.1);
            if cols_moved || rows_moved {
                on_change(p);
            }
        }
    }

    /// Rewrite every anchor row through `remap`, for
    /// [`crate::Grid::reflow`] (REQ-604). `None` evicts the placement;
    /// evictions are returned as [`Self::shift_up`]'s are.
    pub fn remap_rows(&mut self, mut remap: impl FnMut(i32) -> Option<i32>) -> Vec<Placement> {
        self.entries.extract(|p| match remap(p.anchor.row) {
            Some(row) => {
                p.anchor.row = row;
                false
            }
            None => true,
        })
    }
}

#[cfg(test)]
mod tests;

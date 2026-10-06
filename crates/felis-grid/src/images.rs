//! Daemon-side image store for the Kitty graphics protocol: decoded
//! pixel buffers under a per-session byte cap
//! (`docs/explanation/security-model.md` "Kitty graphics").

use bytes::Bytes;
use indexmap::IndexMap;

pub use felis_protocol::{
    ImageId, PlacementId,
    messages::{ImageFormat, MAX_IMAGE_FRAMES, SourceRect},
};

/// One animation frame, already coalesced: the daemon composites the
/// Kitty base frame (`c=`) and transmitted rectangle into a complete
/// `width × height` image before it reaches the store
/// (`docs/reference/protocols/kitty-graphics.md` "Animation").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
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
        }
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
pub struct ImageStore {
    /// Insertion order is the eviction order, front first.
    entries: IndexMap<ImageId, ImageEntry>,
    bytes_total: usize,
    bytes_cap: usize,
    next_anonymous_id: u32,
}

impl ImageStore {
    #[must_use]
    pub fn new(bytes_cap: usize) -> Self {
        Self {
            entries: IndexMap::new(),
            bytes_total: 0,
            bytes_cap,
            next_anonymous_id: u32::MAX,
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
        // shift_remove rather than swap_remove keeps the eviction order.
        if let Some(old) = self.entries.shift_remove(&id) {
            self.bytes_total -= old.byte_len();
        }
        let evicted = self.evict_to_fit(needed, None)?;
        self.bytes_total += needed;
        self.entries.insert(id, entry);
        Ok(evicted)
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

/// Resolves each auto (zero) axis of a `c=` / `r=` request to
/// `ceil(source_px / cell_px)`, independently. An unknown (zero) cell
/// size counts as 1 px, so the extent, and the cursor advance past it,
/// cover the image at any real cell size; the first real size shrinks
/// it.
#[must_use]
pub fn effective_extent(
    requested: (u16, u16),
    source_px: (u32, u32),
    cell_px: (u16, u16),
) -> (u16, u16) {
    let axis = |requested: u16, px: u32, cell: u16| {
        if requested != 0 {
            return requested;
        }
        u16::try_from(px.div_ceil(u32::from(cell.max(1))))
            .unwrap_or(u16::MAX)
            .max(1)
    };
    (
        axis(requested.0, source_px.0, cell_px.0),
        axis(requested.1, source_px.1, cell_px.1),
    )
}

/// 1-based cell coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub struct Placement {
    /// Must already exist in the [`ImageStore`]: the dispatcher inserts
    /// before recording the placement.
    pub image_id: ImageId,
    /// `None` is the image's default placement, at most one per
    /// `image_id`.
    pub placement_id: Option<PlacementId>,
    pub anchor: CellPos,
    /// The effective extent every geometry query uses: the request with
    /// its auto axes resolved by [`effective_extent`].
    pub cols: u16,
    pub rows: u16,
    /// `c=` / `r=` as sent; zero is auto, re-resolved by
    /// [`Placements::rescale`] when the cell size changes.
    pub requested_cols: u16,
    pub requested_rows: u16,
    /// `None` paints the whole image scaled to `cols × rows`.
    pub source: Option<SourceRect>,
    /// Negative renders behind text. The renderer sorts by this; the
    /// table preserves insertion order.
    pub z_index: i32,
    /// `C=1`.
    pub no_cursor_move: bool,
    /// `q=` (0/1/2).
    pub quiet: u8,
}

impl Placement {
    #[must_use]
    pub const fn contains_cell(&self, row_1based: u16, col_1based: u16) -> bool {
        self.contains_row(row_1based) && self.contains_col(col_1based)
    }

    /// Empty placements never intersect. The query row is a live
    /// coordinate; a placement anchored in scrollback matches the live
    /// rows its body still covers.
    #[must_use]
    pub const fn contains_row(&self, row_1based: u16) -> bool {
        if self.rows == 0 {
            return false;
        }
        let top = self.anchor.row;
        let bottom = top.saturating_add(self.rows as i32 - 1);
        let row = row_1based as i32;
        row >= top && row <= bottom
    }

    #[must_use]
    pub const fn contains_col(&self, col_1based: u16) -> bool {
        if self.cols == 0 {
            return false;
        }
        let left = self.anchor.col;
        let right = left.saturating_add(self.cols.saturating_sub(1));
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
pub struct VirtualPlacement {
    pub image_id: ImageId,
    /// Total cells the whole image spans.
    pub cols: u16,
    pub rows: u16,
    pub z_index: i32,
}

/// Side-table of [`Placement`]s keyed by `(image_id, placement_id)`.
/// Insertion order is the renderer's tiebreaker among equal z-indices.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Placements {
    entries: Vec<Placement>,
    /// At most one per image id. Kept apart from `entries`: their
    /// lifetime is tied to the image, not a placement id (no
    /// `PlacementRemoved` names a virtual placement), so one dies only
    /// when its image leaves the store.
    virtuals: Vec<VirtualPlacement>,
}

impl Placements {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            virtuals: Vec::new(),
        }
    }

    /// Last write wins under one `(image_id, placement_id)`, as Kitty
    /// specifies.
    pub fn upsert(&mut self, placement: Placement) {
        let key = (placement.image_id, placement.placement_id);
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|p| (p.image_id, p.placement_id) == key)
        {
            *slot = placement;
        } else {
            self.entries.push(placement);
        }
    }

    pub fn remove(
        &mut self,
        image_id: ImageId,
        placement_id: Option<PlacementId>,
    ) -> Option<Placement> {
        let idx = self
            .entries
            .iter()
            .position(|p| p.image_id == image_id && p.placement_id == placement_id)?;
        Some(self.entries.remove(idx))
    }

    /// Removed entries come back in insertion order; kept entries keep
    /// their order.
    pub fn remove_where(&mut self, mut pred: impl FnMut(&Placement) -> bool) -> Vec<Placement> {
        let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|p| pred(p));
        self.entries = kept;
        removed
    }

    pub fn delete_image(&mut self, image_id: ImageId) -> Vec<Placement> {
        let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|p| p.image_id == image_id);
        self.entries = kept;
        removed
    }

    /// One entry per image id, overwritten in place; the client-side
    /// shadow applies the identical rule.
    pub fn upsert_virtual(&mut self, placement: VirtualPlacement) {
        if let Some(slot) = self
            .virtuals
            .iter_mut()
            .find(|v| v.image_id == placement.image_id)
        {
            *slot = placement;
        } else {
            self.virtuals.push(placement);
        }
    }

    pub fn remove_virtual(&mut self, image_id: ImageId) {
        self.virtuals.retain(|v| v.image_id != image_id);
    }

    pub fn retain_virtual(&mut self, pred: impl FnMut(&VirtualPlacement) -> bool) {
        self.virtuals.retain(pred);
    }

    pub fn iter_virtual(&self) -> impl Iterator<Item = &VirtualPlacement> {
        self.virtuals.iter()
    }

    /// Insertion order; z-index sorting is the renderer's.
    pub fn iter(&self) -> impl Iterator<Item = &Placement> {
        self.entries.iter()
    }

    pub fn for_image(&self, image_id: ImageId) -> impl Iterator<Item = &Placement> {
        self.entries.iter().filter(move |p| p.image_id == image_id)
    }

    /// Anchored entries only; virtual placements are not counted.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.virtuals.is_empty()
    }

    /// Remove every placement intersecting the inclusive 0-based row
    /// range. `C=1` placements are exempt unless `force` (the RIS /
    /// DECSTR path): a pinned status-line image survives ED / EL but
    /// not a hard reset.
    pub fn remove_intersecting(
        &mut self,
        erase_top: u16,
        erase_bottom: u16,
        force: bool,
    ) -> Vec<Placement> {
        let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|p| {
                if p.no_cursor_move && !force {
                    return false;
                }
                let p_top = p.anchor.row - 1;
                let p_bottom = p_top.saturating_add(i32::from(p.rows.saturating_sub(1)));
                p_top <= i32::from(erase_bottom) && i32::from(erase_top) <= p_bottom
            });
        self.entries = kept;
        removed
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
        let mut removed = Vec::new();
        let lines_i32 = i32::try_from(lines).unwrap_or(i32::MAX);
        let horizon = 1i32.saturating_sub(i32::try_from(retain_rows).unwrap_or(i32::MAX));
        let mut i = 0;
        while i < self.entries.len() {
            let shifted = self.entries[i].anchor.row.saturating_sub(lines_i32);
            if shifted >= horizon {
                self.entries[i].anchor.row = shifted;
                i += 1;
            } else {
                removed.push(self.entries.remove(i));
            }
        }
        removed
    }

    /// Re-resolves the auto axes of every placement at `cell_px`, as
    /// kitty's `grman_rescale` does; `on_change` sees each placement
    /// whose extent moved. A placement whose image is gone is left as
    /// is, and so is an axis whose cell size is unknown (zero): the 1 px
    /// rule would grow the extent over text already printed past it.
    pub fn rescale(
        &mut self,
        images: &ImageStore,
        cell_px: (u16, u16),
        mut on_change: impl FnMut(&Placement),
    ) {
        for p in &mut self.entries {
            if p.requested_cols != 0 && p.requested_rows != 0 {
                continue;
            }
            let Some(entry) = images.get(p.image_id) else {
                continue;
            };
            let source_px = clip_source(entry.width, entry.height, p.source)
                .map_or((0, 0), |r| (r.width, r.height));
            let resolved =
                effective_extent((p.requested_cols, p.requested_rows), source_px, cell_px);
            let extent = (
                if cell_px.0 == 0 { p.cols } else { resolved.0 },
                if cell_px.1 == 0 { p.rows } else { resolved.1 },
            );
            if extent != (p.cols, p.rows) {
                (p.cols, p.rows) = extent;
                on_change(p);
            }
        }
    }

    /// Rewrite every anchor row through `remap`, for
    /// [`crate::Grid::reflow`] (REQ-604). `None` evicts the placement;
    /// evictions are returned as [`Self::shift_up`]'s are.
    pub fn remap_rows(&mut self, mut remap: impl FnMut(i32) -> Option<i32>) -> Vec<Placement> {
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.entries.len() {
            match remap(self.entries[i].anchor.row) {
                Some(row) => {
                    self.entries[i].anchor.row = row;
                    i += 1;
                }
                None => removed.push(self.entries.remove(i)),
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests;

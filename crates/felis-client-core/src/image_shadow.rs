//! Client-side mirror of Kitty graphics state driven by `ImageMsg`.
//!
//! Placements preserve insertion order, which the layout keeps on equal z.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};

use felis_grid::{ScreenBuffer, image_layout::LayoutImages};
use felis_protocol::{
    ImageId, PlacementId,
    messages::{
        ImageFormat, ImageMsg, ImageTarget, MAX_IMAGE_BYTES, MAX_IMAGE_FRAMES,
        MAX_SESSION_IMAGE_BYTES,
    },
};
use thiserror::Error;
// The renderer sits below client-core in the dependency order, so it
// cannot name a type defined here.
pub use felis_grid::image_layout::{ImageDraw, ImageLayout};
pub use felis_grid::images::{ClientPlacement, VirtualPlacement};

/// Process-wide rather than per shadow: a session switch replaces the
/// shadow, and a conversion still in flight from the replaced shadow
/// must not match a token its successor hands out.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// One animation frame. felis ships coalesced frames
/// (docs/reference/protocols/kitty-graphics.md "Animation"), so each
/// frame is a complete pixel buffer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientFrame {
    pub pixels: Vec<u8>,
    pub complete: bool,
}

/// `frames[0]` is the root image (Kitty frame number 1); animation
/// frames arrive over the same `Header`/`Chunk`/`Complete` triple under
/// [`ImageTarget::Frame`]. The daemon drives playback and names the
/// displayed frame with `ShowFrame`; the client never animates on its
/// own (docs/reference/protocols/kitty-graphics.md "Animation").
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientImage {
    /// Image width in pixels (Kitty `s=`).
    pub width: u32,
    /// Image height in pixels (Kitty `v=`).
    pub height: u32,
    /// Post-decode format: the daemon decodes `f=100`/PNG, so it arrives
    /// as [`ImageFormat::Rgba32`].
    pub format: ImageFormat,
    /// Byte length of every frame buffer of this image:
    /// `width × height × bytes_per_pixel`. Frames inherit the geometry.
    pub frame_len: usize,
    /// Playback order; `frames[0]` is the root. Always non-empty.
    pub frames: Vec<ClientFrame>,
    pub current: usize,
    /// The displayed frame has completed since the entry was opened. A
    /// re-transmission of that frame keeps it drawable on the copy the
    /// frontend already holds; a whole-image replacement does not, as
    /// kitty frees an image's pixels when a transmission under its id
    /// starts.
    drawable: bool,
    generation: u64,
    /// [`LayoutImages::stack_rank`]: the daemon's creation serial, or
    /// first arrival from a daemon that sends none.
    rank: u64,
}

impl ClientImage {
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.frames[self.current].pixels
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.frames[self.current].complete
    }

    #[must_use]
    pub const fn current_frame(&self) -> usize {
        self.current
    }

    #[must_use]
    pub const fn frame_count(&self) -> usize {
        self.frames.len()
    }

    #[must_use]
    pub const fn is_drawable(&self) -> bool {
        self.drawable
    }

    /// Changes whenever what this image displays changes, and is never
    /// reused within the process: an asynchronous frontend tags a pixel
    /// conversion with it and installs the result only if it still
    /// matches.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The displayed frame as straight RGBA8, the one format a canvas
    /// `ImageData` takes.
    pub fn write_rgba8(&self, out: &mut Vec<u8>) {
        out.clear();
        match self.format {
            ImageFormat::Rgba32 => out.extend_from_slice(self.pixels()),
            ImageFormat::Rgb24 => {
                out.reserve(self.pixels().len() / 3 * 4);
                for px in self.pixels().as_chunks::<3>().0 {
                    out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
                }
            }
        }
    }
}

/// A transfer the mirror will not honor. Every variant is raised
/// *before* the message is acted on, so the shadow the connection dies
/// holding is the one it held before the frame arrived.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ImageShadowError {
    /// One image claimed more than [`MAX_IMAGE_BYTES`].
    #[error("image {id} claimed {claimed} bytes, over the {MAX_IMAGE_BYTES}-byte per-image cap")]
    ImageBytes { id: u32, claimed: u64 },
    /// The claim would carry the whole mirror past
    /// [`MAX_SESSION_IMAGE_BYTES`].
    #[error(
        "image {id} would retain {wanted} bytes, over the {MAX_SESSION_IMAGE_BYTES}-byte session cap"
    )]
    SessionBytes { id: u32, wanted: usize },
    #[error("image {id} named frame {number}, past the {MAX_IMAGE_FRAMES}-frame cap")]
    FrameIndex { id: u32, number: u32 },
    /// One transfer at a time is the ordering contract: two open would
    /// put the arriving chunks' destination in question.
    #[error("image {id} opened a transfer while image {open}'s was still open")]
    HeaderWhileActive { id: u32, open: u32 },
    /// A frame inherits its geometry, so an unheld id leaves nothing to
    /// inherit.
    #[error("frame header named image {id}, which the mirror does not hold")]
    FrameForUnknownImage { id: u32 },
    /// Appending is `have + 1`; anything further would leave the frames
    /// between undefined.
    #[error("image {id} named frame {number} with only {have} frames transferred")]
    FrameNumberSkipsAhead { id: u32, number: u32, have: usize },
    #[error("image {id} sent pixel bytes with no transfer open")]
    ChunkWithoutHeader { id: u32 },
    #[error("image {id} completed a transfer that was never opened")]
    CompleteWithoutHeader { id: u32 },
    #[error("image {named} spoke for image {open}'s open transfer")]
    TransferIdMismatch { open: u32, named: u32 },
    #[error("image {id} sent {received} bytes into a {expected}-byte frame")]
    ChunkOverrun {
        id: u32,
        expected: usize,
        received: usize,
    },
    /// Accepting a short completion would paint the shortfall as
    /// transparent pixels the producer never sent.
    #[error("image {id} completed with {received} of {expected} bytes")]
    IncompleteComplete {
        id: u32,
        expected: usize,
        received: usize,
    },
}

/// The one transfer a connection may have open. `slot` indexes the
/// image's `frames` and stays valid until the transfer closes: the
/// only removal ([`ImageMsg::Delete`]) aborts the transfer with it.
#[derive(Debug)]
struct ActiveTransfer {
    id: ImageId,
    slot: usize,
    expected: usize,
    received: usize,
    /// Pixels for the displayed frame, swapped in at `Complete` so a
    /// re-read mid-transfer (an atlas reset) finds what was on screen.
    /// Uncharged to the session cap, as the daemon edits frames in place
    /// and admits the edit at the cap; one open transfer bounds it.
    staged: Option<Vec<u8>>,
}

/// What one mirrored image costs the aggregate before a single pixel:
/// the entry and the key that finds it.
const ENTRY_OVERHEAD: usize = size_of::<ClientImage>() + size_of::<ImageId>();

/// What one frame slot costs whether or not pixels ever arrive for it.
const FRAME_OVERHEAD: usize = size_of::<ClientFrame>();

#[derive(Debug, Default)]
pub struct ImageShadow {
    images: HashMap<ImageId, ClientImage>,
    image_order: Vec<ImageId>,
    placements: Vec<ClientPlacement>,
    virtual_placements: Vec<VirtualPlacement>,
    dirty_images: HashSet<ImageId>,
    /// Bytes the images above hold ([`ImageShadow::bytes_of`]'s unit),
    /// kept as a running total: the aggregate has to be answerable
    /// before a header is applied, and re-summing every frame of every
    /// image per header is the one cost the mirror pays per
    /// multi-megabyte transmission.
    retained: usize,
    active: Option<ActiveTransfer>,
    /// Images first heard of, for [`ClientImage::rank`] when the daemon
    /// sends no serial.
    arrivals: u64,
}

impl ImageShadow {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn images(&self) -> &HashMap<ImageId, ClientImage> {
        &self.images
    }

    #[must_use]
    pub fn image(&self, id: ImageId) -> Option<&ClientImage> {
        self.images.get(&id)
    }

    /// Insertion order, which [`ImageLayout`] keeps among equal-z
    /// placements of an image only when the daemon sends no serial.
    #[must_use]
    pub fn placements(&self) -> &[ClientPlacement] {
        &self.placements
    }

    /// What the layout resolves a `U+10EEEE` cell against.
    #[must_use]
    pub fn virtual_placements(&self) -> &[VirtualPlacement] {
        &self.virtual_placements
    }

    /// Bytes the mirror holds (pixels plus the entry and frame records
    /// that carry them), checked against [`MAX_SESSION_IMAGE_BYTES`].
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.placements.is_empty() && self.virtual_placements.is_empty()
    }

    /// Lays out this shadow's placements over `screen`, the composed
    /// viewport the client paints; see [`ImageLayout::rebuild`].
    pub fn layout_into(
        &self,
        layout: &mut ImageLayout,
        screen: &ScreenBuffer,
        cell_px: (f32, f32),
        viewport: u32,
    ) {
        layout.rebuild(
            screen,
            self,
            &self.placements,
            &self.virtual_placements,
            cell_px,
            viewport,
        );
    }

    /// In image insertion order: the renderer's atlas-allocation cache
    /// relies on a stable order, and `HashSet` iteration would shuffle.
    pub fn take_dirty_images(&mut self) -> Vec<ImageId> {
        if self.dirty_images.is_empty() {
            return Vec::new();
        }
        let drained = std::mem::take(&mut self.dirty_images);
        self.image_order
            .iter()
            .filter(|id| drained.contains(*id))
            .copied()
            .collect()
    }

    /// Applies an image message to update the mirror.
    ///
    /// # Errors
    /// [`ImageShadowError`] on protocol memory-cap violations or invalid state
    /// transitions, indicating peer corruption.
    pub fn apply(&mut self, msg: &ImageMsg) -> Result<(), ImageShadowError> {
        match msg {
            ImageMsg::Header { id, target } => return self.apply_header(*id, *target),
            ImageMsg::Chunk { id, bytes } => return self.apply_chunk(*id, bytes),
            ImageMsg::Complete { id } => return self.apply_complete(*id),
            ImageMsg::Delete { id } => {
                self.apply_delete(*id);
            }
            ImageMsg::Placement {
                image_id,
                placement_id,
                anchor_row,
                anchor_col,
                cols,
                rows,
                source,
                z_index,
                requested,
                serial,
            } => {
                self.apply_placement(ClientPlacement {
                    image_id: *image_id,
                    placement_id: *placement_id,
                    anchor_row: *anchor_row,
                    anchor_col: *anchor_col,
                    cols: *cols,
                    rows: *rows,
                    source: *source,
                    z_index: *z_index,
                    requested: *requested,
                    serial: *serial,
                });
            }
            ImageMsg::PlacementRemoved {
                image_id,
                placement_id,
            } => {
                self.apply_placement_removed(*image_id, *placement_id);
            }
            ImageMsg::VirtualPlacement {
                image_id,
                placement_id,
                cols,
                rows,
                z_index,
            } => {
                self.apply_virtual_placement(VirtualPlacement {
                    image_id: *image_id,
                    placement_id: *placement_id,
                    anonymous: false,
                    cols: *cols,
                    rows: *rows,
                    z_index: *z_index,
                    serial: 0,
                });
            }
            ImageMsg::ShowFrame { id, number } => {
                self.apply_show_frame(*id, *number);
            }
            ImageMsg::PlacementsShifted { lines } => {
                self.apply_placements_shifted(*lines);
            }
        }
        Ok(())
    }

    /// Bytes one entry holds: pixels plus the records carrying them, matching
    /// [`felis_grid::images::ImageEntry::byte_len`]. Counting pixels alone
    /// would allow frame-record amplification
    /// (`docs/explanation/protocols/kitty-graphics.md`).
    fn bytes_of(entry: &ClientImage) -> usize {
        ENTRY_OVERHEAD + entry.frames.iter().map(Self::frame_bytes).sum::<usize>()
    }

    const fn frame_bytes(frame: &ClientFrame) -> usize {
        FRAME_OVERHEAD + frame.pixels.len()
    }

    /// Computed at wire width and saturating, so two `u32::MAX` axes
    /// refuse rather than wrap into a plausible length.
    fn frame_len(
        id: ImageId,
        width: u32,
        height: u32,
        format: ImageFormat,
    ) -> Result<usize, ImageShadowError> {
        let claimed = u64::from(width)
            .saturating_mul(u64::from(height))
            .saturating_mul(format.bytes_per_pixel() as u64);
        let over = ImageShadowError::ImageBytes { id: id.0, claimed };
        if claimed > MAX_IMAGE_BYTES {
            return Err(over);
        }
        usize::try_from(claimed).map_err(|_| over)
    }

    /// The aggregate after `id`'s claim replaces `freed` bytes of what
    /// it already holds, or the typed refusal.
    const fn admit_aggregate(
        &self,
        id: ImageId,
        freed: usize,
        claimed: usize,
    ) -> Result<usize, ImageShadowError> {
        let wanted = self.retained - freed + claimed;
        if wanted > MAX_SESSION_IMAGE_BYTES {
            return Err(ImageShadowError::SessionBytes { id: id.0, wanted });
        }
        Ok(wanted)
    }

    /// Mirror-only: anchors past the daemon's retention horizon arrive
    /// as explicit `PlacementRemoved`s, so this never drops an entry no
    /// matter how negative its row goes.
    fn apply_placements_shifted(&mut self, lines: u32) {
        let lines = i32::try_from(lines).unwrap_or(i32::MAX);
        for p in &mut self.placements {
            p.anchor_row = p.anchor_row.saturating_sub(lines);
        }
    }

    fn apply_header(&mut self, id: ImageId, target: ImageTarget) -> Result<(), ImageShadowError> {
        if let Some(open) = &self.active {
            return Err(ImageShadowError::HeaderWhileActive {
                id: id.0,
                open: open.id.0,
            });
        }
        let slot = match target {
            ImageTarget::New {
                width,
                height,
                format,
                serial,
            } => self.open_new(id, width, height, format, serial)?,
            ImageTarget::Frame { number } => self.open_frame(id, number)?,
        };
        let expected = self.images.get(&id).map_or(0, |entry| entry.frame_len);
        let staged = self
            .images
            .get(&id)
            .filter(|entry| entry.drawable && entry.current == slot)
            .map(|_| vec![0u8; expected]);
        self.active = Some(ActiveTransfer {
            id,
            slot,
            expected,
            received: 0,
            staged,
        });
        Ok(())
    }

    /// Replaces the whole entry, animation frames included: a fresh
    /// transmission under a live id is a new image, not an edit, though
    /// it keeps its rank as kitty keeps the image's id.
    fn open_new(
        &mut self,
        id: ImageId,
        width: u32,
        height: u32,
        format: ImageFormat,
        serial: Option<u64>,
    ) -> Result<usize, ImageShadowError> {
        let frame_len = Self::frame_len(id, width, height, format)?;
        let freed = self.images.get(&id).map_or(0, Self::bytes_of);
        let retained =
            self.admit_aggregate(id, freed, ENTRY_OVERHEAD + FRAME_OVERHEAD + frame_len)?;
        let rank = match (serial, self.images.get(&id)) {
            (Some(serial), _) => serial,
            (None, Some(held)) => held.rank,
            (None, None) => {
                self.arrivals += 1;
                self.arrivals
            }
        };
        let entry = ClientImage {
            width,
            height,
            format,
            frame_len,
            frames: vec![ClientFrame {
                pixels: vec![0u8; frame_len],
                complete: false,
            }],
            current: 0,
            drawable: false,
            generation: next_generation(),
            rank,
        };
        let is_new = !self.images.contains_key(&id);
        self.images.insert(id, entry);
        if is_new {
            self.image_order.push(id);
        }
        // Un-dirty until the next Complete; otherwise an in-flight
        // replacement trips the renderer's "is this image new" check on
        // a half-filled buffer.
        self.dirty_images.remove(&id);
        self.retained = retained;
        Ok(0)
    }

    /// `number` is 1-based: `1` re-transmits the root in place, `have +
    /// 1` appends. Anything further ahead is refused rather than
    /// gap-filled, because the frames between would be buffers no
    /// producer ever described.
    fn open_frame(&mut self, id: ImageId, number: NonZeroU32) -> Result<usize, ImageShadowError> {
        // Judged before the lookup, so a hostile number's answer does
        // not depend on what the mirror happens to hold.
        let slot = usize::try_from(number.get() - 1)
            .ok()
            .filter(|_| number.get() as usize <= MAX_IMAGE_FRAMES)
            .ok_or_else(|| ImageShadowError::FrameIndex {
                id: id.0,
                number: number.get(),
            })?;
        let entry = self
            .images
            .get(&id)
            .ok_or(ImageShadowError::FrameForUnknownImage { id: id.0 })?;
        let have = entry.frames.len();
        if slot > have {
            return Err(ImageShadowError::FrameNumberSkipsAhead {
                id: id.0,
                number: number.get(),
                have,
            });
        }
        let frame_len = entry.frame_len;
        if entry.drawable && entry.current == slot {
            return Ok(slot);
        }
        let freed = entry.frames.get(slot).map_or(0, Self::frame_bytes);
        let retained = self.admit_aggregate(id, freed, FRAME_OVERHEAD + frame_len)?;

        let frame = ClientFrame {
            pixels: vec![0u8; frame_len],
            complete: false,
        };
        if let Some(entry) = self.images.get_mut(&id) {
            if slot < entry.frames.len() {
                entry.frames[slot] = frame;
            } else {
                entry.frames.push(frame);
            }
            self.retained = retained;
        }
        Ok(slot)
    }

    fn apply_chunk(&mut self, id: ImageId, bytes: &[u8]) -> Result<(), ImageShadowError> {
        let active = self
            .active
            .as_ref()
            .ok_or(ImageShadowError::ChunkWithoutHeader { id: id.0 })?;
        if active.id != id {
            return Err(ImageShadowError::TransferIdMismatch {
                open: active.id.0,
                named: id.0,
            });
        }
        let start = active.received;
        let end = start + bytes.len();
        if end > active.expected {
            return Err(ImageShadowError::ChunkOverrun {
                id: id.0,
                expected: active.expected,
                received: end,
            });
        }
        let slot = active.slot;
        if let Some(active) = self.active.as_mut()
            && let Some(staged) = active.staged.as_mut()
        {
            staged[start..end].copy_from_slice(bytes);
        } else if let Some(frame) = self
            .images
            .get_mut(&id)
            .and_then(|entry| entry.frames.get_mut(slot))
        {
            frame.pixels[start..end].copy_from_slice(bytes);
        }
        if let Some(active) = self.active.as_mut() {
            active.received = end;
        }
        Ok(())
    }

    fn apply_complete(&mut self, id: ImageId) -> Result<(), ImageShadowError> {
        let active = self
            .active
            .as_ref()
            .ok_or(ImageShadowError::CompleteWithoutHeader { id: id.0 })?;
        if active.id != id {
            return Err(ImageShadowError::TransferIdMismatch {
                open: active.id.0,
                named: id.0,
            });
        }
        if active.received != active.expected {
            return Err(ImageShadowError::IncompleteComplete {
                id: id.0,
                expected: active.expected,
                received: active.received,
            });
        }
        let slot = active.slot;
        let staged = self.active.take().and_then(|open| open.staged);
        if let Some(entry) = self.images.get_mut(&id) {
            if let Some(frame) = entry.frames.get_mut(slot) {
                if let Some(pixels) = staged {
                    frame.pixels = pixels;
                }
                frame.complete = true;
            }
            if entry.current == slot {
                entry.drawable = true;
                entry.generation = next_generation();
                self.dirty_images.insert(id);
            }
        }
        Ok(())
    }

    /// Tolerant like `Placement`: a rehydrate can name the live frame
    /// of an image a later eviction in the same drain removed. An
    /// unfinished frame is dropped the same way, so the renderer is
    /// never handed the zero fill an open header left.
    fn apply_show_frame(&mut self, id: ImageId, number: NonZeroU32) {
        let slot = number.get() as usize - 1;
        let Some(entry) = self.images.get_mut(&id) else {
            return;
        };
        if !entry.frames.get(slot).is_some_and(|f| f.complete) || slot == entry.current {
            return;
        }
        entry.current = slot;
        entry.drawable = true;
        entry.generation = next_generation();
        // The renderer keys its atlas by image id, so a frame swap is a
        // re-upload of the id.
        self.dirty_images.insert(id);
    }

    fn apply_delete(&mut self, id: ImageId) {
        // Aborts a transfer into this image rather than leaving one
        // pointed at a slot the removal takes away: an eviction can
        // land between a header and its chunks on the rehydrate path.
        if self.active.as_ref().is_some_and(|open| open.id == id) {
            self.active = None;
        }
        // A drain that creates and drops an image in one cycle ships its
        // placements but not its bytes, so they go even when it is absent.
        self.placements.retain(|p| p.image_id != id);
        self.virtual_placements.retain(|v| v.image_id != id);
        if let Some(entry) = self.images.remove(&id) {
            self.retained -= Self::bytes_of(&entry);
            self.image_order.retain(|x| *x != id);
            self.dirty_images.remove(&id);
        }
    }

    fn apply_placement(&mut self, placement: ClientPlacement) {
        let key = (placement.image_id, placement.placement_id);
        if let Some(slot) = self
            .placements
            .iter_mut()
            .find(|p| (p.image_id, p.placement_id) == key)
        {
            *slot = placement;
        } else {
            self.placements.push(placement);
        }
    }

    /// One `(image_id, placement_id)` names a direct or a virtual
    /// placement, so the removal looks in both.
    fn apply_placement_removed(&mut self, image_id: ImageId, placement_id: Option<PlacementId>) {
        self.placements
            .retain(|p| !(p.image_id == image_id && p.placement_id == placement_id));
        self.virtual_placements
            .retain(|v| !(v.image_id == image_id && v.placement_id == placement_id));
    }

    /// The rule of [`felis_grid::images::Placements::upsert_virtual`].
    fn apply_virtual_placement(&mut self, placement: VirtualPlacement) {
        if let Some(slot) = self.virtual_placements.iter_mut().find(|v| {
            v.image_id == placement.image_id
                && (placement.placement_id.is_none() || v.placement_id == placement.placement_id)
        }) {
            *slot = placement;
        } else {
            self.virtual_placements.push(placement);
        }
    }
}

impl LayoutImages for ImageShadow {
    fn drawable_size(&self, id: ImageId) -> Option<(u32, u32)> {
        let image = self.images.get(&id)?;
        image.drawable.then_some((image.width, image.height))
    }

    fn stack_rank(&self, id: ImageId) -> u64 {
        self.images.get(&id).map_or(u64::MAX, |image| image.rank)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most fixtures below are claims the mirror must admit, so a
    /// refusal is a test failure rather than something to thread
    /// through every line; the limit tests call `apply` directly.
    trait ApplyOk {
        fn apply_ok(&mut self, msg: &ImageMsg);
    }

    impl ApplyOk for ImageShadow {
        fn apply_ok(&mut self, msg: &ImageMsg) {
            self.apply(msg).unwrap();
        }
    }

    /// A `total / 4 × 1` RGBA image, so `total` is the byte length of
    /// every buffer this image carries.
    fn header(id: u32, total: u64) -> ImageMsg {
        assert_eq!(total % 4, 0, "an RGBA row is four bytes per pixel");
        ImageMsg::Header {
            id: ImageId(id),
            target: ImageTarget::New {
                width: u32::try_from(total / 4).unwrap(),
                height: 1,
                format: ImageFormat::Rgba32,
                serial: None,
            },
        }
    }

    fn serial_header(id: u32, serial: u64) -> ImageMsg {
        ImageMsg::Header {
            id: ImageId(id),
            target: ImageTarget::New {
                width: 1,
                height: 1,
                format: ImageFormat::Rgba32,
                serial: Some(serial),
            },
        }
    }

    #[test]
    fn an_image_ranks_by_the_daemon_s_serial_whatever_order_it_arrives_in() {
        let mut s = ImageShadow::new();
        s.apply_ok(&serial_header(2, 8));
        close(&mut s, 2, 4);
        s.apply_ok(&serial_header(1, 3));
        close(&mut s, 1, 4);
        assert_eq!((s.stack_rank(ImageId(1)), s.stack_rank(ImageId(2))), (3, 8));
    }

    #[test]
    fn without_a_serial_an_image_ranks_by_first_arrival_and_a_resend_keeps_it() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(1, 4), vec![0; 4]);
        transfer(&mut s, &header(2, 4), vec![0; 4]);
        transfer(&mut s, &header(1, 8), vec![0; 8]);
        assert!(s.stack_rank(ImageId(1)) < s.stack_rank(ImageId(2)));
        s.apply_ok(&ImageMsg::Delete { id: ImageId(1) });
        transfer(&mut s, &header(1, 4), vec![0; 4]);
        assert!(
            s.stack_rank(ImageId(1)) > s.stack_rank(ImageId(2)),
            "a deleted id comes back as a new image"
        );
    }

    fn frame_header(id: u32, number: u32) -> ImageMsg {
        ImageMsg::Header {
            id: ImageId(id),
            target: ImageTarget::Frame {
                number: NonZeroU32::new(number).unwrap(),
            },
        }
    }

    fn chunk(id: u32, bytes: Vec<u8>) -> ImageMsg {
        ImageMsg::Chunk {
            id: ImageId(id),
            bytes: bytes.into(),
        }
    }

    fn complete(id: u32) -> ImageMsg {
        ImageMsg::Complete { id: ImageId(id) }
    }

    fn show_frame(id: u32, number: u32) -> ImageMsg {
        ImageMsg::ShowFrame {
            id: ImageId(id),
            number: NonZeroU32::new(number).unwrap(),
        }
    }

    fn close(s: &mut ImageShadow, id: u32, len: usize) {
        s.apply_ok(&chunk(id, vec![0u8; len]));
        s.apply_ok(&complete(id));
    }

    fn transfer(shadow: &mut ImageShadow, msg: &ImageMsg, pixels: Vec<u8>) {
        let id = match msg {
            ImageMsg::Header { id, .. } => id.0,
            _ => unreachable!("transfer starts at a header"),
        };
        shadow.apply_ok(msg);
        shadow.apply_ok(&chunk(id, pixels));
        shadow.apply_ok(&complete(id));
    }

    fn placement(image_id: u32, placement_id: Option<u32>, z: i32) -> ImageMsg {
        ImageMsg::Placement {
            image_id: ImageId(image_id),
            placement_id: placement_id.map(PlacementId),
            anchor_row: 1,
            anchor_col: 1,
            cols: 4,
            rows: 2,
            source: None,
            z_index: z,
            requested: None,
            serial: None,
        }
    }

    fn virtual_placement(image_id: u32, placement_id: Option<u32>, cols: u16) -> ImageMsg {
        ImageMsg::VirtualPlacement {
            image_id: ImageId(image_id),
            placement_id: placement_id.map(PlacementId),
            cols,
            rows: 2,
            z_index: 0,
        }
    }

    #[test]
    fn virtual_placement_upserts_one_extent_per_placement() {
        // Pins: a re-emission (rehydrate, alt-screen restore) overwrites
        // in place, or the renderer sizes tiles against a stale entry.
        let mut s = ImageShadow::new();
        s.apply_ok(&virtual_placement(12, Some(1), 4));
        s.apply_ok(&virtual_placement(13, Some(1), 6));
        s.apply_ok(&virtual_placement(12, Some(2), 5));
        s.apply_ok(&virtual_placement(12, Some(1), 8));
        let extents: Vec<_> = s
            .virtual_placements()
            .iter()
            .map(|v| (v.image_id, v.cols))
            .collect();
        assert_eq!(
            extents,
            vec![(ImageId(12), 8), (ImageId(13), 6), (ImageId(12), 5)]
        );
    }

    /// A minor-0 daemon sends no id and keeps one extent per image.
    #[test]
    fn a_virtual_placement_without_an_id_keeps_one_extent_per_image() {
        let mut s = ImageShadow::new();
        s.apply_ok(&virtual_placement(12, None, 4));
        s.apply_ok(&virtual_placement(12, None, 8));
        let extents: Vec<_> = s.virtual_placements().iter().map(|v| v.cols).collect();
        assert_eq!(extents, vec![8]);
    }

    #[test]
    fn a_placement_removal_drops_the_virtual_placement_it_names() {
        let mut s = ImageShadow::new();
        s.apply_ok(&virtual_placement(12, Some(1), 4));
        s.apply_ok(&virtual_placement(12, Some(2), 6));
        s.apply_ok(&ImageMsg::PlacementRemoved {
            image_id: ImageId(12),
            placement_id: Some(PlacementId(1)),
        });
        let extents: Vec<_> = s.virtual_placements().iter().map(|v| v.cols).collect();
        assert_eq!(extents, vec![6]);
    }

    /// A daemon restored from a dump written before virtual placements
    /// had ids names one by its image alone.
    #[test]
    fn a_placement_removal_without_an_id_drops_a_virtual_placement_without_one() {
        let mut s = ImageShadow::new();
        s.apply_ok(&virtual_placement(12, None, 4));
        s.apply_ok(&ImageMsg::PlacementRemoved {
            image_id: ImageId(12),
            placement_id: None,
        });
        assert_eq!(s.virtual_placements(), []);
    }

    #[test]
    fn a_delete_drops_the_placements_of_an_image_never_received() {
        let mut s = ImageShadow::new();
        s.apply_ok(&placement(12, Some(1), 0));
        s.apply_ok(&virtual_placement(12, Some(2), 4));
        s.apply_ok(&ImageMsg::Delete { id: ImageId(12) });
        assert!(s.is_empty());
    }

    #[test]
    fn delete_drops_the_virtual_extent_with_the_image() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(12, 16), vec![0xAA; 16]);
        s.apply_ok(&virtual_placement(12, Some(1), 4));
        s.apply_ok(&ImageMsg::Delete { id: ImageId(12) });
        assert_eq!(s.virtual_placements(), []);
        assert!(s.is_empty(), "nothing may survive the image");
    }

    #[test]
    fn header_chunk_complete_fills_buffer_and_marks_complete() {
        let mut s = ImageShadow::new();
        s.apply_ok(&header(7, 16));
        s.apply_ok(&chunk(7, vec![0xAA; 8]));
        s.apply_ok(&chunk(7, vec![0xBB; 8]));
        s.apply_ok(&complete(7));
        let img = s.image(ImageId(7)).expect("image present after Complete");
        assert!(img.is_complete(), "Complete flips the renderable flag");
        assert_eq!(img.pixels().len(), 16);
        assert_eq!(&img.pixels()[0..8], &[0xAA; 8]);
        assert_eq!(&img.pixels()[8..16], &[0xBB; 8], "chunks append in order");
    }

    #[test]
    fn complete_pushes_id_into_dirty_set() {
        // Pins: `Complete` is the upload gate, not `Header`.
        let mut s = ImageShadow::new();
        s.apply_ok(&header(3, 4));
        assert!(
            s.take_dirty_images().is_empty(),
            "Header alone is not dirty"
        );
        s.apply_ok(&chunk(3, vec![0xCC; 4]));
        assert!(s.take_dirty_images().is_empty(), "Chunk alone is not dirty");
        s.apply_ok(&complete(3));
        assert_eq!(s.take_dirty_images(), vec![ImageId(3)]);
        assert_eq!(s.take_dirty_images(), Vec::<ImageId>::new());
    }

    #[test]
    fn dirty_ids_drain_in_insertion_order_not_hash_order() {
        // Pins: the drain follows `image_order`, not `HashSet` iteration.
        let mut s = ImageShadow::new();
        for id in [42u32, 7, 19, 3, 91] {
            transfer(&mut s, &header(id, 4), vec![0; 4]);
        }
        let drained = s.take_dirty_images();
        assert_eq!(
            drained,
            vec![
                ImageId(42),
                ImageId(7),
                ImageId(19),
                ImageId(3),
                ImageId(91)
            ],
            "dirty drain must follow insertion order",
        );
    }

    #[test]
    fn delete_drops_image_and_its_placements() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(5, 4), vec![0; 4]);
        s.apply_ok(&placement(5, Some(1), 0));
        s.apply_ok(&placement(5, Some(2), 0));
        s.apply_ok(&ImageMsg::Delete { id: ImageId(5) });
        assert!(s.image(ImageId(5)).is_none());
        assert!(
            s.placements().is_empty(),
            "every placement referencing the deleted id is dropped",
        );
    }

    #[test]
    fn placement_upsert_replaces_same_key() {
        // Mirrors `felis_grid::images::Placements::upsert`.
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(11, 4), vec![0; 4]);
        s.apply_ok(&placement(11, Some(1), 0));
        s.apply_ok(&placement(11, Some(1), 5)); // same key, new z
        assert_eq!(s.placements().len(), 1);
        assert_eq!(s.placements()[0].z_index, 5);
    }

    /// Pins: anchors shift into scrollback coordinates (negative rows)
    /// and entries survive until an explicit `PlacementRemoved`.
    #[test]
    fn placements_shifted_moves_anchors_and_keeps_scrolled_out_entries() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(4, 4), vec![0; 4]);
        s.apply_ok(&placement(4, None, 0)); // anchor_row: 1
        s.apply_ok(&ImageMsg::PlacementsShifted { lines: 3 });
        assert_eq!(
            s.placements()[0].anchor_row,
            -2,
            "anchor must shift into scrollback coordinates, not clamp or drop",
        );
        s.apply_ok(&ImageMsg::PlacementsShifted { lines: 5 });
        assert_eq!(s.placements()[0].anchor_row, -7);
        assert_eq!(s.placements().len(), 1);
        s.apply_ok(&ImageMsg::PlacementRemoved {
            image_id: ImageId(4),
            placement_id: None,
        });
        assert_eq!(s.placements(), []);
    }

    #[test]
    fn placement_removed_drops_only_the_matching_key() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(2, 4), vec![0; 4]);
        s.apply_ok(&placement(2, Some(1), 0));
        s.apply_ok(&placement(2, Some(2), 0));
        s.apply_ok(&placement(2, None, 0));
        s.apply_ok(&ImageMsg::PlacementRemoved {
            image_id: ImageId(2),
            placement_id: Some(PlacementId(2)),
        });
        let keys: Vec<_> = s.placements().iter().map(|p| p.placement_id).collect();
        assert_eq!(keys, vec![Some(PlacementId(1)), None]);
    }

    #[test]
    fn a_fresh_header_replaces_the_image_and_clears_dirty() {
        // Pins: the `New` target is a re-transmission, not an edit.
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(8, 4), vec![0xFF; 4]);
        transfer(&mut s, &frame_header(8, 2), vec![0xEE; 4]);
        assert_ne!(s.take_dirty_images(), Vec::<ImageId>::new());

        s.apply_ok(&header(8, 8));
        let img = s.image(ImageId(8)).unwrap();
        assert_eq!(img.pixels().len(), 8);
        assert!(!img.is_complete());
        assert_eq!(img.frame_count(), 1, "replacement resets to a single frame");
        assert!(
            s.take_dirty_images().is_empty(),
            "the replacement header un-dirties the id",
        );
    }

    #[test]
    fn frame_transmission_appends_and_show_frame_switches_current() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(7, 4), vec![0xAA; 4]);
        assert_eq!(s.take_dirty_images(), vec![ImageId(7)]);
        transfer(&mut s, &frame_header(7, 2), vec![0xBB; 4]);
        let img = s.image(ImageId(7)).unwrap();
        assert_eq!(img.frame_count(), 2);
        assert_eq!(img.current_frame(), 0, "still showing root until ShowFrame");
        assert_eq!(s.take_dirty_images(), Vec::<ImageId>::new());
        s.apply_ok(&show_frame(7, 2));
        let img = s.image(ImageId(7)).unwrap();
        assert_eq!(img.current_frame(), 1, "frame 2 on the wire is index 1");
        assert_eq!(
            img.pixels(),
            &[0xBB; 4],
            "pixels() follows the current frame"
        );
        assert_eq!(
            s.take_dirty_images(),
            vec![ImageId(7)],
            "ShowFrame re-dirties so the renderer re-uploads the new frame",
        );
    }

    #[test]
    fn show_frame_out_of_range_or_unchanged_is_a_noop() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(1, 4), vec![0; 4]);
        s.take_dirty_images();
        s.apply_ok(&show_frame(1, 1)); // already current
        s.apply_ok(&show_frame(1, 9)); // never transferred
        assert_eq!(s.take_dirty_images(), Vec::<ImageId>::new());
        assert_eq!(s.image(ImageId(1)).unwrap().current_frame(), 0);
    }

    #[test]
    fn show_frame_naming_a_frame_still_in_transfer_is_a_noop() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(1, 4), vec![0xAA; 4]);
        s.apply_ok(&frame_header(1, 2));
        s.take_dirty_images();
        s.apply_ok(&show_frame(1, 2));
        assert_eq!(s.take_dirty_images(), Vec::<ImageId>::new());
        assert_eq!(
            s.image(ImageId(1)).unwrap().current_frame(),
            0,
            "an open frame is the zero fill its header allocated",
        );
    }

    #[test]
    fn editing_the_root_frame_redirties_without_dropping_frames() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(2, 4), vec![0xAA; 4]);
        transfer(&mut s, &frame_header(2, 2), vec![0xBB; 4]);
        s.take_dirty_images();
        transfer(&mut s, &frame_header(2, 1), vec![0x11; 4]);
        assert_eq!(s.take_dirty_images(), vec![ImageId(2)]);
        let img = s.image(ImageId(2)).unwrap();
        assert_eq!(img.pixels(), &[0x11; 4]);
        assert_eq!(img.frame_count(), 2, "a root edit keeps the frame list");
    }

    #[test]
    fn a_frame_number_past_the_append_point_is_refused() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(1, 4), vec![0; 4]);
        let err = s.apply(&frame_header(1, 3)).unwrap_err();
        assert_eq!(
            err,
            ImageShadowError::FrameNumberSkipsAhead {
                id: 1,
                number: 3,
                have: 1,
            }
        );
        assert_eq!(s.image(ImageId(1)).unwrap().frame_count(), 1);
    }

    #[test]
    fn a_frame_header_for_an_image_the_mirror_lacks_is_refused() {
        let mut s = ImageShadow::new();
        assert_eq!(
            s.apply(&frame_header(99, 1)).unwrap_err(),
            ImageShadowError::FrameForUnknownImage { id: 99 }
        );
    }

    #[test]
    fn a_frame_number_past_the_cap_is_refused() {
        let mut s = ImageShadow::new();
        for number in [u32::try_from(MAX_IMAGE_FRAMES).unwrap() + 1, u32::MAX] {
            assert_eq!(
                s.apply(&frame_header(1, number)).unwrap_err(),
                ImageShadowError::FrameIndex { id: 1, number }
            );
        }
    }

    #[test]
    fn pixel_bytes_outside_an_open_transfer_are_refused() {
        let mut s = ImageShadow::new();
        assert_eq!(
            s.apply(&chunk(1, vec![0; 4])).unwrap_err(),
            ImageShadowError::ChunkWithoutHeader { id: 1 }
        );
        assert_eq!(
            s.apply(&complete(1)).unwrap_err(),
            ImageShadowError::CompleteWithoutHeader { id: 1 }
        );
        transfer(&mut s, &header(1, 4), vec![0; 4]);
        assert_eq!(
            s.apply(&chunk(1, vec![0; 4])).unwrap_err(),
            ImageShadowError::ChunkWithoutHeader { id: 1 }
        );
    }

    #[test]
    fn a_second_header_while_a_transfer_is_open_is_refused() {
        let mut s = ImageShadow::new();
        s.apply_ok(&header(1, 4));
        assert_eq!(
            s.apply(&header(2, 4)).unwrap_err(),
            ImageShadowError::HeaderWhileActive { id: 2, open: 1 }
        );
        assert!(s.image(ImageId(2)).is_none());
        assert_eq!(
            s.apply(&frame_header(1, 1)).unwrap_err(),
            ImageShadowError::HeaderWhileActive { id: 1, open: 1 }
        );
    }

    #[test]
    fn a_chunk_or_completion_naming_another_image_is_refused() {
        let mut s = ImageShadow::new();
        s.apply_ok(&header(1, 4));
        assert_eq!(
            s.apply(&chunk(2, vec![0; 4])).unwrap_err(),
            ImageShadowError::TransferIdMismatch { open: 1, named: 2 }
        );
        assert_eq!(
            s.apply(&complete(2)).unwrap_err(),
            ImageShadowError::TransferIdMismatch { open: 1, named: 2 }
        );
    }

    #[test]
    fn a_transfer_that_does_not_deliver_its_exact_byte_count_is_refused() {
        let mut s = ImageShadow::new();
        s.apply_ok(&header(1, 8));
        s.apply_ok(&chunk(1, vec![0xAA; 4]));
        assert_eq!(
            s.apply(&complete(1)).unwrap_err(),
            ImageShadowError::IncompleteComplete {
                id: 1,
                expected: 8,
                received: 4,
            }
        );
        assert_eq!(
            s.apply(&chunk(1, vec![0xBB; 5])).unwrap_err(),
            ImageShadowError::ChunkOverrun {
                id: 1,
                expected: 8,
                received: 9,
            }
        );
        assert!(
            !s.image(ImageId(1)).unwrap().is_complete(),
            "neither refusal may have marked the buffer renderable",
        );
    }

    #[test]
    fn a_zero_length_chunk_is_a_no_op_the_transfer_survives() {
        // The producer never emits one, but accepting it costs nothing
        // and refusing it would make a keepalive fatal.
        let mut s = ImageShadow::new();
        s.apply_ok(&header(1, 4));
        s.apply_ok(&chunk(1, Vec::new()));
        s.apply_ok(&chunk(1, vec![0xAA; 4]));
        s.apply_ok(&chunk(1, Vec::new()));
        s.apply_ok(&complete(1));
        assert!(s.image(ImageId(1)).unwrap().is_complete());
    }

    #[test]
    fn deleting_the_image_under_an_open_transfer_aborts_it() {
        let mut s = ImageShadow::new();
        s.apply_ok(&header(1, 4));
        s.apply_ok(&ImageMsg::Delete { id: ImageId(1) });
        assert_eq!(
            s.apply(&chunk(1, vec![0; 4])).unwrap_err(),
            ImageShadowError::ChunkWithoutHeader { id: 1 },
        );
        transfer(&mut s, &header(1, 4), vec![0xAA; 4]);
        assert!(s.image(ImageId(1)).unwrap().is_complete());
    }

    /// What a one-frame entry costs the aggregate: the pixels it claims
    /// plus the records the mirror is billed for alongside them.
    fn entry_cost(pixels: usize) -> usize {
        ENTRY_OVERHEAD + FRAME_OVERHEAD + pixels
    }

    /// The per-image cap is admitted exactly; one pixel past it leaves
    /// the mirror untouched, which is the point: a refusal that had
    /// already inserted a half-entry would be a partial mutation the
    /// connection teardown could not undo.
    #[test]
    fn a_header_over_the_per_image_cap_is_refused_without_touching_the_mirror() {
        let mut s = ImageShadow::new();
        s.apply_ok(&header(1, MAX_IMAGE_BYTES));
        close(&mut s, 1, MAX_IMAGE_BYTES as usize);
        assert_eq!(s.retained_bytes(), entry_cost(MAX_IMAGE_BYTES as usize));

        let err = s.apply(&header(2, MAX_IMAGE_BYTES + 4)).unwrap_err();
        assert_eq!(
            err,
            ImageShadowError::ImageBytes {
                id: 2,
                claimed: MAX_IMAGE_BYTES + 4,
            }
        );
        assert!(s.image(ImageId(2)).is_none());
        assert_eq!(s.retained_bytes(), entry_cost(MAX_IMAGE_BYTES as usize));
    }

    /// The aggregate is the mirror's own bound, not an echo of the
    /// daemon's: it is what stops a sequence of individually-legal
    /// headers from adding up past the session store's size.
    #[test]
    fn the_session_aggregate_admits_its_last_bytes_and_refuses_the_next() {
        let mut s = ImageShadow::new();
        let per_image = entry_cost(MAX_IMAGE_BYTES as usize);
        let count = MAX_SESSION_IMAGE_BYTES / per_image;
        for id in 0..count {
            s.apply_ok(&header(id as u32, MAX_IMAGE_BYTES));
            close(&mut s, id as u32, MAX_IMAGE_BYTES as usize);
        }
        let used = count * per_image;
        assert_eq!(s.retained_bytes(), used);

        // The largest claim that still fits, rounded to a whole pixel.
        let last = (MAX_SESSION_IMAGE_BYTES - used - (ENTRY_OVERHEAD + FRAME_OVERHEAD)) & !3;
        s.apply_ok(&header(8, last as u64));
        close(&mut s, 8, last);
        assert_eq!(s.retained_bytes(), used + entry_cost(last));
        assert!(s.retained_bytes() <= MAX_SESSION_IMAGE_BYTES);

        assert!(matches!(
            s.apply(&header(9, 4)).unwrap_err(),
            ImageShadowError::SessionBytes { id: 9, .. }
        ));
        assert!(s.image(ImageId(9)).is_none());

        // A replacement under a live id spends only the difference, or
        // no full store could ever be re-transmitted.
        s.apply_ok(&header(0, MAX_IMAGE_BYTES));
        close(&mut s, 0, MAX_IMAGE_BYTES as usize);
        s.apply_ok(&header(0, 4));
        close(&mut s, 0, 4);
        assert_eq!(
            s.retained_bytes(),
            used - per_image + entry_cost(last) + entry_cost(4)
        );

        s.apply_ok(&ImageMsg::Delete { id: ImageId(1) });
        assert_eq!(
            s.retained_bytes(),
            used - 2 * per_image + entry_cost(last) + entry_cost(4),
            "a delete releases the whole entry's bytes",
        );
    }

    fn generation(s: &ImageShadow, id: u32) -> u64 {
        s.image(ImageId(id)).expect("image held").generation()
    }

    fn drawable(s: &ImageShadow, id: u32) -> Option<(u32, u32)> {
        s.drawable_size(ImageId(id))
    }

    #[test]
    fn an_image_draws_only_once_its_displayed_frame_completes() {
        let mut s = ImageShadow::new();
        s.apply_ok(&header(1, 8));
        assert_eq!(drawable(&s, 1), None);
        let opened = generation(&s, 1);
        close(&mut s, 1, 8);
        assert_eq!(drawable(&s, 1), Some((2, 1)));
        assert!(generation(&s, 1) > opened);
    }

    /// kitty frees an image's pixels when a same-id transmission starts,
    /// so the image vanishes until that transmission completes, and a
    /// conversion started before it fails the generation check.
    #[test]
    fn a_replacement_header_hides_the_image_until_its_complete() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(1, 8), vec![0u8; 8]);
        let shown = generation(&s, 1);
        s.apply_ok(&header(1, 16));
        assert_eq!(drawable(&s, 1), None);
        let replacing = generation(&s, 1);
        assert!(replacing > shown);
        close(&mut s, 1, 16);
        assert_eq!(drawable(&s, 1), Some((4, 1)));
        assert!(generation(&s, 1) > replacing);
    }

    #[test]
    fn a_frame_transfer_off_screen_leaves_the_displayed_image_untouched() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(1, 8), vec![0u8; 8]);
        let shown = generation(&s, 1);
        s.apply_ok(&frame_header(1, 2));
        assert_eq!((drawable(&s, 1), generation(&s, 1)), (Some((2, 1)), shown));
        s.apply_ok(&chunk(1, vec![0u8; 8]));
        assert_eq!((drawable(&s, 1), generation(&s, 1)), (Some((2, 1)), shown));
        s.apply_ok(&complete(1));
        assert_eq!((drawable(&s, 1), generation(&s, 1)), (Some((2, 1)), shown));
        s.apply_ok(&show_frame(1, 2));
        assert!(generation(&s, 1) > shown);
        assert_eq!(drawable(&s, 1), Some((2, 1)));
    }

    /// Until the new pixels complete, the frame keeps showing, and an
    /// atlas reset that re-reads it must find what was on screen.
    #[test]
    fn re_sending_the_displayed_frame_keeps_its_pixels_until_complete() {
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(1, 4), vec![1, 2, 3, 4]);
        s.take_dirty_images();
        let shown = generation(&s, 1);
        let before = s.retained_bytes();
        s.apply_ok(&frame_header(1, 1));
        s.apply_ok(&chunk(1, vec![5, 6, 7, 8]));
        let image = s.image(ImageId(1)).unwrap();
        assert_eq!(image.pixels(), [1, 2, 3, 4]);
        assert_eq!((drawable(&s, 1), generation(&s, 1)), (Some((1, 1)), shown));
        assert_eq!(s.take_dirty_images(), []);
        s.apply_ok(&complete(1));
        assert_eq!(s.image(ImageId(1)).unwrap().pixels(), [5, 6, 7, 8]);
        assert!(generation(&s, 1) > shown);
        assert_eq!(s.take_dirty_images(), [ImageId(1)]);
        assert_eq!(s.retained_bytes(), before);
    }

    /// The daemon edits a frame in place and admits the edit at the cap,
    /// so the mirror must not refuse it for the copy it stages.
    #[test]
    fn re_sending_the_displayed_frame_is_admitted_at_the_session_cap() {
        let mut s = ImageShadow::new();
        let per_image = entry_cost(MAX_IMAGE_BYTES as usize);
        let count = MAX_SESSION_IMAGE_BYTES / per_image;
        for id in 0..count {
            s.apply_ok(&header(id as u32, MAX_IMAGE_BYTES));
            close(&mut s, id as u32, MAX_IMAGE_BYTES as usize);
        }
        let full = s.retained_bytes();
        assert!(full + per_image > MAX_SESSION_IMAGE_BYTES);
        s.apply_ok(&frame_header(0, 1));
        assert_eq!(s.retained_bytes(), full);
        close(&mut s, 0, MAX_IMAGE_BYTES as usize);
        assert_eq!(s.retained_bytes(), full);
        s.apply_ok(&frame_header(0, 1));
        s.apply_ok(&ImageMsg::Delete { id: ImageId(0) });
        assert_eq!(s.retained_bytes(), full - per_image);
    }

    #[test]
    fn generations_never_repeat_across_a_new_shadow() {
        let mut old = ImageShadow::new();
        transfer(&mut old, &header(1, 4), vec![0u8; 4]);
        let mut new = ImageShadow::new();
        transfer(&mut new, &header(1, 4), vec![0u8; 4]);
        assert_ne!(generation(&old, 1), generation(&new, 1));
    }

    #[test]
    fn rgb_pixels_widen_to_opaque_rgba() {
        let mut s = ImageShadow::new();
        let rgb = ImageMsg::Header {
            id: ImageId(1),
            target: ImageTarget::New {
                width: 2,
                height: 1,
                format: ImageFormat::Rgb24,
                serial: None,
            },
        };
        transfer(&mut s, &rgb, vec![1, 2, 3, 4, 5, 6]);
        let mut out = vec![9; 3];
        s.image(ImageId(1)).unwrap().write_rgba8(&mut out);
        assert_eq!(out, [1, 2, 3, 0xFF, 4, 5, 6, 0xFF]);

        transfer(&mut s, &header(2, 4), vec![7, 8, 9, 10]);
        s.image(ImageId(2)).unwrap().write_rgba8(&mut out);
        assert_eq!(out, [7, 8, 9, 10]);
    }

    #[test]
    fn layout_skips_an_image_mid_replacement() {
        let screen = felis_grid::Grid::new(4, 4);
        let mut s = ImageShadow::new();
        transfer(&mut s, &header(1, 8), vec![0u8; 8]);
        s.apply_ok(&placement(1, None, 0));
        let mut layout = ImageLayout::new();
        s.layout_into(&mut layout, screen.screen(), (8.0, 16.0), 0);
        assert_eq!(layout.above_text().len(), 1);
        s.apply_ok(&header(1, 16));
        s.layout_into(&mut layout, screen.screen(), (8.0, 16.0), 0);
        assert_eq!(layout.above_text(), []);
        close(&mut s, 1, 16);
        s.layout_into(&mut layout, screen.screen(), (8.0, 16.0), 0);
        assert_eq!(layout.above_text()[0].image_size, [4, 1]);
    }
}

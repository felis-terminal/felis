//! GPU-side image atlas for Kitty graphics placements.
//!
//! Texture format is `Rgba8UnormSrgb`, unlike the glyph atlas: Kitty payloads decode in
//! producer-sRGB space, so the sampler decoding to linear preserves gamma against cell passes.
//! Eviction is reset-based when shelf room runs out.

use std::{collections::HashMap, num::NonZeroU32};

use felis_protocol::{ImageId, messages::ImageFormat};
use wgpu::{
    BindGroup, BindGroupLayout, Device, Sampler, Texture, TextureFormat, TextureViewDescriptor,
};

use crate::{
    atlas::{Allocation, ShelfAtlas},
    buffer_ring::UploadMode,
    gpu_resources,
    texture_upload::{TexelLayout, TextureUploader},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageSlot {
    /// Atlas UV min (top-left) in `[0, 1]`.
    pub uv_min: [f32; 2],
    /// Atlas UV max (bottom-right) in `[0, 1]`.
    pub uv_max: [f32; 2],
    /// Image width in pixels (whole image, not just the placement
    /// sub-rect).
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub struct ImageIndex {
    atlas: ShelfAtlas,
    slots: HashMap<ImageId, ImageSlot>,
}

impl ImageIndex {
    #[must_use]
    pub fn new(atlas_side: NonZeroU32) -> Self {
        Self {
            atlas: ShelfAtlas::new(atlas_side, atlas_side),
            slots: HashMap::new(),
        }
    }

    #[must_use]
    pub const fn atlas_side(&self) -> u32 {
        self.atlas.width()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub fn slot(&self, id: ImageId) -> Option<ImageSlot> {
        self.slots.get(&id).copied()
    }

    /// `None` when the atlas has no room; the caller resets before
    /// retrying. Re-allocating an id replaces its slot but leaves the
    /// old shelf rectangle occupied until a reset.
    pub fn allocate(&mut self, id: ImageId, width: u32, height: u32) -> Option<ImageSlot> {
        // A degenerate slot lets the renderer resolve a 0x0 image and
        // emit nothing, instead of treating a producer's empty buffer as
        // a full atlas.
        if width == 0 || height == 0 {
            let slot = ImageSlot {
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                width,
                height,
            };
            self.slots.insert(id, slot);
            return Some(slot);
        }
        let alloc = self.atlas.alloc(width, height)?;
        let slot = uv_for(&alloc, self.atlas.width(), self.atlas.height());
        self.slots.insert(id, slot);
        Some(slot)
    }

    pub fn forget(&mut self, id: ImageId) {
        self.slots.remove(&id);
    }

    pub fn reset(&mut self) {
        self.atlas.reset();
        self.slots.clear();
    }
}

fn uv_for(alloc: &Allocation, atlas_w: u32, atlas_h: u32) -> ImageSlot {
    let aw = atlas_w as f32;
    let ah = atlas_h as f32;
    ImageSlot {
        uv_min: [alloc.x as f32 / aw, alloc.y as f32 / ah],
        uv_max: [
            (alloc.x + alloc.width) as f32 / aw,
            (alloc.y + alloc.height) as f32 / ah,
        ],
        width: alloc.width,
        height: alloc.height,
    }
}

/// Created on the first upload rather than at startup, so an
/// image-free session never allocates the (up to 256 MiB) sheet.
struct ImageAtlasGpu {
    texture: Texture,
    bind_group: BindGroup,
}

pub struct ImageAtlas {
    index: ImageIndex,
    layout: BindGroupLayout,
    sampler: Sampler,
    gpu: Option<ImageAtlasGpu>,
    upload_mode: UploadMode,
    /// Reused across frames: a video producer (mpv `t=s`) re-uploads a
    /// multi-megabyte frame 24-30 times a second, and a fresh `Vec` per
    /// frame page-faults ~15 MiB each time.
    upload_scratch: Vec<u8>,
}

impl ImageAtlas {
    pub(crate) fn new(
        device: &Device,
        upload_mode: UploadMode,
        layout: &BindGroupLayout,
        atlas_side: NonZeroU32,
    ) -> Self {
        let sampler = gpu_resources::create_atlas_sampler(device, "felis image sampler");
        Self {
            index: ImageIndex::new(atlas_side),
            layout: layout.clone(),
            sampler,
            gpu: None,
            upload_mode,
            upload_scratch: Vec::new(),
        }
    }

    fn ensure_gpu(&mut self, device: &Device) -> &ImageAtlasGpu {
        self.gpu.get_or_insert_with(|| {
            let texture = gpu_resources::create_atlas_texture(
                device,
                self.upload_mode,
                self.index.atlas_side(),
                TextureFormat::Rgba8UnormSrgb,
                "felis image atlas",
            );
            let view = texture.create_view(&TextureViewDescriptor::default());
            let bind_group = gpu_resources::create_atlas_bind_group(
                device,
                &self.layout,
                &view,
                &self.sampler,
                None,
                "felis image atlas bind group",
            );
            ImageAtlasGpu {
                texture,
                bind_group,
            }
        })
    }

    /// `None` until the first upload; the draw path only reaches for it
    /// inside an `image_count > 0` guard.
    #[must_use]
    pub fn bind_group(&self) -> Option<&BindGroup> {
        self.gpu.as_ref().map(|g| &g.bind_group)
    }

    #[must_use]
    pub const fn atlas_side(&self) -> u32 {
        self.index.atlas_side()
    }

    #[must_use]
    pub fn slot(&self, id: ImageId) -> Option<ImageSlot> {
        self.index.slot(id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn forget(&mut self, id: ImageId) {
        self.index.forget(id);
    }

    /// `None` when the atlas is full; the caller resets and re-uploads
    /// everything.
    pub(crate) fn upload(
        &mut self,
        device: &Device,
        uploader: &mut TextureUploader,
        id: ImageId,
        width: u32,
        height: u32,
        format: ImageFormat,
        pixels: &[u8],
    ) -> Option<ImageSlot> {
        let slot = match self.index.slot(id) {
            Some(existing)
                if existing.width == width
                    && existing.height == height
                    && (existing.uv_max[0] - existing.uv_min[0]).abs() > f32::EPSILON =>
            {
                existing
            }
            _ => {
                self.index.forget(id);
                self.index.allocate(id, width, height)?
            }
        };
        // Before `ensure_gpu`, so a producer that only ever transmits
        // empty buffers never forces the texture allocation.
        if slot.width == 0 || slot.height == 0 {
            return Some(slot);
        }
        let atlas_w = self.index.atlas_side();
        let origin_x = (slot.uv_min[0] * atlas_w as f32).round() as u32;
        let origin_y = (slot.uv_min[1] * atlas_w as f32).round() as u32;
        self.ensure_gpu(device);
        let Some(gpu) = self.gpu.as_ref() else {
            // `ensure_gpu` always populates `self.gpu`; skipping the upload
            // beats an `expect` (the slot stays valid and the next dirty
            // pass re-uploads).
            return Some(slot);
        };
        uploader.write(
            &gpu.texture,
            TexelLayout::Rgba8,
            [origin_x, origin_y],
            [width, height],
            rgba_pixels(format, width, height, pixels, &mut self.upload_scratch),
            width * 4,
        );
        Some(slot)
    }

    /// The texture's pixels are not zeroed; the caller re-uploads every
    /// still-live image before drawing.
    pub fn reset(&mut self) {
        self.index.reset();
    }
}

/// `src` as `Rgba32`: `Rgb24` expands into `scratch` with `α = 0xFF`, and `Rgba32` is
/// borrowed as is.
///
/// Uses fixed-stride chunks to vectorize. A payload shorter than `width * height * 3` leaves
/// the tail zero-filled so truncated frames show black instead of panicking.
pub fn rgba_pixels<'a>(
    format: ImageFormat,
    width: u32,
    height: u32,
    src: &'a [u8],
    scratch: &'a mut Vec<u8>,
) -> &'a [u8] {
    const RGB: usize = ImageFormat::Rgb24.bytes_per_pixel();
    const RGBA: usize = ImageFormat::Rgba32.bytes_per_pixel();

    match format {
        ImageFormat::Rgba32 => src,
        ImageFormat::Rgb24 => {
            let pixel_count = (width as usize) * (height as usize);
            scratch.clear();
            scratch.resize(pixel_count * RGBA, 0);
            for (s, d) in src
                .as_chunks::<RGB>()
                .0
                .iter()
                .zip(scratch.as_chunks_mut::<RGBA>().0)
                .take(pixel_count)
            {
                d[0] = s[0];
                d[1] = s[1];
                d[2] = s[2];
                d[3] = 0xFF;
            }
            scratch
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn nz(v: u32) -> NonZeroU32 {
        NonZeroU32::new(v).unwrap()
    }

    #[test]
    fn fresh_index_is_empty() {
        let idx = ImageIndex::new(nz(256));
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(idx.slot(ImageId(42)).is_none());
    }

    #[test]
    fn allocate_records_slot_with_uvs_in_unit_square() {
        let mut idx = ImageIndex::new(nz(256));
        let slot = idx.allocate(ImageId(7), 64, 32).unwrap();
        assert!(slot.uv_min[0] >= 0.0 && slot.uv_max[0] <= 1.0);
        assert!(slot.uv_min[1] >= 0.0 && slot.uv_max[1] <= 1.0);
        assert_eq!(slot.width, 64);
        assert_eq!(slot.height, 32);
        assert_eq!(idx.slot(ImageId(7)), Some(slot));
    }

    #[test]
    fn reallocation_under_same_id_returns_a_fresh_slot() {
        let mut idx = ImageIndex::new(nz(256));
        let first = idx.allocate(ImageId(1), 32, 32).unwrap();
        let second = idx.allocate(ImageId(1), 64, 64).unwrap();
        assert_ne!(first, second, "new dimensions land in a new rect");
        assert_eq!(
            idx.slot(ImageId(1)),
            Some(second),
            "only the latest slot survives"
        );
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn allocate_returns_none_when_atlas_is_full() {
        let mut idx = ImageIndex::new(nz(64));
        assert!(idx.allocate(ImageId(1), 64, 64).is_some());
        assert!(
            idx.allocate(ImageId(2), 64, 64).is_none(),
            "no shelves left"
        );
        assert!(idx.slot(ImageId(1)).is_some());
        assert!(idx.slot(ImageId(2)).is_none());
    }

    /// When a live image's allocation fails on a fragmented sheet (the
    /// packer never defragments), reset plus re-allocating the whole
    /// live set restores a slot for every image, the already-resident
    /// ones included.
    #[test]
    fn reset_then_reallocating_the_live_set_restores_every_slot() {
        let mut idx = ImageIndex::new(nz(64));
        idx.allocate(ImageId(1), 64, 50).unwrap();
        idx.forget(ImageId(1));

        let live = [(ImageId(2), 64u32, 10u32), (ImageId(3), 64, 10)];
        assert!(idx.allocate(ImageId(2), 64, 10).is_some());
        assert!(
            idx.allocate(ImageId(3), 64, 10).is_none(),
            "the stale shelf leaves no vertical budget"
        );

        idx.reset();
        for (id, w, h) in live {
            assert!(
                idx.allocate(id, w, h).is_some(),
                "the live set fits an empty sheet"
            );
        }
        assert!(idx.slot(ImageId(2)).is_some(), "the resident image is back");
        assert!(idx.slot(ImageId(3)).is_some(), "and so is the new one");
    }

    /// What makes "reset at most once per frame" safe: a second reset
    /// would free nothing this image could use.
    #[test]
    fn an_image_larger_than_the_sheet_still_fails_after_a_reset() {
        let mut idx = ImageIndex::new(nz(64));
        assert!(idx.allocate(ImageId(1), 128, 128).is_none());
        idx.reset();
        assert!(
            idx.allocate(ImageId(1), 128, 128).is_none(),
            "an empty sheet is no bigger than a full one"
        );
    }

    #[test]
    fn reset_clears_every_slot_and_makes_shelves_reusable() {
        let mut idx = ImageIndex::new(nz(64));
        idx.allocate(ImageId(1), 64, 64).unwrap();
        assert!(
            idx.allocate(ImageId(2), 1, 1).is_none(),
            "full before reset"
        );
        idx.reset();
        assert!(idx.is_empty());
        assert!(
            idx.allocate(ImageId(2), 1, 1).is_some(),
            "shelves reusable after reset"
        );
    }

    #[test]
    fn forget_drops_one_slot_without_resetting_others() {
        let mut idx = ImageIndex::new(nz(256));
        idx.allocate(ImageId(1), 8, 8).unwrap();
        idx.allocate(ImageId(2), 8, 8).unwrap();
        idx.forget(ImageId(1));
        assert!(idx.slot(ImageId(1)).is_none());
        assert!(idx.slot(ImageId(2)).is_some());
    }

    #[test]
    fn zero_size_image_resolves_to_degenerate_slot() {
        let mut idx = ImageIndex::new(nz(64));
        let slot = idx.allocate(ImageId(9), 0, 0).unwrap();
        assert_eq!(slot.width, 0);
        assert_eq!(slot.height, 0);
        assert_eq!(slot.uv_min, slot.uv_max);
    }

    /// The reference expansion: one RGBA pixel per cell of the declared
    /// `width * height`, with a short payload's tail left transparent
    /// black rather than panicking.
    fn expand_rgb(width: u32, height: u32, src: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; (width as usize) * (height as usize) * 4];
        for (px, chunk) in out
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src.as_chunks::<3>().0)
        {
            *px = [chunk[0], chunk[1], chunk[2], 0xFF];
        }
        out
    }

    proptest! {
        #[test]
        fn rgba_pixels_expands_rgb_and_passes_rgba_through(
            width in 0u32..8,
            height in 0u32..8,
            src in prop::collection::vec(any::<u8>(), 0..96),
            dirty in prop::collection::vec(any::<u8>(), 0..96),
        ) {
            let mut scratch = dirty;
            prop_assert_eq!(
                rgba_pixels(ImageFormat::Rgba32, width, height, &src, &mut scratch),
                &src[..]
            );

            let mut scratch = vec![0xAAu8; 7];
            prop_assert_eq!(
                rgba_pixels(ImageFormat::Rgb24, width, height, &src, &mut scratch),
                &expand_rgb(width, height, &src)[..]
            );
        }
    }
}

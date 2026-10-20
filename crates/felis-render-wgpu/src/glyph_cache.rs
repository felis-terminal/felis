//! The wgpu side of the glyph atlas: [`GlyphCache`] wraps the
//! renderer-neutral [`GlyphIndex`] with the atlas textures and drains its
//! pending uploads into them.

use std::num::NonZeroU32;

use felis_grid::ScreenBuffer;
use felis_render_core::{
    display_list::AtlasKind,
    glyphs::{
        AtlasView, ClusterGlyph, GlyphIndex, GlyphSlot, GridWalker, ShapeFrame, color_atlas_side,
    },
};
use felis_shaping::{CellMetrics, FontStack, GlyphId, GlyphPixels, Shaper, SizingKey};
use wgpu::{BindGroup, BindGroupLayout, Device, Texture, TextureFormat, TextureViewDescriptor};

use crate::{
    buffer_ring::UploadMode,
    gpu_resources,
    texture_upload::{TexelLayout, Texels, TextureUploader},
};

pub struct GlyphCache {
    index: GlyphIndex,
    texture: Texture,
    /// Allocated eagerly so the fg bind group is valid from frame one; a
    /// lazy texture would force a bind-group rebuild on the first emoji.
    color_texture: Texture,
    bind_group: BindGroup,
    walker: GridWalker,
}

impl GlyphCache {
    pub(crate) fn new(
        device: &Device,
        upload_mode: UploadMode,
        layout: &BindGroupLayout,
        stack: FontStack,
        font_size_physical_px: f32,
        atlas_side: NonZeroU32,
    ) -> Self {
        let texture = gpu_resources::create_atlas_texture(
            device,
            upload_mode,
            atlas_side.get(),
            TextureFormat::R8Unorm,
            "felis glyph atlas",
        );
        let view = texture.create_view(&TextureViewDescriptor::default());
        // sRGB RGBA, like the image atlas, so the alpha blend stays
        // gamma-correct against the sRGB surface.
        let color_side = color_atlas_side(atlas_side);
        let color_texture = gpu_resources::create_atlas_texture(
            device,
            upload_mode,
            color_side.get(),
            TextureFormat::Rgba8UnormSrgb,
            "felis glyph color atlas",
        );
        let color_view = color_texture.create_view(&TextureViewDescriptor::default());
        let sampler = gpu_resources::create_atlas_sampler(device, "felis atlas sampler");
        let bind_group = gpu_resources::create_atlas_bind_group(
            device,
            layout,
            &view,
            &sampler,
            Some(&color_view),
            "felis atlas bind group",
        );
        Self {
            index: GlyphIndex::new(stack, font_size_physical_px, atlas_side),
            texture,
            color_texture,
            bind_group,
            walker: GridWalker::default(),
        }
    }

    #[must_use]
    pub const fn cell_metrics(&self) -> CellMetrics {
        self.index.cell_metrics()
    }

    #[must_use]
    pub const fn font_size_physical_px(&self) -> f32 {
        self.index.font_size_physical_px()
    }

    /// Bind group for the fg pass, slot `@group(1)`.
    #[must_use]
    pub const fn bind_group(&self) -> &BindGroup {
        &self.bind_group
    }

    /// The texture stays resident; the index, shape cache and allocator
    /// reset, so the next `populate_grid` re-rasterizes everything.
    /// Stale pixels in unreached regions are harmless: the new allocator
    /// never hands them out.
    pub fn reload_font(&mut self, stack: FontStack, font_size_physical_px: f32) {
        self.rebuild_index_with(stack, font_size_physical_px);
    }

    /// Keeps the existing `FontStack`: zoom (Ctrl+= / Ctrl+- / wheel)
    /// would otherwise pay `load_system_fonts()` per keypress, which
    /// stutters on systems with many fonts.
    pub fn reload_font_size(&mut self, font_size_physical_px: f32) {
        let stack = self.index.stack().clone();
        self.rebuild_index_with(stack, font_size_physical_px);
    }

    fn rebuild_index_with(&mut self, stack: FontStack, font_size_physical_px: f32) {
        let atlas_side = NonZeroU32::new(self.index.atlas_side()).unwrap_or(NonZeroU32::MIN);
        self.index = GlyphIndex::new(stack, font_size_physical_px, atlas_side);
    }

    #[must_use]
    pub const fn atlas_was_reset(&self) -> bool {
        self.index.atlas_was_reset()
    }

    #[must_use]
    pub const fn atlas_resets(&self) -> u64 {
        self.index.atlas_resets()
    }

    /// Populates `frame` with shaped runs, clusters, and primed cells;
    /// [`GridWalker::walk`] decides which rows.
    pub(crate) fn populate_grid_with_features(
        &mut self,
        screen: &ScreenBuffer,
        shaper: &mut Shaper,
        features: &[String],
        uploader: &mut TextureUploader,
        frame: &mut ShapeFrame,
    ) {
        self.walker
            .walk(&mut self.index, screen, shaper, features, frame);
        self.flush_pending_uploads(uploader);
    }

    /// For off-screen text (the IME pre-edit overlay). Pre-edit has no
    /// OSC 66 surface, so every char is resident at default sizing.
    pub(crate) fn populate_chars(
        &mut self,
        chars: impl IntoIterator<Item = char>,
        uploader: &mut TextureUploader,
    ) {
        let cell_height = self.index.cell_metrics().height;
        for c in chars {
            self.index.ensure(c, cell_height, SizingKey::default());
        }
        self.flush_pending_uploads(uploader);
    }

    /// For overlay text laid out by [`felis_grid::text_cells`]: a lone
    /// scalar is primed as a char, a cluster is shaped.
    pub(crate) fn trim_overlay_clusters(&mut self) {
        self.index.trim_overlay_clusters();
    }

    pub(crate) fn populate_overlay_text(
        &mut self,
        text: &str,
        shaper: &mut Shaper,
        uploader: &mut TextureUploader,
    ) {
        let cell_height = self.index.cell_metrics().height;
        for (range, _) in felis_grid::text_cells(text) {
            let cluster = &text[range];
            let mut chars = cluster.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    self.index.ensure(c, cell_height, SizingKey::default());
                }
                _ => self.index.ensure_overlay_cluster(shaper, cluster),
            }
        }
        self.flush_pending_uploads(uploader);
    }

    fn flush_pending_uploads(&mut self, uploader: &mut TextureUploader) {
        for upload in self.index.drain_pending() {
            let (texture, layout, bytes_per_row) = match &upload.pixels {
                GlyphPixels::Rgba(_) => (
                    &self.color_texture,
                    TexelLayout::Rgba8,
                    upload.alloc.width * 4,
                ),
                GlyphPixels::Coverage(_) => (&self.texture, TexelLayout::R8, upload.alloc.width),
            };
            uploader.write(
                texture,
                layout,
                [upload.alloc.x, upload.alloc.y],
                [upload.alloc.width, upload.alloc.height],
                Texels::Packed {
                    bytes: upload.pixels.as_bytes(),
                    bytes_per_row,
                },
            );
        }
    }
}

impl AtlasView for GlyphCache {
    fn slot(&self, glyph: char, cell_height_px: u32, sizing: SizingKey) -> Option<GlyphSlot> {
        self.index.slot(glyph, cell_height_px, sizing)
    }

    fn glyph_id_slot(
        &self,
        glyph_id: GlyphId,
        cell_height_px: u32,
        sizing: SizingKey,
    ) -> Option<GlyphSlot> {
        self.index.glyph_id_slot(glyph_id, cell_height_px, sizing)
    }

    fn fitted_glyph_id_slot(
        &self,
        glyph_id: GlyphId,
        px: u16,
        cell_height_px: u32,
        sizing: SizingKey,
    ) -> Option<GlyphSlot> {
        self.index
            .fitted_glyph_id_slot(glyph_id, px, cell_height_px, sizing)
    }

    fn overlay_cluster(&self, text: &str) -> Option<&[ClusterGlyph]> {
        self.index.overlay_cluster(text)
    }

    fn sheet_side(&self, sheet: AtlasKind) -> u32 {
        self.index.sheet_side(sheet)
    }
}

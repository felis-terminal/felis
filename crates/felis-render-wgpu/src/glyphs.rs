//! Glyph cache that bridges `felis_shaping` with the GPU atlas:
//! [`GlyphIndex`] is the CPU bookkeeping (testable without an adapter),
//! [`GlyphCache`] wraps it with the wgpu texture and upload path.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use felis_grid::{Grapheme, ScreenBuffer};
use felis_shaping::{
    CellMetrics, Font, FontStack, FontStyle, GlyphBitmap, GlyphId, GlyphPixels, ShapeCache,
    ShapedGlyph, Shaper, SizingKey,
};
use foldhash::fast::RandomState;
use wgpu::{BindGroup, BindGroupLayout, Device, Texture, TextureFormat, TextureViewDescriptor};

use crate::{
    atlas::{Allocation, ShelfAtlas},
    box_drawing,
    buffer_ring::UploadMode,
    gpu_resources,
    instances::{AtlasView, GlyphSlot, font_style_of},
    texture_upload::{TexelLayout, TextureUploader},
};

/// Color-glyph atlas side ceiling. At four bytes per pixel an `Rgba8`
/// sheet half the mono side costs the same bytes as the mono `R8` sheet
/// and still holds hundreds of emoji before a reset recycles it.
pub const COLOR_GLYPH_ATLAS_SIDE: u32 = 1024;
const _: () = assert!(COLOR_GLYPH_ATLAS_SIDE < crate::GLYPH_ATLAS_SIDE);

/// Never larger than the mono sheet. Shared by [`GlyphIndex`] and
/// [`GlyphCache`] so their UV spaces agree.
#[must_use]
pub fn color_atlas_side(mono_side: NonZeroU32) -> NonZeroU32 {
    NonZeroU32::new(mono_side.get().min(COLOR_GLYPH_ATLAS_SIDE)).unwrap_or(mono_side)
}

/// Base scalar of a [`Grapheme::Cluster`] cell. The compositing pass,
/// `populate`'s priming, and the emission fallback all resolve through
/// this so they agree on the scalar. `None` while the handle is not yet
/// in the screen's cluster table (a transient reattach state).
#[must_use]
pub(crate) fn cluster_base_char(screen: &ScreenBuffer, id: NonZeroU32) -> Option<char> {
    screen.cluster_str(id).and_then(|s| s.chars().next())
}

#[derive(Debug)]
pub struct PendingUpload {
    pub alloc: Allocation,
    /// The variant picks the destination: `Coverage` goes to the mono
    /// atlas, `Rgba` to the color atlas.
    pub pixels: GlyphPixels,
}

/// The shaper collapses ligatures into glyph ids with no single source
/// `char`, so the shaped path cannot ride a `char`-keyed map; the
/// variant keeps the two key spaces apart in one map. A shaped run
/// claims every cell it covers, so no cell is populated under both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SlotKey {
    Char(char),
    Glyph(GlyphId),
    /// A cluster glyph rasterized at the pixel size its cluster fits at:
    /// a mark such as U+0301 is native in `é` and shrunk in `※́`.
    FittedGlyph(GlyphId, u16),
}

pub struct GlyphIndex {
    stack: FontStack,
    font_size_physical_px: f32,
    metrics: CellMetrics,
    shape: ShapeCache,
    atlas: ShelfAtlas,
    /// Separate RGBA shelf so color and mono allocations never
    /// interleave across textures with different bytes per texel.
    color_atlas: ShelfAtlas,
    /// `None` caches known blanks and glyphs too large for an empty sheet.
    /// The `SizingKey` isolates OSC 66 sized glyphs and faces
    /// ([`SizingKey::font_id`]). Not `FixedState` like felis-grid's intern
    /// tables: PTY output picks these keys, so a fixed seed would let a
    /// producer precompute keys sharing a bucket.
    slots: HashMap<(SlotKey, u32, SizingKey), Option<GlyphSlot>, RandomState>,
    /// `None` entries take no atlas space, so no atlas recycle bounds
    /// them; past [`Self::BLANK_SLOT_MAX`] they are swept instead.
    blank_slots: usize,
    /// Tiny glyphs fill a sheet only after millions of slots, so the
    /// sheets count as full at [`Self::PLACED_SLOT_MAX`] as well.
    placed_slots: usize,
    /// Run-level shape memo per [`FontStyle`] for borrow-keyed `&str` lookup.
    /// Text and style suffice as keys because font, size, and features are fixed.
    /// Avoids Swash cluster-analysis and GSUB costs on every frame.
    shaped_runs: [HashMap<String, Vec<ShapedGlyph>>; 4],
    /// Overlay clusters (pre-edit, bar labels) shaped like a grid cluster
    /// cell. Lives here so a font, size or feature reload drops it with
    /// the faces its glyph ids belong to.
    overlay_clusters: HashMap<String, Vec<ClusterGlyph>>,
    /// The fitted pixel size of each overrunning cluster, keyed by its
    /// text, block columns and shaped key: measuring rasterizes every
    /// glyph, which each repaint of a prompt would otherwise repeat.
    cluster_fits: HashMap<(String, u16, SizingKey), u16>,
    pending: Vec<PendingUpload>,
    primed: HashSet<(char, SizingKey), RandomState>,
    /// Set when a whole-atlas reset fires mid-walk: the slots the walk
    /// already handed out are gone, so the renderer walks again before
    /// building the frame's instances.
    atlas_reset_during_walk: bool,
    atlas_resets: u64,
    /// The last complete walk's key; a recycle drops it along with the
    /// slots, so the next walk covers every row.
    walked: Option<WalkKey>,
}

/// What a grid walk's result depends on besides the cells of the rows
/// it walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalkKey {
    rows: u16,
    cols: u16,
    shaped: bool,
}

/// The pixel box an overwide glyph or cluster is shrunk into.
#[derive(Debug, Clone, Copy)]
struct FitBlock {
    w: f32,
    h: f32,
    slack: f32,
}

impl FitBlock {
    /// The cell width rounds the primary advance, so a primary glyph
    /// overruns its block by up to half a pixel per unit of `s`.
    fn holds(self, advance: f32) -> bool {
        advance <= self.w + self.slack
    }

    fn ratio(self, ink_w: f32, ink_h: f32) -> f32 {
        (self.w / ink_w).min(self.h / ink_h).min(1.0)
    }
}

impl GlyphIndex {
    #[must_use]
    pub fn new(stack: FontStack, font_size_physical_px: f32, atlas_side: NonZeroU32) -> Self {
        let metrics = stack.primary().cell_metrics(font_size_physical_px);
        let color_side = color_atlas_side(atlas_side);
        Self {
            stack,
            font_size_physical_px,
            metrics,
            shape: ShapeCache::default(),
            atlas: ShelfAtlas::new(atlas_side, atlas_side),
            color_atlas: ShelfAtlas::new(color_side, color_side),
            slots: HashMap::default(),
            blank_slots: 0,
            placed_slots: 0,
            shaped_runs: std::array::from_fn(|_| HashMap::new()),
            overlay_clusters: HashMap::new(),
            cluster_fits: HashMap::new(),
            pending: Vec::new(),
            primed: HashSet::default(),
            atlas_reset_during_walk: false,
            atlas_resets: 0,
            walked: None,
        }
    }

    /// Scrollback streams unbounded unique row texts; at ~2.5 KiB per
    /// 80-column entry this bounds the memo near 10 MiB, the same order
    /// as `ShapeCache`'s 16 MiB budget.
    pub const SHAPED_RUN_CACHE_MAX: usize = 4096;

    /// A screenful rarely holds more than a few dozen distinct blank
    /// keys (the spaces in four styles and a handful of sizings).
    pub const BLANK_SLOT_MAX: usize = 4096;

    /// Twice the one-cell slots of an 8×16 cell a 2048² sheet holds, so
    /// text at a normal size fills the sheet first. The map's table then
    /// stays under 8 MiB, spare hash capacity included.
    pub const PLACED_SLOT_MAX: usize = 65_536;

    /// Returns `true` when this call shaped (a miss), `false` on a memo
    /// hit. `features` must be constant for the life of the index: the
    /// memo keys on text alone because the renderer routes every feature
    /// change through a reload that rebuilds the `GlyphIndex`
    /// (`reload_font_features`).
    pub fn shape_run_cached(
        &mut self,
        shaper: &mut Shaper,
        features: &[String],
        style: FontStyle,
        text: &str,
        out: &mut Vec<ShapedGlyph>,
    ) -> bool {
        out.clear();
        let map = &self.shaped_runs[style.index()];
        if let Some(cached) = map.get(text) {
            out.extend_from_slice(cached);
            return false;
        }
        let primary = self.stack.styled_primary(style).clone();
        let shaped = shaper.shape_run(&primary, self.font_size_physical_px, features, text);
        out.extend_from_slice(&shaped);
        // Insert first, then evict an arbitrary *other* entry while over
        // cap, so the entry just produced survives its own insertion.
        let map = &mut self.shaped_runs[style.index()];
        map.insert(text.to_owned(), shaped);
        while map.len() > Self::SHAPED_RUN_CACHE_MAX {
            let Some(evict_key) = map.keys().find(|k| k.as_str() != text).cloned() else {
                break;
            };
            map.remove(&evict_key);
        }
        true
    }

    #[must_use]
    pub fn shaped_run_cache_len(&self) -> usize {
        self.shaped_runs.iter().map(HashMap::len).sum()
    }

    #[must_use]
    pub const fn cell_metrics(&self) -> CellMetrics {
        self.metrics
    }

    #[must_use]
    pub const fn font_size_physical_px(&self) -> f32 {
        self.font_size_physical_px
    }

    #[must_use]
    pub const fn stack(&self) -> &FontStack {
        &self.stack
    }

    #[must_use]
    pub const fn atlas_side(&self) -> u32 {
        self.atlas.width()
    }

    #[must_use]
    pub fn contains(&self, glyph: char, cell_height: u32, sizing: SizingKey) -> bool {
        self.contains_key(SlotKey::Char(glyph), cell_height, sizing)
    }

    fn contains_key(&self, key: SlotKey, cell_height: u32, sizing: SizingKey) -> bool {
        self.slots.contains_key(&(key, cell_height, sizing))
    }

    fn lookup(&self, key: SlotKey, cell_height: u32, sizing: SizingKey) -> Option<GlyphSlot> {
        self.slots
            .get(&(key, cell_height, sizing))
            .copied()
            .flatten()
    }

    /// A second call with the same `(glyph, cell_height, sizing)` is a
    /// no-op. The rasterizer receives `font_size_physical_px × scale`,
    /// so an `s=N` cell produces an N×-bigger bitmap under its own key.
    /// Returns `true` when this call did work, `false` on a hit.
    pub fn ensure(&mut self, glyph: char, cell_height: u32, sizing: SizingKey) -> bool {
        let key = (SlotKey::Char(glyph), cell_height, sizing);
        if self.slots.contains_key(&key) {
            return false;
        }
        let scale = sizing.effective_scale();
        let scaled = CellMetrics {
            width: ((self.metrics.width as f32) * scale).round().max(1.0) as u32,
            height: ((cell_height as f32) * scale).round().max(1.0) as u32,
            ascent: ((self.metrics.ascent as f32) * scale).round() as u32,
        };
        if let Some(bitmap) = box_drawing::rasterize(glyph, scaled) {
            let slot = self.allocate(bitmap);
            self.insert_slot(key, slot);
            return true;
        }
        let font = self.stack.resolve(glyph, sizing.style()).clone();
        let bitmap = if let Some(bitmap) = self.fitted_bitmap(&font, glyph, cell_height, sizing) {
            bitmap
        } else {
            let px = (self.font_size_physical_px * scale).round() as u32;
            self.shape
                .get_or_insert_sized(&font, glyph, px, sizing)
                .clone()
        };
        let slot = self.allocate(bitmap);
        self.insert_slot(key, slot);
        true
    }

    /// `None` keeps the glyph's own size and bearings. The block matches
    /// `push_char_cell`'s: `w` (or the char's own width) cells at the
    /// integer scale `s`, whatever the fractional glyph scale.
    fn fitted_bitmap(
        &mut self,
        font: &Font,
        glyph: char,
        cell_height: u32,
        sizing: SizingKey,
    ) -> Option<GlyphBitmap> {
        let glyph_scale = sizing.effective_scale();
        let base_px = self.font_size_physical_px * glyph_scale;
        let cells = match sizing.cell_width() {
            0 => felis_grid::char_cell_width(glyph).max(1),
            w => w,
        };
        let block = self.fit_block(cells.into(), cell_height, sizing);
        if block.holds(font.advance_px(glyph, base_px)?) {
            return None;
        }
        let ink = self
            .shape
            .get_or_insert_sized(font, glyph, base_px.round() as u32, sizing);
        if ink.is_blank() {
            return None;
        }
        let ratio = block.ratio(ink.width() as f32, ink.height() as f32);
        let px = (base_px * ratio).floor().max(1.0) as u32;
        let mut bitmap = self
            .shape
            .get_or_insert_sized(font, glyph, px, sizing)
            .clone();
        bitmap.left = ((block.w - bitmap.width() as f32) / 2.0).round() as i32;
        let ascent = self.metrics.ascent as f32 * glyph_scale;
        bitmap.top = (ascent - (block.h - bitmap.height() as f32) / 2.0).round() as i32;
        Some(bitmap)
    }

    /// `cells` cells at the integer scale `s`, whatever the fractional
    /// glyph scale.
    fn fit_block(&self, cells: u16, cell_height: u32, sizing: SizingKey) -> FitBlock {
        let layout_scale = f32::from(sizing.scale().max(1));
        FitBlock {
            w: (self.metrics.width * u32::from(cells)) as f32 * layout_scale,
            h: cell_height as f32 * layout_scale,
            slack: layout_scale,
        }
    }

    /// Glyph ids are face-relative: [`SizingKey::font_id`] names the
    /// face (`0` = the styled primary, non-zero a fallback face) and
    /// rides the key, so the same raw id from two faces lands two slots.
    /// Returns `true` on first insertion, `false` on a hit.
    pub fn ensure_glyph_id(
        &mut self,
        glyph_id: GlyphId,
        cell_height: u32,
        sizing: SizingKey,
    ) -> bool {
        let px = self.font_size_physical_px * sizing.effective_scale();
        self.ensure_glyph_slot(SlotKey::Glyph(glyph_id), glyph_id, px, cell_height, sizing)
    }

    /// [`Self::ensure_glyph_id`] at `px`, the size [`Self::cluster_fit_px`]
    /// chose, instead of the sizing's own.
    pub fn ensure_fitted_glyph_id(
        &mut self,
        glyph_id: GlyphId,
        px: u16,
        cell_height: u32,
        sizing: SizingKey,
    ) -> bool {
        let key = SlotKey::FittedGlyph(glyph_id, px);
        self.ensure_glyph_slot(key, glyph_id, f32::from(px), cell_height, sizing)
    }

    fn ensure_glyph_slot(
        &mut self,
        slot_key: SlotKey,
        glyph_id: GlyphId,
        px: f32,
        cell_height: u32,
        sizing: SizingKey,
    ) -> bool {
        let key = (slot_key, cell_height, sizing);
        if self.slots.contains_key(&key) {
            return false;
        }
        let font = self.stack.font_at(sizing.font_id(), sizing.style()).clone();
        let bitmap = self.shape.rasterize_glyph_id(&font, glyph_id, px);
        let slot = self.allocate(bitmap);
        self.insert_slot(key, slot);
        true
    }

    /// PTY output picks the keys. Dropping the memo costs only a
    /// re-measure: the shape frame keeps each cell's fitted size.
    const CLUSTER_FIT_MAX: usize = 1024;

    /// The pixel size at which the cluster's ink fits a block of `cols`
    /// cells, or `None` when its advance stays inside the block. The
    /// block is `fitted_bitmap`'s, so a one-glyph cluster fits exactly
    /// as its bare char does.
    fn cluster_fit_px(
        &mut self,
        text: &str,
        glyphs: &[ShapedGlyph],
        cols: u16,
        sizing: SizingKey,
    ) -> Option<u16> {
        let glyph_scale = sizing.effective_scale();
        let block = self.fit_block(cols, self.metrics.height, sizing);
        let advance: f32 = glyphs.iter().map(|g| g.advance_px).sum::<f32>() * glyph_scale;
        if block.holds(advance) {
            return None;
        }
        let key = (text.to_owned(), cols, sizing);
        if let Some(&px) = self.cluster_fits.get(&key) {
            return (px > 0).then_some(px);
        }
        let font = self.stack.font_at(sizing.font_id(), sizing.style()).clone();
        let effective_px = self.font_size_physical_px * glyph_scale;
        let mut pen = 0.0f32;
        let mut ink: Option<[f32; 4]> = None;
        for g in glyphs {
            let bitmap = self
                .shape
                .rasterize_glyph_id(&font, g.glyph_id, effective_px);
            if !bitmap.is_blank() {
                let x0 = g.x_offset_px.mul_add(glyph_scale, pen) + bitmap.left as f32;
                let y0 = g.y_offset_px.mul_add(-glyph_scale, -bitmap.top as f32);
                let [x1, y1] = [x0 + bitmap.width() as f32, y0 + bitmap.height() as f32];
                ink = Some(ink.map_or([x0, y0, x1, y1], |[a, b, c, d]| {
                    [a.min(x0), b.min(y0), c.max(x1), d.max(y1)]
                }));
            }
            pen = g.advance_px.mul_add(glyph_scale, pen);
        }
        // A blank cluster has nothing to fit; 0 records that.
        let px = ink.map_or(0, |[x0, y0, x1, y1]| {
            (effective_px * block.ratio(x1 - x0, y1 - y0))
                .floor()
                .clamp(1.0, f32::from(u16::MAX)) as u16
        });
        if self.cluster_fits.len() >= Self::CLUSTER_FIT_MAX {
            self.cluster_fits.clear();
        }
        self.cluster_fits.insert(key, px);
        (px > 0).then_some(px)
    }

    /// Overlay text is typed by the user, not streamed by a producer, so
    /// a frame holds a handful of clusters at most.
    const OVERLAY_CLUSTER_MAX: usize = 256;

    /// Bounds the overlay shapes between frames, never during one: the
    /// frame paints every cluster it primed.
    pub(crate) fn trim_overlay_clusters(&mut self) {
        if self.overlay_clusters.len() > Self::OVERLAY_CLUSTER_MAX {
            self.overlay_clusters.clear();
        }
    }

    /// Shapes `text`, one multi-scalar cluster of overlay text, as
    /// `composite_cluster` shapes a cluster cell, and primes its glyphs.
    pub(crate) fn ensure_overlay_cluster(&mut self, shaper: &mut Shaper, text: &str) {
        if !self.overlay_clusters.contains_key(text) {
            let shaping = shaper.shape_cluster(
                &self.stack,
                self.font_size_physical_px,
                FontStyle::REGULAR,
                text,
            );
            let font_id = u8::try_from(shaping.font_id.min(0x3F)).unwrap_or(0x3F);
            let glyphs = shaping
                .glyphs
                .iter()
                .map(|g| ClusterGlyph {
                    glyph_id: g.glyph_id,
                    font_id,
                    advance_px: g.advance_px,
                    x_offset_px: g.x_offset_px,
                    y_offset_px: g.y_offset_px,
                })
                .collect();
            self.overlay_clusters.insert(text.to_owned(), glyphs);
        }
        let cell_height = self.metrics.height;
        let primes: Vec<(GlyphId, u8)> = self.overlay_clusters[text]
            .iter()
            .map(|g| (g.glyph_id, g.font_id))
            .collect();
        for (glyph_id, font_id) in primes {
            let key = SizingKey::default().with_font_id(usize::from(font_id));
            self.ensure_glyph_id(glyph_id, cell_height, key);
        }
    }

    /// Dropping a `None` entry cannot change a frame: [`AtlasView`]
    /// lookups read a missing key and a blank one alike as no slot.
    fn insert_slot(&mut self, key: (SlotKey, u32, SizingKey), slot: Option<GlyphSlot>) {
        if slot.is_none() {
            if self.blank_slots >= Self::BLANK_SLOT_MAX {
                self.slots.retain(|_, s| s.is_some());
                self.blank_slots = 0;
            }
            self.blank_slots += 1;
        } else {
            self.placed_slots += 1;
        }
        self.slots.insert(key, slot);
    }

    #[must_use]
    pub fn contains_glyph_id(
        &self,
        glyph_id: GlyphId,
        cell_height: u32,
        sizing: SizingKey,
    ) -> bool {
        self.contains_key(SlotKey::Glyph(glyph_id), cell_height, sizing)
    }

    pub fn drain_pending(&mut self) -> std::vec::Drain<'_, PendingUpload> {
        self.pending.drain(..)
    }

    fn begin_populate(&mut self) {
        self.primed.clear();
        self.atlas_reset_during_walk = false;
    }

    /// True means the slots primed before the reset are gone; the
    /// renderer walks once more (`Renderer::render`).
    #[must_use]
    pub const fn atlas_was_reset(&self) -> bool {
        self.atlas_reset_during_walk
    }

    /// Counts every recycle, across walks; a new index restarts at zero.
    #[must_use]
    pub const fn atlas_resets(&self) -> u64 {
        self.atlas_resets
    }

    /// Deduplicated by the cache key, not the char: sized variants
    /// share a base glyph but land distinct entries.
    fn prime_char(&mut self, glyph: char, cell_height: u32, sizing: SizingKey) {
        if self.primed.insert((glyph, sizing)) {
            self.ensure(glyph, cell_height, sizing);
        }
    }

    pub fn populate(&mut self, screen: &ScreenBuffer) {
        self.begin_populate();
        for r in 0..screen.rows() {
            self.populate_row(screen, r);
        }
    }

    fn populate_row(&mut self, screen: &ScreenBuffer, r: u16) {
        let cell_height = self.metrics.height;
        for c in 0..screen.cols() {
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            // Clusters are skipped: the shaping walk's cluster arm primes them,
            // so a per-char slot primed here would never be read
            // (docs/explanation/data-model/grid-and-cells.md "Cluster interning").
            let glyph = match &cell.grapheme {
                Grapheme::Empty
                | Grapheme::Spacer
                | Grapheme::SizedSpacer
                | Grapheme::Cluster(_) => continue,
                Grapheme::Ascii(b) => *b as char,
                Grapheme::Char(c) => *c,
            };
            let sizing = screen
                .cell_sizing(r, c)
                .map(|s| crate::instances::sizing_key_from(*s))
                .unwrap_or_default()
                .with_style(font_style_of(screen.style(cell.style).flags));
            self.prime_char(glyph, cell_height, sizing);
        }
    }

    /// Resets the atlas and retries once when the shelf packer is full.
    /// Returns `None` for blank glyphs or glyphs that cannot fit even an empty atlas,
    /// degrading to a negative slot instead of looping infinitely.
    fn allocate(&mut self, bitmap: GlyphBitmap) -> Option<GlyphSlot> {
        match self.try_allocate(bitmap) {
            AllocOutcome::Placed(slot) => Some(slot),
            AllocOutcome::Blank => None,
            AllocOutcome::Full(bitmap) => {
                self.reset_atlas_full();
                match self.try_allocate(bitmap) {
                    AllocOutcome::Placed(slot) => Some(slot),
                    AllocOutcome::Blank | AllocOutcome::Full(_) => None,
                }
            }
        }
    }

    /// Both sheets reset together even when only one overflowed, so no
    /// orphaned allocation leaks space across repeated resets. `shape`
    /// and `shaped_runs` are position-independent and stay warm. The
    /// walk's dedup set goes too: it records "already primed this walk",
    /// which the reset just made false for every entry.
    fn reset_atlas_full(&mut self) {
        self.atlas.reset();
        self.color_atlas.reset();
        self.slots.clear();
        self.blank_slots = 0;
        self.placed_slots = 0;
        self.pending.clear();
        self.primed.clear();
        self.atlas_reset_during_walk = true;
        self.atlas_resets += 1;
        self.walked = None;
    }

    fn try_allocate(&mut self, bitmap: GlyphBitmap) -> AllocOutcome {
        if bitmap.is_blank() || bitmap.width() == 0 || bitmap.height() == 0 {
            return AllocOutcome::Blank;
        }
        if self.placed_slots >= Self::PLACED_SLOT_MAX {
            return AllocOutcome::Full(bitmap);
        }
        let (width, height) = (bitmap.width(), bitmap.height());
        let offset_px = [bitmap.left, bitmap.top];
        let is_color = bitmap.is_color();
        let (alloc, side) = if is_color {
            match self.color_atlas.alloc(width, height) {
                Some(alloc) => (alloc, self.color_atlas.width()),
                None => return AllocOutcome::Full(bitmap),
            }
        } else {
            match self.atlas.alloc(width, height) {
                Some(alloc) => (alloc, self.atlas.width()),
                None => return AllocOutcome::Full(bitmap),
            }
        };
        let side = side as f32;
        let uv_min = [alloc.x as f32 / side, alloc.y as f32 / side];
        let uv_max = [
            (alloc.x + alloc.width) as f32 / side,
            (alloc.y + alloc.height) as f32 / side,
        ];
        self.pending.push(PendingUpload {
            alloc,
            pixels: bitmap.into_pixels(),
        });
        AllocOutcome::Placed(GlyphSlot {
            uv_min,
            uv_max,
            size_px: [width, height],
            offset_px,
            is_color,
        })
    }
}

enum AllocOutcome {
    Placed(GlyphSlot),
    Blank,
    /// Handed back so the caller can reset the atlas and retry.
    Full(GlyphBitmap),
}

/// ASCII bytes are 1:1 with cells, so `cell_count` equals the length
/// of the text written into the caller's buffer.
struct ShapeRun {
    start_col: u16,
    cell_count: u16,
    style: FontStyle,
}

/// Identifies eligible [`Grapheme::Ascii`] runs with default sizing and primary-face coverage.
///
/// Color and link boundaries do not end the run, preserving ligatures across syntax highlighting;
/// `extend_instances` slices quads per cell instead. Bold and italic changes do end the run
/// because they select a different face.
fn next_shape_run<F>(
    screen: &ScreenBuffer,
    row: u16,
    start_col: u16,
    text: &mut String,
    covers: F,
) -> Option<ShapeRun>
where
    F: Fn(char, FontStyle) -> bool,
{
    text.clear();
    let cols = screen.cols();
    if start_col >= cols {
        return None;
    }
    let first = screen.cell(row, start_col)?;
    let (Grapheme::Ascii(b0), None) = (&first.grapheme, screen.cell_sizing(row, start_col)) else {
        return None;
    };
    let style = font_style_of(screen.style(first.style).flags);
    let c0 = *b0 as char;
    if !covers(c0, style) {
        return None;
    }
    text.push(c0);
    let mut last_col = start_col;
    for c in (start_col + 1)..cols {
        let Some(cell) = screen.cell(row, c) else {
            break;
        };
        let Grapheme::Ascii(b) = &cell.grapheme else {
            break;
        };
        if screen.cell_sizing(row, c).is_some() {
            break;
        }
        if font_style_of(screen.style(cell.style).flags) != style {
            break;
        }
        let ch = *b as char;
        if !covers(ch, style) {
            break;
        }
        text.push(ch);
        last_col = c;
    }
    let cell_count = last_col - start_col + 1;
    Some(ShapeRun {
        start_col,
        cell_count,
        style,
    })
}

struct RunPlacement {
    row: u16,
    start_col: u16,
    cell_count: u16,
    style: FontStyle,
}

struct ClusterSite {
    row: u16,
    col: u16,
    id: NonZeroU32,
    style: FontStyle,
    sizing: SizingKey,
}

fn apply_shaped_run(
    frame_cells: &mut [ShapedCell],
    index: &mut GlyphIndex,
    cols: u16,
    run: &RunPlacement,
    shaped: &[ShapedGlyph],
) {
    let cell_height = index.cell_metrics().height;
    let cell_width = index.cell_metrics().width as f32;
    // `CellPainter` rebuilds this same key from the cell, so the style
    // must ride it.
    let sizing = SizingKey::default().with_style(run.style);
    let row_base = usize::from(run.row) * usize::from(cols);
    // Guard so a malformed shape result cannot land a primary past the
    // run's right edge and corrupt neighbors.
    let run_start_col = run.start_col;
    let run_end_col_exclusive = run_start_col + run.cell_count;
    // Processes clusters as units rather than individual glyphs. All pieces share
    // `source_byte_start`, so mapping naively by source start would stack them on the
    // leftmost cell instead of using intra-cluster pen positions.
    let mut i = 0;
    while i < shaped.len() {
        let src_start = shaped[i].source_byte_start;
        let src_len = shaped[i].source_byte_len;
        let mut j = i + 1;
        while j < shaped.len()
            && shaped[j].source_byte_start == src_start
            && shaped[j].source_byte_len == src_len
        {
            j += 1;
        }
        let cluster = &shaped[i..j];
        i = j;
        let cluster_col = run_start_col + (src_start as u16);
        if cluster_col >= run_end_col_exclusive {
            continue;
        }
        let span_cols = u16::try_from(src_len).unwrap_or(1).max(1);
        // Clamp so a degenerate `source_byte_len = 0` cluster still
        // terminates.
        let last_col = (cluster_col + span_cols).min(run_end_col_exclusive);
        let total_advance: f32 = cluster.iter().map(|g| g.advance_px).sum();
        // A ligature wider than its source cells stays unshaped: its cells
        // keep `None`, and the per-char path draws each char in its own
        // cell. The slack of a pixel per cell absorbs the rounding of the
        // primary advance into the cell width.
        if total_advance > (cell_width + 1.0) * f32::from(span_cols) {
            continue;
        }
        if let [g] = cluster {
            index.ensure_glyph_id(g.glyph_id, cell_height, sizing);
            if let Some(slot) = frame_cells.get_mut(row_base + usize::from(cluster_col)) {
                *slot = ShapedCell::Primary {
                    glyph_id: g.glyph_id,
                    span_cols,
                };
            }
            for col in (cluster_col + 1)..last_col {
                if let Some(slot) = frame_cells.get_mut(row_base + usize::from(col)) {
                    *slot = ShapedCell::Trailing;
                }
            }
            continue;
        }
        // Divide the pen by the cluster's own per-cell advance, not the
        // screen cell width: the two disagree by fractions of a pixel.
        let per_cell_advance = total_advance / f32::from(span_cols);
        // Trailing suppresses the per-char fallback, which would repaint
        // the raw source chars under the substituted pieces.
        for col in cluster_col..last_col {
            if let Some(slot) = frame_cells.get_mut(row_base + usize::from(col)) {
                *slot = ShapedCell::Trailing;
            }
        }
        let mut pen = 0.0f32;
        for g in cluster {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let cell_offset = if per_cell_advance > 0.0 {
                ((pen / per_cell_advance).round().max(0.0) as u16).min(span_cols - 1)
            } else {
                0
            };
            pen += g.advance_px;
            let col = cluster_col + cell_offset;
            if col >= last_col {
                continue;
            }
            index.ensure_glyph_id(g.glyph_id, cell_height, sizing);
            if let Some(slot) = frame_cells.get_mut(row_base + usize::from(col)) {
                *slot = ShapedCell::Primary {
                    glyph_id: g.glyph_id,
                    span_cols: 1,
                };
            }
        }
    }
}

impl AtlasView for GlyphIndex {
    fn slot(&self, glyph: char, cell_height_px: u32, sizing: SizingKey) -> Option<GlyphSlot> {
        self.lookup(SlotKey::Char(glyph), cell_height_px, sizing)
    }

    fn glyph_id_slot(
        &self,
        glyph_id: GlyphId,
        cell_height_px: u32,
        sizing: SizingKey,
    ) -> Option<GlyphSlot> {
        self.lookup(SlotKey::Glyph(glyph_id), cell_height_px, sizing)
    }

    fn fitted_glyph_id_slot(
        &self,
        glyph_id: GlyphId,
        px: u16,
        cell_height_px: u32,
        sizing: SizingKey,
    ) -> Option<GlyphSlot> {
        self.lookup(SlotKey::FittedGlyph(glyph_id, px), cell_height_px, sizing)
    }

    fn overlay_cluster(&self, text: &str) -> Option<&[ClusterGlyph]> {
        self.overlay_clusters.get(text).map(Vec::as_slice)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShapedCell {
    /// No shape data: the renderer falls back to the per-char path.
    #[default]
    None,
    /// Leftmost cell of a shaped cluster; trailing cells carry
    /// [`ShapedCell::Trailing`].
    Primary { glyph_id: GlyphId, span_cols: u16 },
    /// The fg pass skips this cell (the primary's quad covers it); the
    /// bg quad still draws.
    Trailing,
    /// Leftmost cell of a composited cluster; its glyphs live in the
    /// frame's pool at `[start .. start + len]`. A wide base's second
    /// cell is a [`Grapheme::Spacer`] the emission loop already skips,
    /// so it needs no [`Self::Trailing`]. A non-zero `fit_px` is the
    /// size its glyphs were shrunk to so the cluster fits its cells.
    Cluster { start: u32, len: u16, fit_px: u16 },
}

/// Produced while the [`ShapeFrame`] is built and read back in
/// `extend_instances`, so priming and emission cannot drift.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterGlyph {
    pub glyph_id: GlyphId,
    /// [`SizingKey::with_font_id`] handle, already saturated to the
    /// 6-bit field; priming and emission both rebuild the key with it.
    pub font_id: u8,
    /// Horizontal pen advance in pixels (≈0 for a combining mark).
    pub advance_px: f32,
    pub x_offset_px: f32,
    /// GPOS y offset in pixels (font y-up: positive raises the glyph).
    pub y_offset_px: f32,
}

/// Shape result indexed by cell, kept across frames: a walk rewrites
/// only the rows it visits. Empty means the shaper never touched the
/// screen: every lookup yields `ShapedCell::None`.
#[derive(Debug, Clone, Default)]
pub struct ShapeFrame {
    cells: Vec<ShapedCell>,
    cols: u16,
    cluster_glyphs: Vec<ClusterGlyph>,
    /// Pool entries some cell still points at; the rest belong to rows
    /// walked again since, and [`Self::compact`] drops them.
    live_cluster_glyphs: usize,
}

impl ShapeFrame {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            cells: Vec::new(),
            cols: 0,
            cluster_glyphs: Vec::new(),
            live_cluster_glyphs: 0,
        }
    }

    fn begin(&mut self, rows: u16, cols: u16) {
        let cell_count = usize::from(rows) * usize::from(cols);
        self.cells.clear();
        self.cells.resize(cell_count, ShapedCell::None);
        self.cols = cols;
        self.cluster_glyphs.clear();
        self.live_cluster_glyphs = 0;
    }

    fn reset_empty(&mut self) {
        self.cells.clear();
        self.cols = 0;
        self.cluster_glyphs.clear();
        self.live_cluster_glyphs = 0;
    }

    fn clear_row(&mut self, row: u16) {
        let cols = usize::from(self.cols);
        let start = usize::from(row) * cols;
        let Some(cells) = self.cells.get_mut(start..start + cols) else {
            return;
        };
        for cell in cells {
            if let ShapedCell::Cluster { len, .. } = *cell {
                self.live_cluster_glyphs -= usize::from(len);
            }
            *cell = ShapedCell::None;
        }
    }

    /// Rewrites the pool once the rows walked again left it mostly
    /// unreferenced, so it stays within a small multiple of what the
    /// screen shows.
    fn compact(&mut self) {
        if self.cluster_glyphs.len() <= (self.live_cluster_glyphs * 2).max(4096) {
            return;
        }
        let mut pool = Vec::with_capacity(self.live_cluster_glyphs);
        for cell in &mut self.cells {
            if let ShapedCell::Cluster { start, len, .. } = cell {
                let from = *start as usize;
                *start = u32::try_from(pool.len()).unwrap_or(u32::MAX);
                pool.extend_from_slice(&self.cluster_glyphs[from..from + usize::from(*len)]);
            }
        }
        self.cluster_glyphs = pool;
    }

    /// The quad-slicing tests in `instances.rs` build frames directly
    /// because the full pipeline needs a real ligature font.
    #[cfg(test)]
    pub(crate) const fn from_cells(cells: Vec<ShapedCell>, cols: u16) -> Self {
        Self {
            cells,
            cols,
            cluster_glyphs: Vec::new(),
            live_cluster_glyphs: 0,
        }
    }

    #[cfg(test)]
    pub(crate) const fn from_cluster_cells(
        cells: Vec<ShapedCell>,
        cols: u16,
        cluster_glyphs: Vec<ClusterGlyph>,
    ) -> Self {
        let live_cluster_glyphs = cluster_glyphs.len();
        Self {
            cells,
            cols,
            cluster_glyphs,
            live_cluster_glyphs,
        }
    }

    #[must_use]
    pub fn cluster_slice(&self, start: u32, len: u16) -> &[ClusterGlyph] {
        let s = start as usize;
        let e = s + len as usize;
        self.cluster_glyphs.get(s..e).unwrap_or(&[])
    }

    #[must_use]
    pub fn cell_at(&self, row: u16, col: u16) -> ShapedCell {
        if self.cells.is_empty() || self.cols == 0 || col >= self.cols {
            return ShapedCell::None;
        }
        let idx = usize::from(row) * usize::from(self.cols) + usize::from(col);
        self.cells.get(idx).copied().unwrap_or(ShapedCell::None)
    }

    #[must_use]
    pub fn cells(&self) -> &[ShapedCell] {
        &self.cells
    }

    #[must_use]
    pub const fn cols(&self) -> u16 {
        self.cols
    }
}

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
                upload.pixels.as_bytes(),
                bytes_per_row,
            );
        }
    }
}

/// Scratch and bookkeeping for the grid walk, kept apart from the GPU
/// resources so tests drive it against a bare [`GlyphIndex`].
#[derive(Default)]
pub(crate) struct GridWalker {
    run_text: String,
    run_glyphs: Vec<ShapedGlyph>,
    rows: Vec<u16>,
}

impl GridWalker {
    /// Walks only the rows `screen` marked dirty while `frame` and the
    /// index's slots still hold everything the last walk put there, and
    /// every row otherwise. With empty `features` and no interned
    /// clusters this avoids shaping overhead. `shaper` is borrowed
    /// mutably so Swash's feature cache on `ShapeContext` stays resident.
    pub(crate) fn walk(
        &mut self,
        index: &mut GlyphIndex,
        screen: &ScreenBuffer,
        shaper: &mut Shaper,
        features: &[String],
        frame: &mut ShapeFrame,
    ) {
        let (rows, cols) = (screen.rows(), screen.cols());
        // `cluster_count` is an O(1) table-size probe; it can be non-zero
        // for a cluster that has scrolled off, which is harmless.
        let shaped = !features.is_empty() || screen.cluster_count() > 0;
        let key = WalkKey { rows, cols, shaped };
        let full = index.walked != Some(key);
        self.rows.clear();
        if full {
            self.rows.extend(0..rows);
        } else {
            let dirty = screen.damage().dirty_rows();
            self.rows
                .extend(dirty.filter_map(|r| u16::try_from(r).ok()));
        }
        index.walked = None;
        index.begin_populate();
        let walk = std::mem::take(&mut self.rows);
        if shaped {
            if full {
                frame.begin(rows, cols);
            }
            let stack = index.stack().clone();
            for &r in &walk {
                frame.clear_row(r);
                self.walk_shaped_row(index, screen, shaper, features, &stack, r, frame);
            }
            frame.compact();
        } else {
            frame.reset_empty();
            for &r in &walk {
                index.populate_row(screen, r);
            }
        }
        self.rows = walk;
        if !index.atlas_was_reset() {
            index.walked = Some(key);
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the walk's borrowed inputs, split so the index and frame stay separately mutable"
    )]
    fn walk_shaped_row(
        &mut self,
        index: &mut GlyphIndex,
        screen: &ScreenBuffer,
        shaper: &mut Shaper,
        features: &[String],
        stack: &FontStack,
        r: u16,
        frame: &mut ShapeFrame,
    ) {
        let cols = screen.cols();
        let mut col = 0u16;
        while col < cols {
            if !features.is_empty()
                && let Some(run) = next_shape_run(screen, r, col, &mut self.run_text, |c, style| {
                    stack.primary_covers(c, style)
                })
            {
                col = run.start_col + run.cell_count;
                self.shape_and_apply_run(
                    index,
                    shaper,
                    features,
                    cols,
                    &RunPlacement {
                        row: r,
                        start_col: run.start_col,
                        cell_count: run.cell_count,
                        style: run.style,
                    },
                    frame,
                );
                continue;
            }
            shape_cell(index, screen, shaper, stack, r, col, frame);
            col += 1;
        }
    }

    /// The run's source chars are primed through the per-char path too:
    /// shaping can degenerately yield nothing, leaving the cells
    /// [`ShapedCell::None`] for the emission fallback.
    fn shape_and_apply_run(
        &mut self,
        index: &mut GlyphIndex,
        shaper: &mut Shaper,
        features: &[String],
        cols: u16,
        run: &RunPlacement,
        frame: &mut ShapeFrame,
    ) {
        if self.run_text.is_empty() {
            return;
        }
        let cell_height = index.cell_metrics().height;
        let sizing = SizingKey::default().with_style(run.style);
        for ch in self.run_text.chars() {
            index.prime_char(ch, cell_height, sizing);
        }
        index.shape_run_cached(
            shaper,
            features,
            run.style,
            &self.run_text,
            &mut self.run_glyphs,
        );
        apply_shaped_run(&mut frame.cells, index, cols, run, &self.run_glyphs);
    }
}

fn shape_cell(
    index: &mut GlyphIndex,
    screen: &ScreenBuffer,
    shaper: &mut Shaper,
    stack: &FontStack,
    row: u16,
    col: u16,
    frame: &mut ShapeFrame,
) {
    let Some(cell) = screen.cell(row, col) else {
        return;
    };
    let glyph = match cell.grapheme {
        Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => return,
        Grapheme::Cluster(_) => None,
        Grapheme::Ascii(b) => Some(b as char),
        Grapheme::Char(c) => Some(c),
    };
    let style = font_style_of(screen.style(cell.style).flags);
    let sizing = screen
        .cell_sizing(row, col)
        .map(|s| crate::instances::sizing_key_from(*s))
        .unwrap_or_default()
        .with_style(style);
    let cell_height = index.cell_metrics().height;
    if let Some(glyph) = glyph {
        index.prime_char(glyph, cell_height, sizing);
        return;
    }
    let Grapheme::Cluster(id) = cell.grapheme else {
        return;
    };
    composite_cluster(
        index,
        screen,
        shaper,
        stack,
        &ClusterSite {
            row,
            col,
            id,
            style,
            sizing,
        },
        frame,
    );
}

/// Primes a cluster's glyphs with a sized key for scaled rasterization.
///
/// If shaping yields nothing, primes the base char under the same key so the fallback
/// resolves a correctly scaled bitmap.
fn composite_cluster(
    index: &mut GlyphIndex,
    screen: &ScreenBuffer,
    shaper: &mut Shaper,
    stack: &FontStack,
    site: &ClusterSite,
    frame: &mut ShapeFrame,
) {
    let Some(text) = screen.cluster_str(site.id) else {
        return;
    };
    let base = text.chars().next();
    // The image pass paints the placeholder; compositing it would
    // draw tofu under the image.
    if base == Some(felis_protocol::kitty_graphics::placeholder::PLACEHOLDER) {
        return;
    }
    let cell_height = index.cell_metrics().height;
    let px = index.font_size_physical_px();
    let shaping = shaper.shape_cluster(stack, px, site.style, text);
    if shaping.glyphs.is_empty() {
        if let Some(bc) = base {
            index.ensure(bc, cell_height, site.sizing);
        }
        return;
    }
    let sized_key = site.sizing.with_font_id(shaping.font_id);
    let font_id = u8::try_from(shaping.font_id.min(0x3F)).unwrap_or(0x3F);
    let cols = crate::instances::cluster_block_cols(screen, site.row, site.col);
    let fit_px = index.cluster_fit_px(text, &shaping.glyphs, cols, sized_key);
    // Emission scales the pen by the sizing's glyph scale; pre-scaling
    // the pool by the fit keeps one formula for both.
    let pen_ratio = fit_px.map_or(1.0, |px| {
        f32::from(px) / (index.font_size_physical_px() * sized_key.effective_scale())
    });
    let start = u32::try_from(frame.cluster_glyphs.len()).unwrap_or(u32::MAX);
    let mut len = 0u16;
    for g in &shaping.glyphs {
        match fit_px {
            Some(px) => index.ensure_fitted_glyph_id(g.glyph_id, px, cell_height, sized_key),
            None => index.ensure_glyph_id(g.glyph_id, cell_height, sized_key),
        };
        frame.cluster_glyphs.push(ClusterGlyph {
            glyph_id: g.glyph_id,
            font_id,
            advance_px: g.advance_px * pen_ratio,
            x_offset_px: g.x_offset_px * pen_ratio,
            y_offset_px: g.y_offset_px * pen_ratio,
        });
        len = len.saturating_add(1);
    }
    let idx = usize::from(site.row) * usize::from(frame.cols) + usize::from(site.col);
    if let Some(slot) = frame.cells.get_mut(idx) {
        *slot = ShapedCell::Cluster {
            start,
            len,
            fit_px: fit_px.unwrap_or(0),
        };
        frame.live_cluster_glyphs += usize::from(len);
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use felis_grid::Grid;

    use std::sync::Arc;

    use felis_shaping::Font;
    use felis_vt::Parser;

    use super::*;

    fn nz(v: u32) -> NonZeroU32 {
        NonZeroU32::new(v).unwrap()
    }

    fn stack() -> FontStack {
        FontStack::new(Arc::new(
            Font::load_default().expect("system monospace font"),
        ))
    }

    #[test]
    fn an_overlay_cluster_is_shaped_and_its_glyphs_primed() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let mut shaper = Shaper::new();
        assert!(idx.overlay_cluster("e\u{301}").is_none());
        idx.ensure_overlay_cluster(&mut shaper, "e\u{301}");
        let glyphs = idx.overlay_cluster("e\u{301}").expect("shaped").to_vec();
        assert_ne!(glyphs.len(), 0);
        let h = idx.cell_metrics().height;
        let base = &glyphs[0];
        let key = SizingKey::default().with_font_id(usize::from(base.font_id));
        assert!(
            idx.glyph_id_slot(base.glyph_id, h, key).is_some(),
            "base primed"
        );
    }

    #[test]
    fn overlay_shapes_survive_the_frame_that_primed_them() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let mut shaper = Shaper::new();
        let clusters: Vec<String> = ('a'..='z')
            .flat_map(|base| ('\u{300}'..='\u{30F}').map(move |mark| format!("{base}{mark}")))
            .take(GlyphIndex::OVERLAY_CLUSTER_MAX + 50)
            .collect();
        idx.trim_overlay_clusters();
        for cluster in &clusters {
            idx.ensure_overlay_cluster(&mut shaper, cluster);
        }
        assert!(
            clusters.iter().all(|c| idx.overlay_cluster(c).is_some()),
            "a label past the bound keeps every cluster for the frame it paints"
        );
        idx.trim_overlay_clusters();
        assert!(
            idx.overlay_cluster(&clusters[0]).is_none(),
            "bounded between frames"
        );
    }

    #[test]
    fn ensure_records_a_slot_and_pending_upload_for_a_printable_glyph() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        let did_work = idx.ensure('A', h, sk);
        assert!(did_work, "first ensure must rasterize + allocate");
        assert!(idx.contains('A', h, sk));
        let slot = idx.slot('A', h, sk).expect("A produced a slot");
        assert!(slot.uv_min[0] >= 0.0 && slot.uv_max[0] <= 1.0);
        assert!(slot.uv_min[1] >= 0.0 && slot.uv_max[1] <= 1.0);
        assert!(slot.size_px[0] > 0 && slot.size_px[1] > 0);
        let mut iter = idx.drain_pending();
        let first = iter.next().expect("one upload queued");
        assert!(iter.next().is_none(), "exactly one upload queued");
        assert_eq!(
            first.pixels.as_bytes().len(),
            (first.alloc.width * first.alloc.height) as usize,
            "pixel buffer length matches the allocation"
        );
    }

    #[test]
    fn ensure_is_idempotent_for_the_same_key() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        assert!(idx.ensure('B', h, sk));
        assert!(!idx.ensure('B', h, sk), "second ensure is a hit");
        assert_eq!(
            idx.drain_pending().count(),
            1,
            "second ensure must not enqueue another upload"
        );
    }

    #[test]
    fn whitespace_caches_a_negative_entry_with_no_upload() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        idx.ensure(' ', h, sk);
        assert!(idx.contains(' ', h, sk));
        assert_eq!(idx.slot(' ', h, sk), None, "space resolves to no slot");
        assert!(
            idx.drain_pending().next().is_none(),
            "blank glyphs do not queue uploads"
        );
    }

    #[test]
    fn populate_walks_the_grid_and_caches_unique_glyphs_once() {
        let mut screen = Grid::new(2, 5);
        let mut parser = Parser::new();
        parser.advance(&mut screen, b"hello\r\nhello");
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        idx.populate(screen.screen());
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        for c in ['h', 'e', 'l', 'o'] {
            assert!(idx.contains(c, h, sk), "missing cache entry for {c:?}");
            assert!(idx.slot(c, h, sk).is_some(), "missing slot for {c:?}");
        }
        let pending_count = idx.drain_pending().count();
        assert_eq!(pending_count, 4);
    }

    #[test]
    fn sized_variant_takes_a_distinct_atlas_slot_from_default() {
        // An OSC 66 `s=2` 'A' must not share a slot with the default 'A'.
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(512));
        let h = idx.cell_metrics().height;
        let default_sk = SizingKey::default();
        let scale_2_sk = SizingKey::new(2, 0, 0, 0, 0, 0);
        assert!(idx.ensure('A', h, default_sk), "first ensure must work");
        assert!(
            idx.ensure('A', h, scale_2_sk),
            "sized variant must rasterize separately"
        );
        let small = idx.slot('A', h, default_sk).expect("default slot");
        let large = idx.slot('A', h, scale_2_sk).expect("scaled slot");
        assert_ne!(
            (small.uv_min, small.uv_max),
            (large.uv_min, large.uv_max),
            "sized variant must occupy a different atlas rect"
        );
        // Generous slack: swash's hinted metrics are not exactly 2×.
        assert!(
            large.size_px[1] > small.size_px[1],
            "scaled bitmap height ({}) must exceed default ({})",
            large.size_px[1],
            small.size_px[1]
        );
    }

    #[test]
    fn a_full_atlas_resets_and_keeps_placing_glyphs() {
        // A small atlas so one page of distinct glyphs overflows; the
        // index must reset and keep handing out real slots.
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(48));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        let mut reset_seen = false;
        let mut placed_after_reset = false;
        let mut prev_len = idx.slots.len();
        for c in ('!'..='~').chain('\u{a1}'..='\u{17f}') {
            idx.ensure(c, h, sk);
            let len = idx.slots.len();
            if len < prev_len {
                reset_seen = true;
                assert!(
                    idx.slot(c, h, sk).is_some(),
                    "post-reset glyph {c:?} must get a real slot",
                );
                placed_after_reset = true;
            }
            prev_len = len;
        }
        assert!(
            reset_seen,
            "a page of glyphs must overflow the tiny atlas and reset it",
        );
        assert!(
            placed_after_reset,
            "allocations must resume after a reset instead of turning into permanent misses",
        );
    }

    /// The OSC 66 sizings the VT parser admits, each a distinct key.
    fn osc66_sizings() -> impl Iterator<Item = SizingKey> {
        (1..=7u8).flat_map(|s| {
            (0..=7u8).flat_map(move |w| {
                (0..=15u8).flat_map(move |d| {
                    (0..d.max(1)).flat_map(move |n| {
                        (0..=2u8).flat_map(move |v| {
                            (0..=2u8).map(move |h| SizingKey::new(s, w, n, d, v, h))
                        })
                    })
                })
            })
        })
    }

    #[test]
    fn distinct_blank_keys_keep_the_slot_map_under_the_blank_cap() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let h = idx.cell_metrics().height;
        for sk in osc66_sizings().take(3 * GlyphIndex::BLANK_SLOT_MAX) {
            idx.ensure(' ', h, sk);
            assert_eq!(idx.slot(' ', h, sk), None, "a sized space stays blank");
            assert!(
                idx.slots.len() <= GlyphIndex::BLANK_SLOT_MAX,
                "{} slot entries after blank key {sk:?}",
                idx.slots.len(),
            );
        }
    }

    #[test]
    fn dropping_blank_entries_keeps_placed_slots_and_the_atlas() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        idx.ensure('A', h, sk);
        let placed = idx.slot('A', h, sk).expect("A is placed");
        let _ = idx.drain_pending().count();
        for blank in osc66_sizings().take(2 * GlyphIndex::BLANK_SLOT_MAX) {
            idx.ensure(' ', h, blank);
        }
        assert_eq!(
            idx.atlas_resets(),
            0,
            "blank entries never recycle the atlas"
        );
        assert!(!idx.ensure('A', h, sk), "A is still resident");
        assert_eq!(idx.slot('A', h, sk), Some(placed));
        assert!(idx.drain_pending().next().is_none(), "nothing re-uploads");
    }

    fn placed_slot_count(idx: &GlyphIndex) -> usize {
        idx.slots.values().filter(|s| s.is_some()).count()
    }

    #[test]
    fn distinct_tiny_placed_glyphs_recycle_the_atlas_at_the_placed_cap() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(crate::GLYPH_ATLAS_SIDE));
        let h = idx.cell_metrics().height;
        let fractional = osc66_sizings()
            .filter(|sk| sk.scale() == 1 && sk.frac_num() > 0 && sk.frac_num() < sk.frac_den());
        let keys = fractional.flat_map(|sk| ('A'..='Z').map(move |c| (c, sk)));
        let mut placed = 0;
        let mut last = None;
        for (c, sk) in keys {
            let before = idx.placed_slots;
            let resets = idx.atlas_resets();
            idx.ensure(c, h, sk);
            if idx.atlas_resets() != resets {
                assert_eq!(
                    before,
                    GlyphIndex::PLACED_SLOT_MAX,
                    "the cap, not the sheet, recycles tiny glyphs",
                );
            }
            assert!(idx.placed_slots <= GlyphIndex::PLACED_SLOT_MAX);
            if let Some(slot) = idx.slot(c, h, sk) {
                placed += 1;
                last = Some((c, sk, slot));
                if placed == GlyphIndex::PLACED_SLOT_MAX + 1 {
                    break;
                }
            }
        }
        assert_eq!(placed, GlyphIndex::PLACED_SLOT_MAX + 1);
        assert_eq!(idx.atlas_resets(), 1);
        assert_eq!(placed_slot_count(&idx), idx.placed_slots);
        let (c, sk, slot) = last.expect("a glyph was placed");
        assert!(
            !idx.ensure(c, h, sk),
            "the newest glyph survives the recycle"
        );
        assert_eq!(idx.slot(c, h, sk), Some(slot));
    }

    /// Overlay text (a chrome bar's label, the pre-edit) is primed
    /// after the grid walk, so its own overflow drops the grid's fresh
    /// slots; the renderer reads the same reported reset to walk again.
    #[test]
    fn an_overlay_char_that_overflows_after_the_walk_reports_the_reset() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(32));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();

        let text = "uvwxyz";
        let mut screen = Grid::new(1, 8);
        Parser::new().advance(&mut screen, text.as_bytes());
        idx.populate(screen.screen());
        assert!(
            !idx.atlas_was_reset(),
            "the walk of one short row must fit, isolating the overlay overflow",
        );

        // Overlay text never goes through `populate`, so it primes on
        // top of a finished walk exactly as `populate_chars` does.
        for c in '\u{a1}'..='\u{2ff}' {
            if idx.atlas_was_reset() {
                break;
            }
            idx.ensure(c, h, sk);
        }
        assert!(
            idx.atlas_was_reset(),
            "priming overlay text after the walk must report the reset",
        );
        assert!(
            text.chars().any(|c| idx.slot(c, h, sk).is_none()),
            "the grid slots handed out before the overlay's reset are gone",
        );
    }

    /// A reset partway through a walk drops slots already handed out,
    /// and rendering is damage-driven, so nothing would repaint those
    /// cells; the reported reset drives a second walk.
    #[test]
    fn a_mid_walk_reset_is_reported_and_a_second_walk_reslots_the_grid() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(48));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();

        // Measured, not assumed: it depends on the system face's bitmap
        // sizes.
        let capacity = {
            let mut probe = GlyphIndex::new(stack(), 14.0, nz(48));
            ('!'..='~')
                .take_while(|c| {
                    let before = probe.slots.len();
                    probe.ensure(*c, h, sk);
                    probe.slots.len() > before
                })
                .count()
        };
        let text = "uvwxyz";
        assert!(
            capacity > text.len() + 2,
            "the sheet must hold the screen's own set with room to pre-fill",
        );

        // Fill the sheet with glyphs the screen does not use, stopping
        // just short of a reset, so the screen's walk resets partway.
        for c in ('!'..='~').take(capacity - 2) {
            idx.ensure(c, h, sk);
        }

        let mut screen = Grid::new(1, 8);
        Parser::new().advance(&mut screen, text.as_bytes());

        idx.populate(screen.screen());
        assert!(
            idx.atlas_was_reset(),
            "the pre-filled sheet must overflow during the walk",
        );
        assert!(
            text.chars().any(|c| idx.slot(c, h, sk).is_none()),
            "the glyphs primed before the reset lost their slots",
        );

        idx.populate(screen.screen());
        assert!(
            !idx.atlas_was_reset(),
            "the reclaimed sheet holds the screen's own set in one walk",
        );
        for c in text.chars() {
            assert!(
                idx.slot(c, h, sk).is_some(),
                "second walk must re-slot {c:?} so its cell paints this frame",
            );
        }
    }

    #[test]
    fn a_glyph_too_large_for_the_atlas_degrades_without_looping() {
        // An atlas so small nothing fits even when empty: a reset frees
        // no usable space, so without the retry-once guard this hangs.
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(2));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        idx.ensure('W', h, sk);
        assert_eq!(
            idx.slot('W', h, sk),
            None,
            "a glyph larger than the whole atlas degrades to no slot",
        );
    }

    #[test]
    fn fractional_scale_rasterizes_below_default_for_sub_cell_shrink() {
        // `s=1, n=1, d=2` is effective scale 0.5, so the bitmap must be
        // smaller than the default.
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(512));
        let h = idx.cell_metrics().height;
        let default_sk = SizingKey::default();
        let frac_half_sk = SizingKey::new(1, 0, 1, 2, 0, 0);
        let scale_2_sk = SizingKey::new(2, 0, 0, 0, 0, 0);
        assert!(idx.ensure('A', h, default_sk));
        assert!(idx.ensure('A', h, frac_half_sk));
        assert!(idx.ensure('A', h, scale_2_sk));
        let small = idx.slot('A', h, frac_half_sk).expect("fractional slot");
        let mid = idx.slot('A', h, default_sk).expect("default slot");
        let large = idx.slot('A', h, scale_2_sk).expect("integer scale=2 slot");
        // swash's hinted metrics are not strictly proportional; compare
        // the ordering, not exact ratios.
        assert!(
            small.size_px[1] < mid.size_px[1],
            "0.5× bitmap height ({}) must be below default ({})",
            small.size_px[1],
            mid.size_px[1],
        );
        assert!(
            large.size_px[1] > mid.size_px[1],
            "2× bitmap height ({}) must exceed default ({})",
            large.size_px[1],
            mid.size_px[1],
        );
    }

    #[test]
    fn primary_face_ascii_is_never_fitted_at_any_osc66_scale() {
        let font = Arc::new(Font::load_test_font_or_default().expect("a monospace font"));
        for px in [13.0, 18.67, 24.0, 29.33] {
            let mut idx = GlyphIndex::new(FontStack::new(font.clone()), px, nz(512));
            let h = idx.cell_metrics().height;
            for s in 1..=7 {
                let sk = SizingKey::new(s, 0, 0, 0, 0, 0);
                for c in '!'..='~' {
                    assert!(
                        idx.fitted_bitmap(&font, c, h, sk).is_none(),
                        "{c:?} at {px} px, s={s}"
                    );
                }
            }
        }
    }

    /// Width-1 chars the pinned emoji and symbol faces draw wider than a
    /// Monaspace cell.
    const OVERWIDE: [char; 6] = [
        '\u{263A}',
        '\u{2764}',
        '\u{2654}',
        '\u{26F5}',
        '\u{2B50}',
        '\u{1F30D}',
    ];

    /// The ink fills the block along its binding axis and is centred on
    /// both, so a fitted glyph reads as large as the cell allows.
    #[test]
    fn a_fallback_glyph_wider_than_its_cell_fills_and_centres_in_it() {
        let Some(stack) = FontStack::try_pinned_test_stack() else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut idx = GlyphIndex::new(stack, 18.0, nz(1024));
        let m = idx.cell_metrics();
        let (cw, ch, ascent) = (i64::from(m.width), i64::from(m.height), i64::from(m.ascent));
        let mut fitted = 0;
        for c in OVERWIDE
            .into_iter()
            .filter(|&c| felis_grid::char_cell_width(c) == 1)
        {
            let font = idx.stack().resolve(c, FontStyle::REGULAR).clone();
            let Some(bitmap) = idx.fitted_bitmap(&font, c, m.height, SizingKey::default()) else {
                continue;
            };
            fitted += 1;
            let (w, h) = (i64::from(bitmap.width()), i64::from(bitmap.height()));
            let (left, top) = (i64::from(bitmap.left), i64::from(bitmap.top));
            let right = cw - left - w;
            let (above, below) = (ascent - top, ch - (ascent - top) - h);
            assert!(
                left >= 0 && right >= -1,
                "{c:?} inside the cell: left {left}, right {right}"
            );
            assert!(
                above >= -1 && below >= -1,
                "{c:?} inside the cell: above {above}, below {below}"
            );
            assert!(
                (left - right).abs() <= 1,
                "{c:?} centred: left {left}, right {right}"
            );
            assert!(
                (above - below).abs() <= 1,
                "{c:?} centred: above {above}, below {below}"
            );
            assert!(
                w >= cw - 2 || h >= ch - 2,
                "{c:?} fills an axis: {w}x{h} in {cw}x{ch}"
            );
        }
        assert!(
            fitted > 0,
            "no candidate overruns its cell in the pinned set"
        );
    }

    /// An OSC 66 block is `w` (or the char's width) cells at the integer
    /// `s`, so a glyph between one and two cells wide fits `w=2` and a
    /// halved glyph in an `s=2` block, and is fitted only for `w=1`.
    #[test]
    fn osc66_width_and_fractional_scale_set_the_fitted_block() {
        let Some(stack) = FontStack::try_pinned_test_stack() else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let px = 18.0;
        let mut idx = GlyphIndex::new(stack, px, nz(1024));
        let m = idx.cell_metrics();
        let cw = m.width as f32;
        let (c, font) = OVERWIDE
            .into_iter()
            .filter(|&c| felis_grid::char_cell_width(c) == 1)
            .map(|c| (c, idx.stack().resolve(c, FontStyle::REGULAR).clone()))
            .find(|(c, font)| {
                font.advance_px(*c, px)
                    .is_some_and(|a| a > cw + 1.0 && a < 2.0 * cw)
            })
            .expect("the pinned set has a width-1 glyph between one and two cells wide");
        let w1 = SizingKey::new(1, 1, 0, 0, 0, 0);
        let w2 = SizingKey::new(1, 2, 0, 0, 0, 0);
        let half_in_s2 = SizingKey::new(2, 0, 1, 2, 0, 0);
        assert!(
            idx.fitted_bitmap(&font, c, m.height, w1).is_some(),
            "w=1 fits {c:?}"
        );
        assert!(
            idx.fitted_bitmap(&font, c, m.height, w2).is_none(),
            "w=2 holds {c:?}"
        );
        assert!(
            idx.fitted_bitmap(&font, c, m.height, half_in_s2).is_none(),
            "s=2 n=1 d=2 holds {c:?}"
        );
    }

    /// A star the symbol faces cover is still drawn in color, as an
    /// emoji-presentation scalar.
    #[test]
    fn an_emoji_presentation_char_rasterizes_in_color() {
        let Some(stack) = FontStack::try_pinned_test_stack() else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut idx = GlyphIndex::new(stack, 18.0, nz(1024));
        let h = idx.cell_metrics().height;
        idx.ensure('\u{2B50}', h, SizingKey::default());
        let slot = idx
            .slot('\u{2B50}', h, SizingKey::default())
            .expect("a star slot");
        assert!(slot.is_color);
    }

    /// A one-row grid fed `bytes`, walked and painted against the pinned
    /// set; `None` when `FELIS_TEST_FONT_DIR` is unset.
    struct Painted {
        index: GlyphIndex,
        frame: ShapeFrame,
        grid: Grid,
        fg: Vec<crate::instances::FgInstance>,
    }

    fn paint_pinned(bytes: &[u8], cols: u16) -> Option<Painted> {
        let stack = FontStack::try_pinned_test_stack()?;
        let mut grid = Grid::new(1, cols);
        Parser::new().advance(&mut grid, bytes);
        let mut index = GlyphIndex::new(stack, 18.0, nz(1024));
        let mut frame = ShapeFrame::empty();
        GridWalker::default().walk(
            &mut index,
            grid.screen(),
            &mut Shaper::new(),
            &[],
            &mut frame,
        );
        let theme = crate::palette::Theme::default();
        let mut out = crate::instances::InstanceBuffers::default();
        crate::instances::CellPainter::new(
            grid.screen(),
            index.cell_metrics(),
            &crate::palette::ResolvedTheme::new(&theme),
            &index,
            &frame,
        )
        .with_viewport(0, 1)
        .extend_instances(&mut out);
        Some(Painted {
            index,
            frame,
            grid,
            fg: out.fg,
        })
    }

    fn cluster_fit_px_at(p: &Painted, col: u16) -> u16 {
        match p.frame.cell_at(0, col) {
            ShapedCell::Cluster { fit_px, .. } => fit_px,
            other => panic!("col {col} is not a cluster: {other:?}"),
        }
    }

    /// The fg quads' union as `[x0, y0, x1, y1]`.
    fn ink(fg: &[crate::instances::FgInstance]) -> [f32; 4] {
        fg.iter()
            .map(|q| {
                let [x, y] = q.origin_px;
                [x, y, x + q.size_px[0], y + q.size_px[1]]
            })
            .reduce(|[a, b, c, d], [x0, y0, x1, y1]| [a.min(x0), b.min(y0), c.max(x1), d.max(y1)])
            .expect("the cluster painted")
    }

    /// Each repro is the only ink on its row, its cluster at `col` with
    /// `cells` cells: a ZWJ pair the face has no glyph for, a modifier on a
    /// base that takes none, and a VS16 widen refused at the last column.
    #[test]
    fn a_cluster_wider_than_its_cells_fills_and_centres_in_them() {
        let repros: [(&str, &[u8], u16, u16); 3] = [
            ("non-RGI ZWJ", "\u{1F408}\u{200D}\u{1F408}".as_bytes(), 0, 2),
            ("modifier", "\u{1F600}\u{1F3FB}".as_bytes(), 0, 2),
            ("refused widen", "\x1b[4G\u{2764}\u{FE0F}".as_bytes(), 3, 1),
        ];
        for (name, bytes, col, cells) in repros {
            let Some(p) = paint_pinned(bytes, 4) else {
                eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
                return;
            };
            let fit_px = f32::from(cluster_fit_px_at(&p, col));
            assert_ne!(fit_px, 0.0, "{name}: fitted");
            // The size is floored to a whole pixel, which costs up to one
            // part in `fit_px + 1` of the block.
            let filled = |ink: f32, block: f32| ink >= block * fit_px / (fit_px + 1.0) - 1.0;
            let m = p.index.cell_metrics();
            let (cw, ch) = (m.width as f32, m.height as f32);
            let (bx0, bx1) = (f32::from(col) * cw, f32::from(col + cells) * cw);
            let [x0, y0, x1, y1] = ink(&p.fg);
            assert!(
                x0 >= bx0 - 1.0 && x1 <= bx1 + 1.0 && y0 >= -1.0 && y1 <= ch + 1.0,
                "{name}: ink {x0}..{x1} x {y0}..{y1} inside {bx0}..{bx1} x 0..{ch}"
            );
            assert!(
                ((x0 - bx0) - (bx1 - x1)).abs() <= 1.0 && (y0 - (ch - y1)).abs() <= 1.0,
                "{name}: ink {x0}..{x1} x {y0}..{y1} centred in {bx0}..{bx1} x 0..{ch}"
            );
            assert!(
                filled(x1 - x0, bx1 - bx0) || filled(y1 - y0, ch),
                "{name}: ink {x0}..{x1} x {y0}..{y1} fills an axis"
            );
        }
    }

    /// The fit is the bare char's: a cluster of one glyph shrinks to the
    /// size `fitted_bitmap` gives that glyph alone.
    #[test]
    fn a_one_glyph_cluster_fits_as_its_bare_char_does() {
        let Some(cluster) = paint_pinned("\x1b[4G\u{263A}\u{FE0F}".as_bytes(), 4) else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let bare = paint_pinned("\x1b[4G\u{263A}".as_bytes(), 4).expect("pinned set");
        let [a0, b0, a1, b1] = ink(&cluster.fg);
        let [c0, d0, c1, d1] = ink(&bare.fg);
        assert!(
            ((a1 - a0) - (c1 - c0)).abs() <= 1.0 && ((b1 - b0) - (d1 - d0)).abs() <= 1.0,
            "cluster {}x{} vs bare {}x{}",
            a1 - a0,
            b1 - b0,
            c1 - c0,
            d1 - d0,
        );
    }

    /// Clusters whose advance stays within their cells keep the native
    /// path: a ligated flag, a widened VS16 emoji, a keycap, a combining mark.
    #[test]
    fn a_cluster_within_its_cells_is_not_fitted() {
        for text in [
            "\u{1F1EF}\u{1F1F5}",
            "\u{2764}\u{FE0F}",
            "#\u{FE0F}\u{20E3}",
            "e\u{301}",
        ] {
            let Some(p) = paint_pinned(text.as_bytes(), 4) else {
                eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
                return;
            };
            assert_eq!(cluster_fit_px_at(&p, 0), 0, "{text:?}");
        }
    }

    /// The fit is measured once per cluster: a later walk reads the memo,
    /// so a repainted prompt does not rasterize its clusters again.
    #[test]
    fn a_fitted_cluster_is_measured_once() {
        let Some(mut p) = paint_pinned("\u{1F408}\u{200D}\u{1F408}".as_bytes(), 4) else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        assert_eq!(p.index.cluster_fits.len(), 1);
        let first = cluster_fit_px_at(&p, 0);
        p.index.cluster_fits.values_mut().for_each(|px| *px += 1);
        let mut frame = ShapeFrame::empty();
        p.index.walked = None;
        GridWalker::default().walk(
            &mut p.index,
            p.grid.screen(),
            &mut Shaper::new(),
            &[],
            &mut frame,
        );
        p.frame = frame;
        assert_eq!(
            cluster_fit_px_at(&p, 0),
            first + 1,
            "the walk read the memo"
        );
    }

    /// Under an OSC 66 fractional scale a fitted cluster's pen shrinks
    /// with its glyphs: the second cat sits one advance at the fitted
    /// size after the first, not one advance at the native size.
    #[test]
    fn a_fitted_cluster_advances_its_pen_at_the_fitted_size() {
        let cats = "\u{1F408}\u{200D}\u{1F408}";
        let Some(stack) = FontStack::try_pinned_test_stack() else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut grid = Grid::new(2, 8);
        Parser::new().advance(
            &mut grid,
            format!("\x1b]66;w=1:s=2:n=3:d=4;{cats}\x07").as_bytes(),
        );
        let mut index = GlyphIndex::new(stack, 18.0, nz(1024));
        let mut frame = ShapeFrame::empty();
        GridWalker::default().walk(
            &mut index,
            grid.screen(),
            &mut Shaper::new(),
            &[],
            &mut frame,
        );
        let ShapedCell::Cluster { start, len, fit_px } = frame.cell_at(0, 0) else {
            panic!("not a cluster: {:?}", frame.cell_at(0, 0));
        };
        assert_ne!(fit_px, 0, "fitted");
        let native = Shaper::new().shape_cluster(index.stack(), 18.0, FontStyle::REGULAR, cats);
        let pool = frame.cluster_slice(start, len);
        let expected = native.glyphs[0].advance_px * f32::from(fit_px) / (18.0 * 1.5);
        assert!(
            (pool[0].advance_px - expected).abs() < 1e-3,
            "pool advance {} vs {expected}",
            pool[0].advance_px
        );
        let theme = crate::palette::Theme::default();
        let mut out = crate::instances::InstanceBuffers::default();
        crate::instances::CellPainter::new(
            grid.screen(),
            index.cell_metrics(),
            &crate::palette::ResolvedTheme::new(&theme),
            &index,
            &frame,
        )
        .with_viewport(0, 2)
        .extend_instances(&mut out);
        let [first, second] = out.fg.as_slice() else {
            panic!("two cats: {} quads", out.fg.len());
        };
        let step = second.origin_px[0] - first.origin_px[0];
        let fitted_advance = native.glyphs[0].advance_px * f32::from(fit_px) / 18.0;
        assert!(
            (step - fitted_advance).abs() < 1e-3,
            "step {step} vs the fitted advance {fitted_advance}"
        );
    }

    /// A font-supplied `│` lands a glyph-sized bitmap shorter than the
    /// cell, so `size_px[1] == cell_height` only holds on the synthetic
    /// path.
    #[test]
    fn vertical_box_drawing_bitmap_exactly_fills_the_cell() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let metrics = idx.cell_metrics();
        let sk = SizingKey::default();
        assert!(idx.ensure('│', metrics.height, sk));
        let slot = idx.slot('│', metrics.height, sk).expect("│ slot");
        assert_eq!(
            slot.size_px,
            [metrics.width, metrics.height],
            "synthetic │ must fill the cell so adjacent rows connect",
        );
        let ascent_i32 = i32::try_from(metrics.ascent).expect("ascent fits in i32");
        assert_eq!(
            slot.offset_px,
            [0, ascent_i32],
            "left=0 + top=ascent pins the bitmap at the cell's top-left",
        );
    }

    #[test]
    fn reload_size_keeps_the_same_font_stack_arc_identity() {
        // `reload_font_size` must not rebuild the `FontStack`; pin the
        // primary `Arc<Font>` identity across the reload.
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let before = Arc::as_ptr(idx.stack().primary());
        let new_stack = idx.stack().clone();
        let atlas_side = nz(idx.atlas_side());
        idx = GlyphIndex::new(new_stack, 18.0, atlas_side);
        let after = Arc::as_ptr(idx.stack().primary());
        assert_eq!(before, after, "size-only reload must not swap fonts");
        assert!(idx.cell_metrics().height > 0);
    }

    #[test]
    fn reload_size_resets_atlas_so_old_slot_uvs_no_longer_resolve() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let h_old = idx.cell_metrics().height;
        let sk = SizingKey::default();
        idx.ensure('A', h_old, sk);
        assert!(idx.contains('A', h_old, sk));
        let stack = idx.stack().clone();
        let atlas_side = nz(idx.atlas_side());
        idx = GlyphIndex::new(stack, 28.0, atlas_side);
        let h_new = idx.cell_metrics().height;
        assert!(h_new != h_old, "new size should change cell height");
        assert!(
            !idx.contains('A', h_new, sk),
            "atlas reset → 'A' must re-rasterize at new size"
        );
    }

    #[test]
    fn allocations_within_the_atlas_never_overlap_in_uv_space() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        let mut rects: Vec<(f32, f32, f32, f32)> = Vec::new();
        for c in 0x21u8..=0x7Eu8 {
            idx.ensure(c as char, h, sk);
            if let Some(s) = idx.slot(c as char, h, sk) {
                rects.push((s.uv_min[0], s.uv_min[1], s.uv_max[0], s.uv_max[1]));
            }
        }
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                let separated = a.2 <= b.0 || b.2 <= a.0 || a.3 <= b.1 || b.3 <= a.1;
                assert!(separated, "overlap between {a:?} and {b:?}");
            }
        }
    }

    /// Same idempotence and negative-entry handling as the per-char
    /// path.
    #[test]
    fn ensure_glyph_id_is_idempotent_and_caches_a_slot() {
        use felis_shaping::Shaper;
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        let primary = idx.stack().primary().clone();
        let mut shaper = Shaper::new();
        let shaped = shaper.shape_run(&primary, 14.0, &[], "A");
        let glyph_id = shaped[0].glyph_id;
        assert_ne!(glyph_id, 0);
        let first = idx.ensure_glyph_id(glyph_id, h, sk);
        assert!(first, "first ensure must rasterize + allocate");
        assert!(idx.contains_glyph_id(glyph_id, h, sk));
        assert!(idx.glyph_id_slot(glyph_id, h, sk).is_some());
        let second = idx.ensure_glyph_id(glyph_id, h, sk);
        assert!(!second, "second ensure is a cache hit");
    }

    /// `.notdef` stores a negative entry, like a charmap miss.
    #[test]
    fn ensure_glyph_id_zero_lands_a_negative_slot() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
        let h = idx.cell_metrics().height;
        let sk = SizingKey::default();
        idx.ensure_glyph_id(0, h, sk);
        assert!(idx.contains_glyph_id(0, h, sk));
        assert_eq!(idx.glyph_id_slot(0, h, sk), None);
        assert_eq!(idx.drain_pending().count(), 0);
    }

    #[test]
    fn empty_shape_frame_reports_none_for_every_cell() {
        let frame = ShapeFrame::empty();
        assert_eq!(frame.cell_at(0, 0), ShapedCell::None);
        assert_eq!(frame.cell_at(100, 100), ShapedCell::None);
        assert_eq!(frame.cols(), 0);
        assert_eq!(frame.cells(), []);
    }

    /// Non-ASCII graphemes split the run; the wide cell falls back to
    /// the per-char path.
    #[test]
    fn shape_run_stops_at_non_ascii_grapheme() {
        let mut screen = Grid::new(1, 5);
        let mut parser = Parser::new();
        parser.advance(&mut screen, "aあb".as_bytes());
        let stack = stack();
        let primary = stack.primary().clone();
        let mut text = String::new();
        let run = next_shape_run(screen.screen(), 0, 0, &mut text, |c, _style| {
            primary.has_glyph(c)
        })
        .expect("first cell must start a run");
        assert_eq!(text, "a");
        assert_eq!(run.cell_count, 1, "non-ASCII grapheme stops the sweep");
    }

    /// The same raw `glyph_id` primed against two faces keys two slots;
    /// the second `ensure_glyph_id` reports work only because the key
    /// differs.
    #[test]
    fn glyph_id_slots_key_on_font_id_so_faces_do_not_collide() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(512));
        let h = idx.cell_metrics().height;
        let face0 = SizingKey::default();
        let face2 = SizingKey::default().with_font_id(2);
        assert_ne!(face0, face2, "font-id must change the key");
        assert!(
            idx.ensure_glyph_id(7, h, face0),
            "first face-0 insert works"
        );
        assert!(
            idx.ensure_glyph_id(7, h, face2),
            "same id under a different font-id is a distinct slot, not a hit",
        );
        assert!(
            !idx.ensure_glyph_id(7, h, face0),
            "re-inserting the face-0 key is a cache hit",
        );
        assert!(idx.contains_glyph_id(7, h, face0));
        assert!(idx.contains_glyph_id(7, h, face2));
    }

    /// Single-glyph ligature cluster (the FiraCode-style collapse).
    /// Hand-rolled `ShapedGlyph` values so the shape under test cannot
    /// drift with the pinned font's GSUB.
    #[test]
    fn single_glyph_ligature_cluster_marks_primary_then_trailing() {
        let stack = stack();
        let mut idx = GlyphIndex::new(stack, 14.0, nz(512));
        let mut frame_cells = vec![ShapedCell::None; 4];
        let shaped = [ShapedGlyph {
            glyph_id: 42,
            source_byte_start: 0,
            source_byte_len: 2,
            advance_px: 16.0,
            x_offset_px: 0.0,
            y_offset_px: 0.0,
            from_ligature: true,
        }];
        apply_shaped_run(
            &mut frame_cells,
            &mut idx,
            4,
            &RunPlacement {
                row: 0,
                start_col: 0,
                cell_count: 2,
                style: FontStyle::REGULAR,
            },
            &shaped,
        );
        assert_eq!(
            frame_cells[0],
            ShapedCell::Primary {
                glyph_id: 42,
                span_cols: 2,
            },
        );
        assert_eq!(frame_cells[1], ShapedCell::Trailing);
        assert_eq!(frame_cells[2], ShapedCell::None);
    }

    /// Multi-glyph ligature cluster (the Monaspace-style piece form: a
    /// blank lead glyph plus the body, both claiming the full span). The
    /// body's ink hangs off the second cell's pen with a negative left
    /// bearing, so it must land on cell 1.
    #[test]
    fn multi_glyph_ligature_cluster_places_pieces_by_pen_position() {
        let stack = stack();
        let mut idx = GlyphIndex::new(stack, 14.0, nz(512));
        let mut frame_cells = vec![ShapedCell::None; 4];
        let piece = |glyph_id| ShapedGlyph {
            glyph_id,
            source_byte_start: 0,
            source_byte_len: 2,
            advance_px: 8.0,
            x_offset_px: 0.0,
            y_offset_px: 0.0,
            from_ligature: true,
        };
        let shaped = [piece(100), piece(200)];
        apply_shaped_run(
            &mut frame_cells,
            &mut idx,
            4,
            &RunPlacement {
                row: 0,
                start_col: 0,
                cell_count: 2,
                style: FontStyle::REGULAR,
            },
            &shaped,
        );
        assert_eq!(
            frame_cells[0],
            ShapedCell::Primary {
                glyph_id: 100,
                span_cols: 1,
            },
            "lead piece (pen 0) anchors at the cluster's first cell",
        );
        assert_eq!(
            frame_cells[1],
            ShapedCell::Primary {
                glyph_id: 200,
                span_cols: 1,
            },
            "body piece (pen = one advance) anchors at the second cell",
        );
        assert_eq!(frame_cells[2], ShapedCell::None);
    }

    /// A ligature wider than the cells it replaces is dropped, so the
    /// per-char path draws its source chars; one within them is kept.
    #[test]
    fn a_ligature_wider_than_its_cells_leaves_them_unshaped() {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(512));
        let cw = idx.cell_metrics().width as f32;
        let run = RunPlacement {
            row: 0,
            start_col: 0,
            cell_count: 2,
            style: FontStyle::REGULAR,
        };
        let ligature = |advance_px| ShapedGlyph {
            glyph_id: 42,
            source_byte_start: 0,
            source_byte_len: 2,
            advance_px,
            x_offset_px: 0.0,
            y_offset_px: 0.0,
            from_ligature: true,
        };
        let mut wide = vec![ShapedCell::None; 2];
        apply_shaped_run(
            &mut wide,
            &mut idx,
            2,
            &run,
            &[ligature(cw.mul_add(2.0, 3.0))],
        );
        assert_eq!(wide, [ShapedCell::None, ShapedCell::None]);
        let mut fitting = vec![ShapedCell::None; 2];
        apply_shaped_run(
            &mut fitting,
            &mut idx,
            2,
            &run,
            &[ligature(cw.mul_add(2.0, 1.0))],
        );
        assert_eq!(
            fitting,
            [
                ShapedCell::Primary {
                    glyph_id: 42,
                    span_cols: 2,
                },
                ShapedCell::Trailing,
            ],
        );
    }

    /// Shaping `"->"` with `ss03` through the real run-scan + shape +
    /// apply pipeline against Monaspace Neon: both cells consumed, the
    /// visible arrow on cell 1. Skips when `FELIS_TEST_FONT_DIR` is
    /// unset; `flake.nix` exports it so CI always runs it.
    #[test]
    fn ligature_run_places_monaspace_arrow_pieces() {
        use felis_shaping::{Font, FontStack, Shaper};
        let Some(font) = Font::try_load_test_font() else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let features = vec!["ss03".to_owned()];
        let mut screen = Grid::new(1, 6);
        let mut parser = Parser::new();
        parser.advance(&mut screen, b"-> ab");
        let primary = Arc::new(font);
        let stack = FontStack::with_primary_features(primary, features.clone());
        let mut idx = GlyphIndex::new(stack, 14.0, nz(512));
        let cell_height = idx.cell_metrics().height;
        let cols = screen.cols();
        let cell_count = usize::from(screen.rows()) * usize::from(cols);
        let mut frame_cells = vec![ShapedCell::None; cell_count];
        let mut shaper = Shaper::new();
        let primary_face = idx.stack().primary().clone();
        let mut col = 0u16;
        let mut run_text = String::new();
        while col < cols {
            let Some(run) = next_shape_run(screen.screen(), 0, col, &mut run_text, |c, _style| {
                primary_face.has_glyph(c)
            }) else {
                col += 1;
                continue;
            };
            let run_start = run.start_col;
            let run_cell_count = run.cell_count;
            col = run_start + run_cell_count;
            if run_text.is_empty() {
                continue;
            }
            let shaped = shaper.shape_run(&primary_face, 14.0, &features, &run_text);
            apply_shaped_run(
                &mut frame_cells,
                &mut idx,
                cols,
                &RunPlacement {
                    row: 0,
                    start_col: run_start,
                    cell_count: run_cell_count,
                    style: FontStyle::REGULAR,
                },
                &shaped,
            );
        }
        // Unsubstituted gids via a features-off shape of each char alone:
        // the raw charmap is not public outside felis-shaping.
        let plain_gid = |shaper: &mut Shaper, c: &str| {
            let out = shaper.shape_run(&primary_face, 14.0, &[], c);
            assert_eq!(out.len(), 1);
            out[0].glyph_id
        };
        let plain_hyphen = plain_gid(&mut shaper, "-");
        let plain_greater = plain_gid(&mut shaper, ">");
        match frame_cells[1] {
            ShapedCell::Primary {
                glyph_id,
                span_cols,
            } => {
                assert_ne!(glyph_id, 0, "arrow body glyph id must be non-zero");
                assert_ne!(
                    glyph_id, plain_greater,
                    "cell 1 must carry the substituted arrow piece, not `>`",
                );
                assert_eq!(span_cols, 1, "piece glyphs claim one cell each");
                assert!(idx.contains_glyph_id(glyph_id, cell_height, SizingKey::default()));
            }
            other => panic!("expected the arrow body Primary at (0,1), got {other:?}"),
        }
        match frame_cells[0] {
            ShapedCell::Primary { glyph_id, .. } => {
                assert_ne!(
                    glyph_id, plain_hyphen,
                    "cell 0 must carry the blank lead piece, not `-`",
                );
            }
            ShapedCell::Trailing => {}
            ShapedCell::None | ShapedCell::Cluster { .. } => {
                panic!("cell 0 must be consumed by the shaped ligature cluster")
            }
        }
    }

    /// The memo returns byte-identical output to a direct
    /// `Shaper::shape_run` and reports hit/miss like `ensure`.
    #[test]
    fn shape_run_cached_matches_direct_output_and_hits_on_repeat() {
        let stack = stack();
        let primary = stack.primary().clone();
        let features = vec!["calt".to_owned()];
        let mut idx = GlyphIndex::new(stack, 14.0, nz(256));
        let mut shaper = Shaper::new();
        let direct = shaper.shape_run(&primary, 14.0, &features, "Hello");
        let mut out = Vec::new();
        let first = idx.shape_run_cached(
            &mut shaper,
            &features,
            FontStyle::REGULAR,
            "Hello",
            &mut out,
        );
        assert!(first, "first call must shape (miss)");
        assert_eq!(out, direct, "memoized output must match a direct shape");
        let second = idx.shape_run_cached(
            &mut shaper,
            &features,
            FontStyle::REGULAR,
            "Hello",
            &mut out,
        );
        assert!(!second, "second call must be a cache hit");
        assert_eq!(out, direct, "hit must replay the identical output");
    }

    /// Scrollback produces unbounded unique row texts, so the memo must
    /// stay bounded.
    #[test]
    fn shape_run_cache_stays_bounded_under_unique_text_pressure() {
        let stack = stack();
        let mut idx = GlyphIndex::new(stack, 14.0, nz(256));
        let mut shaper = Shaper::new();
        let mut out = Vec::new();
        let cap = GlyphIndex::SHAPED_RUN_CACHE_MAX;
        for i in 0..(cap + 16) {
            let text = format!("line {i}");
            idx.shape_run_cached(&mut shaper, &[], FontStyle::REGULAR, &text, &mut out);
        }
        assert!(
            idx.shaped_run_cache_len() <= cap,
            "cache len {} must not exceed cap {cap}",
            idx.shaped_run_cache_len(),
        );
        let kept = format!("line {}", cap + 15);
        assert!(
            !idx.shape_run_cached(&mut shaper, &[], FontStyle::REGULAR, &kept, &mut out),
            "most-recent insert must survive the eviction sweep",
        );
    }

    /// A color change mid-row must not end a run (kitty ligates across
    /// syntax-highlight boundaries); per-cell color is applied at
    /// emission by `push_sliced_by_cell`.
    #[test]
    fn shape_run_continues_across_attribute_boundary() {
        use felis_grid::{Attributes, Cell, Color};
        let mut screen = Grid::new(1, 4);
        let default_style = screen.style_table_mut().intern(Attributes::default());
        screen.set_cell(
            0,
            0,
            Cell {
                grapheme: Grapheme::Ascii(b'='),
                style: default_style,
                link: None,
                sizing: None,
            },
        );
        let differ = screen.style_table_mut().intern(Attributes {
            fg: Color::Indexed(1),
            ..Attributes::default()
        });
        screen.set_cell(
            0,
            1,
            Cell {
                grapheme: Grapheme::Ascii(b'>'),
                style: differ,
                link: None,
                sizing: None,
            },
        );
        let stack = stack();
        let primary = stack.primary().clone();
        let mut text = String::new();
        let run0 = next_shape_run(screen.screen(), 0, 0, &mut text, |c, _style| {
            primary.has_glyph(c)
        })
        .expect("first cell must start a run");
        assert_eq!(run0.cell_count, 2, "color boundary must not end the run");
        assert_eq!(text, "=>");
    }

    /// Unlike color, a bold/italic change ends the run.
    #[test]
    fn shape_run_breaks_at_bold_italic_boundary() {
        use felis_grid::{AttrFlags, Attributes, Cell};
        let mut screen = Grid::new(1, 4);
        let default_style = screen.style_table_mut().intern(Attributes::default());
        screen.set_cell(
            0,
            0,
            Cell {
                grapheme: Grapheme::Ascii(b'='),
                style: default_style,
                link: None,
                sizing: None,
            },
        );
        let bold = screen.style_table_mut().intern(Attributes {
            flags: AttrFlags::BOLD,
            ..Attributes::default()
        });
        screen.set_cell(
            0,
            1,
            Cell {
                grapheme: Grapheme::Ascii(b'>'),
                style: bold,
                link: None,
                sizing: None,
            },
        );
        let stack = stack();
        let primary = stack.primary().clone();
        let mut text = String::new();
        let run0 = next_shape_run(screen.screen(), 0, 0, &mut text, |c, _style| {
            primary.has_glyph(c)
        })
        .expect("first cell must start a run");
        assert_eq!(run0.cell_count, 1, "bold boundary must end the run");
        assert_eq!(text, "=");
        assert_eq!(run0.style, FontStyle::REGULAR);
        let run1 = next_shape_run(screen.screen(), 0, 1, &mut text, |c, _style| {
            primary.has_glyph(c)
        })
        .expect("second cell starts a fresh run");
        assert_eq!(text, ">");
        assert_eq!(
            run1.style,
            FontStyle {
                bold: true,
                italic: false
            },
        );
    }

    /// The style rides the `SizingKey`, so bold and regular `'A'` key
    /// separate slots.
    #[test]
    fn styled_cells_get_distinct_atlas_slots() {
        let stack = stack();
        let mut idx = GlyphIndex::new(stack, 14.0, nz(512));
        let h = idx.cell_metrics().height;
        let regular = SizingKey::default();
        let bold = SizingKey::default().with_style(FontStyle {
            bold: true,
            italic: false,
        });
        assert!(idx.ensure('A', h, regular), "regular 'A' is a miss");
        assert!(
            idx.ensure('A', h, bold),
            "bold 'A' must be a separate miss, not a hit on the regular slot",
        );
        assert!(idx.contains('A', h, regular));
        assert!(idx.contains('A', h, bold));
    }
}

#[cfg(test)]
mod walk_tests;

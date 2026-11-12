//! Glyph index that bridges `felis_shaping` with a backend's glyph
//! atlas: [`GlyphIndex`] shapes, rasterizes and packs glyphs, and the
//! backend drains its [`PendingUpload`]s into wherever its atlas lives
//! (`felis-render-wgpu`'s `GlyphCache` writes them into a texture).

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use felis_grid::{AttrFlags, Grapheme, HAlign, ScreenBuffer, Sizing, VAlign};
use felis_shaping::{
    CellMetrics, FACE_ID_SATURATED, Font, FontStack, FontStyle, GlyphBitmap, GlyphId, GlyphPixels,
    ShapeCache, ShapedGlyph, Shaper, SizingKey,
};
use foldhash::fast::RandomState;

use crate::{
    atlas::{Allocation, ShelfAtlas},
    box_drawing,
    display_list::AtlasKind,
};

/// Glyphs are cell-sized, so the 8192 image ceiling would be gross
/// over-allocation: a 2048² `R8` sheet is 4 MiB against 64 MiB and holds
/// thousands of slots; the packer's evict-and-rebuild path absorbs
/// overflow. Revisit if CJK-heavy sessions show glyph atlas churn.
pub const GLYPH_ATLAS_SIDE: u32 = 2048;

/// Color-glyph atlas side ceiling. At four bytes per pixel an `Rgba8`
/// sheet half the mono side costs the same bytes as the mono `R8` sheet
/// and still holds hundreds of emoji before a reset recycles it.
pub const COLOR_GLYPH_ATLAS_SIDE: u32 = 1024;
const _: () = assert!(COLOR_GLYPH_ATLAS_SIDE < GLYPH_ATLAS_SIDE);

/// Never larger than the mono sheet. Shared by [`GlyphIndex`] and the
/// backend's color atlas so their UV spaces agree.
#[must_use]
pub fn color_atlas_side(mono_side: NonZeroU32) -> NonZeroU32 {
    NonZeroU32::new(mono_side.get().min(COLOR_GLYPH_ATLAS_SIDE)).unwrap_or(mono_side)
}

/// Base scalar of a [`Grapheme::Cluster`] cell. The compositing pass,
/// `populate`'s priming, and the emission fallback all resolve through
/// this so they agree on the scalar. `None` while the handle is not yet
/// in the screen's cluster table (a transient reattach state).
#[must_use]
pub fn cluster_base_char(screen: &ScreenBuffer, id: NonZeroU32) -> Option<char> {
    screen.cluster_str(id).and_then(|s| s.chars().next())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GlyphSlot {
    /// Top-left of the bitmap in its atlas sheet, in pixels.
    pub atlas_px: [u32; 2],
    /// Pixel size of the glyph bitmap (not the cell).
    pub size_px: [u32; 2],
    /// `(left, top)` from the cell's `(0, baseline)` origin, as rasterized.
    pub offset_px: [i32; 2],
    /// True when the bitmap lives in the RGBA color atlas.
    pub is_color: bool,
    /// A `box_drawing` bitmap sized to the whole scaled cell, which takes
    /// none of the glyph-box padding.
    pub fills_cell: bool,
}

pub trait AtlasView {
    /// `None` on a miss (cache not populated, atlas full, whitespace).
    /// `sizing` keeps OSC 66 sized variants on separate slots.
    fn slot(&self, glyph: char, cell_height_px: u32, sizing: SizingKey) -> Option<GlyphSlot>;

    /// Deliberately no default impl: a `None` default would be silently
    /// inherited by `GlyphCache`, and every shaped lookup would miss, so
    /// any non-empty `font.features` would render primary-font runs
    /// blank.
    fn glyph_id_slot(
        &self,
        glyph_id: GlyphId,
        cell_height_px: u32,
        sizing: SizingKey,
    ) -> Option<GlyphSlot>;

    /// The slot of a cluster glyph shrunk to `px` to fit its cells;
    /// see [`ShapedCell::Cluster`].
    fn fitted_glyph_id_slot(
        &self,
        glyph_id: GlyphId,
        px: u16,
        cell_height_px: u32,
        sizing: SizingKey,
    ) -> Option<GlyphSlot>;

    /// The shaped glyphs of a multi-scalar overlay cluster, `None` until
    /// the overlay's text was populated.
    fn overlay_cluster(&self, text: &str) -> Option<&[ClusterGlyph]>;

    /// The pixel side of the sheet a slot's [`GlyphSlot::atlas_px`] is in.
    fn sheet_side(&self, sheet: AtlasKind) -> u32;
}

/// Mirrors the enum-to-u8 mapping `felis-vt`'s parser enforces.
#[must_use]
pub const fn sizing_key_from(sizing: Sizing) -> SizingKey {
    let valign = match sizing.valign() {
        VAlign::Top => 0,
        VAlign::Bottom => 1,
        VAlign::Center => 2,
    };
    let halign = match sizing.halign() {
        HAlign::Left => 0,
        HAlign::Right => 1,
        HAlign::Center => 2,
    };
    SizingKey::new(
        sizing.scale(),
        sizing.cell_width(),
        sizing.frac_num(),
        sizing.frac_den(),
        valign,
        halign,
    )
}

/// Only `BOLD` / `ITALIC` participate; `FAINT` does not change glyph
/// selection.
#[must_use]
pub const fn font_style_of(flags: AttrFlags) -> FontStyle {
    FontStyle {
        bold: flags.contains(AttrFlags::BOLD),
        italic: flags.contains(AttrFlags::ITALIC),
    }
}

/// The columns a cluster's block spans before the OSC 66 scale: `w`,
/// else the grid's span, so a VS16 widen the grid refused gets one.
#[must_use]
pub fn cluster_block_cols(screen: &ScreenBuffer, row: u16, col: u16) -> u16 {
    match screen.cell_sizing(row, col).map_or(0, |s| s.cell_width()) {
        0 => {
            let (first, last) = screen.char_span(row, col);
            last - first + 1
        }
        w => u16::from(w),
    }
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
    /// Avoids shaping costs on every frame.
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

    /// `font.cell_width_scale` / `font.cell_height_scale`; call before
    /// the first `ensure`, whose slots it would leave mispadded.
    #[must_use]
    pub fn with_cell_scale(mut self, scale: [f32; 2]) -> Self {
        self.metrics = self.metrics.with_cell_scale(scale);
        self
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
        let layout_scale = f32::from(sizing.scale().max(1));
        let scaled = CellMetrics::unscaled(
            ((self.metrics.width as f32) * scale).round().max(1.0) as u32,
            ((cell_height as f32) * scale).round().max(1.0) as u32,
            self.metrics.baseline(scale, layout_scale).round().max(0.0) as u32,
        );
        if let Some(bitmap) = box_drawing::rasterize(glyph, scaled) {
            let slot = self.allocate(bitmap).map(|slot| GlyphSlot {
                fills_cell: true,
                ..slot
            });
            self.insert_slot(key, slot);
            return true;
        }
        let font = self.stack.resolve(glyph, sizing.style()).clone();
        let bitmap = if let Some(bitmap) = self.fitted_bitmap(&font, glyph, sizing) {
            bitmap
        } else {
            let px = (self.font_size_physical_px * scale).round() as u32;
            let mut bitmap = self
                .shape
                .get_or_insert_sized(&font, glyph, px, sizing)
                .clone();
            bitmap.left += self.pad_x(sizing);
            bitmap
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
        sizing: SizingKey,
    ) -> Option<GlyphBitmap> {
        let glyph_scale = sizing.effective_scale();
        let base_px = self.font_size_physical_px * glyph_scale;
        let cells = match sizing.cell_width() {
            0 => felis_grid::char_cell_width(glyph).max(1),
            w => w,
        };
        let block = self.fit_block(cells.into(), sizing);
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
        bitmap.left = ((block.w - bitmap.width() as f32) / 2.0).round() as i32 + self.pad_x(sizing);
        let ascent = self.metrics.glyph_ascent() * glyph_scale;
        bitmap.top = (ascent - (block.h - bitmap.height() as f32) / 2.0).round() as i32;
        Some(bitmap)
    }

    /// `cells` glyph boxes at the integer scale `s`, whatever the
    /// fractional glyph scale. The glyph box, not the scaled cell, so a
    /// `font.cell_*_scale` shrink never shrinks ordinary text.
    fn fit_block(&self, cells: u16, sizing: SizingKey) -> FitBlock {
        let layout_scale = f32::from(sizing.scale().max(1));
        let [glyph_w, glyph_h] = self.metrics.glyph_box;
        FitBlock {
            w: (glyph_w * u32::from(cells)) as f32 * layout_scale,
            h: glyph_h as f32 * layout_scale,
            slack: layout_scale,
        }
    }

    /// Font glyphs sit in the glyph box, `pad` into the scaled cell;
    /// `box_drawing` glyphs fill the cell and take none.
    fn pad_x(&self, sizing: SizingKey) -> i32 {
        self.metrics.pad[0] * i32::from(sizing.scale().max(1))
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

    /// [`Self::ensure_glyph_id`] at `px`, the size `cluster_fit_px`
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
        let mut bitmap = self.shape.rasterize_glyph_id(&font, glyph_id, px);
        bitmap.left += self.pad_x(sizing);
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
        let block = self.fit_block(cols, sizing);
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
    pub fn trim_overlay_clusters(&mut self) {
        if self.overlay_clusters.len() > Self::OVERLAY_CLUSTER_MAX {
            self.overlay_clusters.clear();
        }
    }

    /// Shapes `text`, one multi-scalar cluster of overlay text, as
    /// `composite_cluster` shapes a cluster cell, and primes its glyphs.
    pub fn ensure_overlay_cluster(&mut self, shaper: &mut Shaper, text: &str) {
        if !self.overlay_clusters.contains_key(text) {
            let shaping = shaper.shape_cluster(
                &self.stack,
                self.font_size_physical_px,
                FontStyle::REGULAR,
                text,
            );
            let font_id = saturated_face_id(shaping.font_id);
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
        let primes: Vec<(GlyphId, u16)> = self.overlay_clusters[text]
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
                .map(|s| sizing_key_from(*s))
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
        let sheet = if is_color {
            &mut self.color_atlas
        } else {
            &mut self.atlas
        };
        let Some(alloc) = sheet.alloc(width, height) else {
            return AllocOutcome::Full(bitmap);
        };
        let atlas_px = [alloc.x, alloc.y];
        self.pending.push(PendingUpload {
            alloc,
            pixels: bitmap.into_pixels(),
        });
        AllocOutcome::Placed(GlyphSlot {
            atlas_px,
            size_px: [width, height],
            offset_px,
            is_color,
            fills_cell: false,
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
/// `CellPainter` slices quads per cell instead. Bold and italic changes do end the run
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
    let cell_width = index.cell_metrics().glyph_box[0] as f32;
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

    fn sheet_side(&self, sheet: AtlasKind) -> u32 {
        match sheet {
            AtlasKind::Mono => self.atlas.width(),
            AtlasKind::Color => self.color_atlas.width(),
        }
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

fn saturated_face_id(font_id: usize) -> u16 {
    u16::try_from(font_id.min(FACE_ID_SATURATED)).unwrap_or(u16::MAX)
}

/// Produced while the [`ShapeFrame`] is built and read back in
/// the cell painter, so priming and emission cannot drift.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterGlyph {
    pub glyph_id: GlyphId,
    /// [`SizingKey::with_font_id`] handle, already saturated to
    /// [`FACE_ID_SATURATED`]; priming and emission both rebuild the key
    /// with it.
    pub font_id: u16,
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

    /// The quad-slicing tests in `painter.rs` build frames directly
    /// because the full pipeline needs a real ligature font.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    #[must_use]
    pub const fn from_cells(cells: Vec<ShapedCell>, cols: u16) -> Self {
        Self {
            cells,
            cols,
            cluster_glyphs: Vec::new(),
            live_cluster_glyphs: 0,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    #[must_use]
    pub const fn from_cluster_cells(
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

/// Scratch and bookkeeping for the grid walk, kept apart from the GPU
/// resources so tests drive it against a bare [`GlyphIndex`].
#[derive(Default)]
pub struct GridWalker {
    run_text: String,
    run_glyphs: Vec<ShapedGlyph>,
    rows: Vec<u16>,
}

impl GridWalker {
    /// Walks only the rows `screen` marked dirty while `frame` and the
    /// index's slots still hold everything the last walk put there, and
    /// every row otherwise. With empty `features` and no interned
    /// clusters this avoids shaping overhead. `shaper` is borrowed
    /// mutably so its shape-plan cache stays resident.
    pub fn walk(
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
        .map(|s| sizing_key_from(*s))
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
    let font_id = saturated_face_id(shaping.font_id);
    let cols = cluster_block_cols(screen, site.row, site.col);
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

#[cfg(test)]
mod tests;

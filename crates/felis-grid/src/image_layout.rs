//! Where each Kitty image draws this frame, as plain rectangles every
//! frontend paints with its own primitive (`docs/reference/protocols/kitty-graphics.md`
//! "Z-ordering", "Unicode-placeholder placement").

use std::collections::{HashMap, HashSet};

use felis_protocol::messages::{RequestedExtent, SourceRect};

use crate::{
    ScreenBuffer,
    images::{ClientPlacement, ImageId, VirtualPlacement, clip_source},
};

/// What the layout needs to know about an image: its pixel size, and
/// only once it can draw. An image mid-transfer answers `None`.
pub trait LayoutImages {
    fn drawable_size(&self, id: ImageId) -> Option<(u32, u32)>;

    /// The image's place in creation order: kitty stacks equal-z draws
    /// by it before the placement's own.
    fn stack_rank(&self, id: ImageId) -> u64;
}

/// kitty's `(z_index, image id, ref id)`. `kind` puts an image's
/// placeholder tiles above its direct placements: kitty mints a
/// placeholder ref each time its row renders, so it outranks every
/// older ref of the image, and the tiles keep cell order among
/// themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct StackKey {
    z: i32,
    image: u64,
    kind: DrawKind,
    placement: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DrawKind {
    Direct,
    Placeholder,
}

/// One rectangle of one image. `src` is in the image's own pixels and
/// `dst` in screen pixels from the grid's top-left, both `[x, y, w, h]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageDraw {
    pub image_id: ImageId,
    /// The size `src` was computed against; a frontend holding pixels of
    /// another size has a stale copy and skips the draw.
    pub image_size: [u32; 2],
    pub src: [f32; 4],
    pub dst: [f32; 4],
}

/// kitty draws every Unicode-placeholder ref at this z, whatever `z=`
/// its virtual placement recorded.
const PLACEHOLDER_Z: i32 = -1;

/// kitty's three image layers: under the cell backgrounds, between
/// backgrounds and glyphs, above glyphs.
const Z_UNDER_BG_MAX: i32 = i32::MIN / 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZLayer {
    UnderBg,
    UnderText,
    AboveText,
}

const fn z_layer(z: i32) -> ZLayer {
    if z < Z_UNDER_BG_MAX {
        ZLayer::UnderBg
    } else if z < 0 {
        ZLayer::UnderText
    } else {
        ZLayer::AboveText
    }
}

/// How the last rebuild resolved Unicode-placeholder cells, for the
/// frontend's debug log: a producer whose image never shows is told
/// apart by which count stays at zero.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlaceholderStats {
    pub virtual_placements: usize,
    pub cells: u32,
    pub tiles: u32,
    /// The cell's image has no pixels to draw yet.
    pub no_image: u32,
    /// No virtual placement matches the cell's image and placement id.
    pub no_placement: u32,
}

/// Each layer is in paint order, back to front. Kept across frames so a
/// redraw reuses its buffers.
#[derive(Debug, Default)]
pub struct ImageLayout {
    under_bg: Vec<ImageDraw>,
    under_text: Vec<ImageDraw>,
    above_text: Vec<ImageDraw>,
    live: Vec<ImageId>,
    placeholder_stats: PlaceholderStats,
    sorted: Vec<(StackKey, ImageDraw)>,
    vp_map: HashMap<(ImageId, u32), VirtualPlacement>,
    seen: HashSet<ImageId>,
}

impl ImageLayout {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn under_bg(&self) -> &[ImageDraw] {
        &self.under_bg
    }

    #[must_use]
    pub fn under_text(&self) -> &[ImageDraw] {
        &self.under_text
    }

    #[must_use]
    pub fn above_text(&self) -> &[ImageDraw] {
        &self.above_text
    }

    /// Every image a placement names, drawn this frame or not, deduped
    /// in first-mention order: what a frontend that dropped its pixel
    /// cache must restore, since a producer never re-sends an image.
    #[must_use]
    pub fn live_images(&self) -> &[ImageId] {
        &self.live
    }

    #[must_use]
    pub const fn placeholder_stats(&self) -> PlaceholderStats {
        self.placeholder_stats
    }

    /// `screen` is the composed viewport the client paints, so
    /// placeholder cells are already where they show; anchored
    /// placements live in live-screen rows and shift by `viewport`.
    pub fn rebuild(
        &mut self,
        screen: &ScreenBuffer,
        images: &impl LayoutImages,
        placements: &[ClientPlacement],
        virtual_placements: &[VirtualPlacement],
        cell_px: (f32, f32),
        viewport: u32,
    ) {
        self.sorted.clear();
        self.collect_live(placements, virtual_placements);
        self.push_placements(screen, images, placements, cell_px, viewport);
        self.push_placeholders(screen, images, virtual_placements, cell_px);
        // Stable: placeholder tiles tie and keep cell order, as do the
        // placements of a daemon that sends no serial, in shadow order.
        self.sorted.sort_by_key(|(key, _)| *key);
        self.under_bg.clear();
        self.under_text.clear();
        self.above_text.clear();
        for &(key, draw) in &self.sorted {
            match z_layer(key.z) {
                ZLayer::UnderBg => self.under_bg.push(draw),
                ZLayer::UnderText => self.under_text.push(draw),
                ZLayer::AboveText => self.above_text.push(draw),
            }
        }
    }

    fn collect_live(
        &mut self,
        placements: &[ClientPlacement],
        virtual_placements: &[VirtualPlacement],
    ) {
        self.live.clear();
        self.seen.clear();
        let ids = placements
            .iter()
            .map(|p| p.image_id)
            .chain(virtual_placements.iter().map(|v| v.image_id));
        for id in ids {
            if self.seen.insert(id) {
                self.live.push(id);
            }
        }
    }

    fn push_placements(
        &mut self,
        screen: &ScreenBuffer,
        images: &impl LayoutImages,
        placements: &[ClientPlacement],
        (cw, ch): (f32, f32),
        viewport: u32,
    ) {
        let rows = i64::from(screen.rows());
        for p in placements {
            let Some((width, height)) = images.drawable_size(p.image_id) else {
                continue;
            };
            // Mirrors `ScreenBuffer::cell_at_viewport`: live row L paints
            // at visible row L + viewport.
            let top_row = i64::from(p.anchor_row) - 1 + i64::from(viewport);
            if top_row >= rows {
                continue;
            }
            let Some(draw) = draw_for_placement(p, width, height, cw, ch, viewport) else {
                continue;
            };
            // The drawn height, not the daemon's row count: an automatic
            // axis draws at this window's cell size, which a mirror with
            // smaller cells than the daemon resolved against can exceed.
            if draw.dst[1] + draw.dst[3] <= 0.0 {
                continue;
            }
            let key = StackKey {
                z: p.z_index,
                image: images.stack_rank(p.image_id),
                kind: DrawKind::Direct,
                placement: p.serial.unwrap_or(0),
            };
            self.sorted.push((key, draw));
        }
    }

    fn push_placeholders(
        &mut self,
        screen: &ScreenBuffer,
        images: &impl LayoutImages,
        virtual_placements: &[VirtualPlacement],
        (cw, ch): (f32, f32),
    ) {
        self.placeholder_stats = PlaceholderStats {
            virtual_placements: virtual_placements.len(),
            ..PlaceholderStats::default()
        };
        if virtual_placements.is_empty() {
            return;
        }
        self.vp_map.clear();
        for v in virtual_placements {
            // A cell naming no placement takes its image's first; one
            // without an id (a minor-0 daemon's) answers every cell.
            self.vp_map.entry((v.image_id, 0)).or_insert(*v);
            let key = v.placement_id.map_or(0, |p| p.0);
            self.vp_map.entry((v.image_id, key)).or_insert(*v);
        }
        for pc in screen.placeholder_cells() {
            self.placeholder_stats.cells += 1;
            let found = self
                .vp_map
                .get(&(pc.image_id, pc.placement_id))
                .or_else(|| {
                    self.vp_map
                        .get(&(pc.image_id, 0))
                        .filter(|v| v.placement_id.is_none())
                });
            let Some(&vp) = found else {
                self.placeholder_stats.no_placement += 1;
                continue;
            };
            let Some((width, height)) = images.drawable_size(pc.image_id) else {
                self.placeholder_stats.no_image += 1;
                continue;
            };
            let (total_cols, total_rows) =
                placeholder_grid_extent(vp.cols, vp.rows, width, height, cw, ch);
            let tile = PlaceholderTile {
                screen_row: pc.row,
                screen_col: pc.col,
                tile_row: pc.tile_row,
                tile_col: pc.tile_col,
                total_cols,
                total_rows,
            };
            if let Some(draw) = placeholder_tile_draw(pc.image_id, width, height, cw, ch, tile) {
                self.placeholder_stats.tiles += 1;
                let key = StackKey {
                    z: PLACEHOLDER_Z,
                    image: images.stack_rank(pc.image_id),
                    kind: DrawKind::Placeholder,
                    placement: 0,
                };
                self.sorted.push((key, draw));
            }
        }
    }
}

/// kitty's destination rectangle: a requested axis spans its cells, an
/// automatic one keeps the source's aspect ratio against the other, and
/// with neither requested the source draws at its own pixel size.
/// `sw` and `sh` are nonzero ([`clip_source`]).
fn dest_size_px(requested: RequestedExtent, sw: u32, sh: u32, cw: f32, ch: f32) -> [f32; 2] {
    let aspect = sw as f32 / sh as f32;
    match (requested.cols, requested.rows) {
        (0, 0) => [sw as f32, sh as f32],
        (0, rows) => {
            let h = f32::from(rows) * ch;
            [h * aspect, h]
        }
        (cols, 0) => {
            let w = f32::from(cols) * cw;
            [w, w / aspect]
        }
        (cols, rows) => [f32::from(cols) * cw, f32::from(rows) * ch],
    }
}

/// The anchor is 1-based (`anchor_row - 1 + viewport`, `anchor_col - 1`),
/// shifted by `viewport` so a placement tracks its text while browsing
/// scrollback. `None` for a degenerate placement.
fn draw_for_placement(
    p: &ClientPlacement,
    width: u32,
    height: u32,
    cw: f32,
    ch: f32,
    viewport: u32,
) -> Option<ImageDraw> {
    let SourceRect {
        x: sx,
        y: sy,
        width: sw,
        height: sh,
    } = clip_source(width, height, p.source)?;
    let cols = if p.cols == 0 {
        ((sw + (cw.max(1.0) as u32).saturating_sub(1)) / cw.max(1.0) as u32).max(1) as u16
    } else {
        p.cols
    };
    let rows = if p.rows == 0 {
        ((sh + (ch.max(1.0) as u32).saturating_sub(1)) / ch.max(1.0) as u32).max(1) as u16
    } else {
        p.rows
    };
    if cols == 0 || rows == 0 {
        return None;
    }
    // Signed: a scrollback-anchored placement (`anchor_row <= 0`) has a
    // negative origin until the viewport lifts it into view, and the
    // frontend clips the rest.
    let origin_row = i64::from(p.anchor_row) - 1 + i64::from(viewport);
    let origin = [
        f32::from(p.anchor_col.saturating_sub(1)) * cw,
        origin_row as f32 * ch,
    ];
    let size = if let Some(requested) = p.requested {
        dest_size_px(requested, sw, sh, cw, ch)
    } else {
        // A minor-0 daemon sends no request: a box matching `ceil(px / cell)`
        // draws at native pixels to avoid non-integer resampling seams on
        // high-contrast edges, any other scales to the box.
        let cw_px = cw.max(1.0) as u32;
        let ch_px = ch.max(1.0) as u32;
        let width_px = if u32::from(cols) == sw.div_ceil(cw_px) {
            sw as f32
        } else {
            f32::from(cols) * cw
        };
        let height_px = if u32::from(rows) == sh.div_ceil(ch_px) {
            sh as f32
        } else {
            f32::from(rows) * ch
        };
        [width_px, height_px]
    };
    Some(ImageDraw {
        image_id: p.image_id,
        image_size: [width, height],
        src: [sx as f32, sy as f32, sw as f32, sh as f32],
        dst: [origin[0], origin[1], size[0], size[1]],
    })
}

/// The producer's `c=`/`r=` when sent (presenterm), else the natural
/// `ceil(image_px / cell_px)`: yazi omits them and paints exactly that
/// screen, and a zero extent would skip every tile.
fn placeholder_grid_extent(
    vp_cols: u16,
    vp_rows: u16,
    img_w: u32,
    img_h: u32,
    cw: f32,
    ch: f32,
) -> (u16, u16) {
    let natural = |px: u32, cell: f32| {
        u16::try_from(px.div_ceil((cell.max(1.0)) as u32))
            .unwrap_or(u16::MAX)
            .max(1)
    };
    let cols = if vp_cols > 0 {
        vp_cols
    } else {
        natural(img_w, cw)
    };
    let rows = if vp_rows > 0 {
        vp_rows
    } else {
        natural(img_h, ch)
    };
    (cols, rows)
}

#[derive(Clone, Copy)]
struct PlaceholderTile {
    /// 0-based, unlike `ClientPlacement`'s anchor.
    screen_row: u16,
    screen_col: u16,
    /// 0-based.
    tile_row: u32,
    tile_col: u32,
    total_cols: u16,
    total_rows: u16,
}

/// kitty's `grman_put_cell_image`: the image is fit into the whole
/// `total_cols` x `total_rows` box with its aspect ratio kept and
/// centered, and each cell shows its share of that box, or nothing when
/// it falls in the margin.
fn placeholder_tile_draw(
    image_id: ImageId,
    width: u32,
    height: u32,
    cw: f32,
    ch: f32,
    at: PlaceholderTile,
) -> Option<ImageDraw> {
    let cols = u32::from(at.total_cols);
    let rows = u32::from(at.total_rows);
    if cols == 0 || rows == 0 || at.tile_col >= cols || at.tile_row >= rows {
        return None;
    }
    if width == 0 || height == 0 {
        return None;
    }
    let (img_w, img_h) = (width as f32, height as f32);
    let (box_w, box_h) = (cols as f32 * cw, rows as f32 * ch);
    let scale = (box_w / img_w).min(box_h / img_h);
    let (fit_w, fit_h) = (img_w * scale, img_h * scale);
    let (fit_x, fit_y) = ((box_w - fit_w) / 2.0, (box_h - fit_h) / 2.0);
    // Both edges of a cell come from its index, so neighboring cells
    // share the exact float boundary and no seam opens between them.
    let cell_x = at.tile_col as f32 * cw;
    let cell_y = at.tile_row as f32 * ch;
    let x0 = cell_x.max(fit_x);
    let x1 = ((at.tile_col + 1) as f32 * cw).min(fit_x + fit_w);
    let y0 = cell_y.max(fit_y);
    let y1 = ((at.tile_row + 1) as f32 * ch).min(fit_y + fit_h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let src_x = |x: f32| (x - fit_x) / fit_w * img_w;
    let src_y = |y: f32| (y - fit_y) / fit_h * img_h;
    let (u0, u1) = (src_x(x0), src_x(x1));
    let (v0, v1) = (src_y(y0), src_y(y1));
    Some(ImageDraw {
        image_id,
        image_size: [width, height],
        src: [u0, v0, u1 - u0, v1 - v0],
        dst: [
            f32::from(at.screen_col).mul_add(cw, x0 - cell_x),
            f32::from(at.screen_row).mul_add(ch, y0 - cell_y),
            x1 - x0,
            y1 - y0,
        ],
    })
}

#[cfg(test)]
mod tests;

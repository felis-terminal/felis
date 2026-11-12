#![allow(clippy::float_cmp)]

use std::collections::HashMap;
#[cfg(not(windows))]
use std::num::NonZeroU32;

#[cfg(not(windows))]
use crate::glyphs::{GlyphIndex, GridWalker};
use crate::{
    appearance::{cell_contains_bidi_override, dim_toward_bg},
    overlay::SearchHitSpan,
    palette::{BIDI_MARKER_BG, BIDI_MARKER_FG, SELECTION_BG, Theme, resolve_fg},
};
use felis_grid::{AttrFlags, Color, Grid};
#[cfg(not(windows))]
use felis_shaping::{FontStack, FontStyle, Shaper};
use felis_vt::Parser;
use proptest::prelude::{Strategy, any, prop};

use super::*;

mod background_and_cursor;
mod decorations;
#[cfg(not(windows))]
mod fitted_clusters;
mod glyph_colors;
mod links;
mod osc66_sizing;
mod preedit_overlay;
mod scrollback_viewport;
mod search_and_link_preview;
mod selection_and_bidi;
mod shaped_quads;
mod wide_chars;

/// Whitespace returns `None`, as a blank glyph has no slot.
struct MockAtlas {
    map: HashMap<char, GlyphSlot>,
    gid_map: HashMap<felis_shaping::GlyphId, GlyphSlot>,
    fitted_map: HashMap<(felis_shaping::GlyphId, u16), GlyphSlot>,
    clusters: HashMap<String, Vec<ClusterGlyph>>,
}

impl MockAtlas {
    fn full(slot: GlyphSlot) -> Self {
        let mut map = HashMap::new();
        for b in 0x21u8..=0x7Eu8 {
            map.insert(b as char, slot);
        }
        Self {
            map,
            gid_map: HashMap::new(),
            fitted_map: HashMap::new(),
            clusters: HashMap::new(),
        }
    }
}

impl AtlasView for MockAtlas {
    fn slot(&self, glyph: char, _height: u32, _sizing: SizingKey) -> Option<GlyphSlot> {
        self.map.get(&glyph).copied()
    }

    fn glyph_id_slot(
        &self,
        glyph_id: felis_shaping::GlyphId,
        _cell_height_px: u32,
        _sizing: SizingKey,
    ) -> Option<GlyphSlot> {
        self.gid_map.get(&glyph_id).copied()
    }

    fn fitted_glyph_id_slot(
        &self,
        glyph_id: felis_shaping::GlyphId,
        px: u16,
        _cell_height_px: u32,
        _sizing: SizingKey,
    ) -> Option<GlyphSlot> {
        self.fitted_map.get(&(glyph_id, px)).copied()
    }

    fn overlay_cluster(&self, text: &str) -> Option<&[ClusterGlyph]> {
        self.clusters.get(text).map(Vec::as_slice)
    }

    fn sheet_side(&self, _sheet: AtlasKind) -> u32 {
        80
    }
}

fn metrics() -> CellMetrics {
    CellMetrics::unscaled(8, 16, 12)
}

fn slot() -> GlyphSlot {
    GlyphSlot {
        atlas_px: [0, 0],
        size_px: [6, 10],
        offset_px: [1, 10],
        is_color: false,
        fills_cell: false,
    }
}

fn drive(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
    let mut g = Grid::new(rows, cols);
    let mut p = Parser::new();
    p.advance(&mut g, bytes);
    g
}

fn build_instances<A: AtlasView>(
    screen: &ScreenBuffer,
    metrics: CellMetrics,
    theme: &Theme,
    atlas: &A,
) -> RowPaint {
    build_instances_with_reverse(screen, metrics, theme, atlas, false)
}

fn build_instances_with_reverse<A: AtlasView>(
    screen: &ScreenBuffer,
    metrics: CellMetrics,
    theme: &Theme,
    atlas: &A,
    screen_reverse: bool,
) -> RowPaint {
    let mut buffers = RowPaint::default();
    CellPainter::new(
        screen,
        metrics,
        &ResolvedTheme::new(theme),
        atlas,
        &ShapeFrame::empty(),
    )
    .without_compaction()
    .with_cursor_visible(true)
    .with_viewport(0, u32::from(screen.rows()))
    .with_reverse_video(screen_reverse)
    .extend_screen(&mut buffers);
    buffers
}

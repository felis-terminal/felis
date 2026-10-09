//! Per-frame instance buffers built from the screen: the bg and fg
//! passes of `docs/explanation/rendering/pipeline.md`. Coordinates are
//! `[f32; 2]` physical pixels; the vertex shader divides by viewport
//! size. The layout is committed here so shader and Rust cannot drift.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};
use felis_grid::{
    AttrFlags, Cell, Color, CursorStyle, Grapheme, HAlign, ScreenBuffer, Sizing, UnderlineStyle,
    VAlign, bidi, char_cell_width,
};
use felis_shaping::{CellMetrics, FontStyle, SizingKey};

use crate::{
    SelectionRange,
    palette::{
        BIDI_MARKER_BG, BIDI_MARKER_FG, CONFIRM_BAR_BG, LINK_UNDERLINE, PREEDIT_BG, ResolvedTheme,
        SCROLLBAR_THUMB, SCROLLBAR_TRACK, SEARCH_BAR_BG, SEARCH_CURRENT_BG, SEARCH_MATCH_BG,
        SELECTION_BG, resolve_bg, resolve_fg,
    },
};

pub(crate) const CURSOR_MARKER_PX: f32 = 2.0;

/// One pixel less than the cursor marker so a cursor underline on a
/// link cell still visually wins.
const LINK_UNDERLINE_PX: f32 = 1.0;

const PREEDIT_UNDERLINE_PX: f32 = 2.0;

/// The Mozc / fcitx / iBus convention: a thicker bar under the segment
/// being edited.
const PREEDIT_ACTIVE_UNDERLINE_PX: f32 = 4.0;

const SCROLLBAR_LANE_PX: f32 = 6.0;

/// Uncommitted IME composition text painted at 0-based `anchor` `(row, col)`.
///
/// Stops at the row's right edge without wrapping. `cursor` is the active-segment
/// byte range reported by winit.
#[derive(Debug, Clone)]
pub struct PreeditOverlay {
    pub anchor: (u16, u16),
    pub text: String,
    pub cursor: Option<(usize, usize)>,
}

/// Scrollback-search overlay with hit highlights and a bottom-row status bar.
///
/// The bar occludes the bottom row's cells. Re-derived on redraw and painted
/// until [`crate::Renderer::set_search_overlay`] passes `None`.
#[derive(Debug, Clone, Default)]
pub struct SearchOverlay {
    /// Empty suppresses the bar; highlights still paint.
    pub label: String,
    /// One entry per `col_span` of a `SearchToClientMsg::Match`, already mapped
    /// through the viewport by the caller. The renderer clips
    /// out-of-range rows / cols so a viewport edit racing the paint
    /// cannot panic.
    pub visible_hits: Vec<SearchHitSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchHitSpan {
    /// Composed-view row (0 = top of visible screen; `rows - 1` = bottom).
    pub row: u16,
    /// Inclusive column range start.
    pub col_start: u16,
    /// Exclusive column range end.
    pub col_end: u16,
    /// The renderer does not enforce uniqueness.
    pub is_current: bool,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
pub struct BgInstance {
    /// Top-left of the cell in physical pixels.
    pub origin_px: [f32; 2],
    /// Cell width / height in physical pixels.
    pub size_px: [f32; 2],
    /// Background color in linear RGBA.
    pub color: [f32; 4],
}

const _: () = assert!(size_of::<BgInstance>() == 32);

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
pub struct FgInstance {
    /// Top-left of the glyph quad in physical pixels.
    pub origin_px: [f32; 2],
    /// Glyph quad width / height in physical pixels.
    pub size_px: [f32; 2],
    /// Atlas-space uv min `[u, v]` in [0, 1].
    pub uv_min: [f32; 2],
    /// Atlas-space uv max `[u, v]` in [0, 1].
    pub uv_max: [f32; 2],
    /// Foreground (glyph) color in linear RGBA.
    pub color: [f32; 4],
    /// `1.0` for an RGBA color bitmap from the color atlas, `0.0` for a
    /// coverage mask tinted by `color`. A float so it rides
    /// `VertexFormat::Float32` like the rest of the instance.
    pub is_color: f32,
    /// Pads the stride to the WGSL struct layout.
    #[expect(
        clippy::pub_underscore_fields,
        reason = "explicit WGSL alignment padding; never read, but Pod needs it pub"
    )]
    pub _pad: [f32; 3],
}

const _: () = assert!(size_of::<FgInstance>() == 64);

/// Two passes share this layout (z<0 below the cell layers, z≥0
/// above) for the `docs/reference/protocols/kitty-graphics.md`
/// z-ordering.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
pub struct ImgInstance {
    /// Top-left of the image quad in physical pixels.
    pub origin_px: [f32; 2],
    /// Image quad width / height in physical pixels.
    pub size_px: [f32; 2],
    /// Atlas-space uv min `[u, v]` in [0, 1].
    pub uv_min: [f32; 2],
    /// Atlas-space uv max `[u, v]` in [0, 1].
    pub uv_max: [f32; 2],
    /// Reserved; the dispatcher pins this to `1.0`.
    pub opacity: f32,
    /// Pads the stride to the WGSL struct layout.
    #[expect(
        clippy::pub_underscore_fields,
        reason = "explicit WGSL alignment padding; the underscore marks it as never read, and Pod needs it pub"
    )]
    pub _pad: [f32; 3],
}

const _: () = assert!(size_of::<ImgInstance>() == 48);

/// The quad spans the decoration band: the line rectangle for the
/// straight kinds, a taller band the sine swings inside for
/// `DECO_CURLY`. `deco_fs` computes coverage from [`Self::kind`].
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
pub struct DecorationInstance {
    /// Top-left of the band quad in physical pixels.
    pub origin_px: [f32; 2],
    /// Band width / height in physical pixels.
    pub size_px: [f32; 2],
    /// Line color in linear RGBA.
    pub color: [f32; 4],
    /// [`DECO_SOLID`] / [`DECO_DOTTED`] / [`DECO_DASHED`] / [`DECO_CURLY`].
    pub kind: f32,
    /// Line thickness in physical pixels.
    pub thickness_px: f32,
    /// Pattern period (dotted / dashed) or sine wavelength (curly), px.
    /// Unused by [`DECO_SOLID`].
    pub period_px: f32,
    /// Pads the stride to the WGSL struct layout.
    #[expect(
        clippy::pub_underscore_fields,
        reason = "explicit WGSL alignment padding; never read, but Pod needs it pub"
    )]
    pub _pad: f32,
}

const _: () = assert!(size_of::<DecorationInstance>().is_multiple_of(16));

/// The kind values must match the `deco_fs` branch cutoffs in
/// `shader.wgsl`.
pub const DECO_SOLID: f32 = 0.0;
/// Dotted underline (`SGR 4:4`).
pub const DECO_DOTTED: f32 = 1.0;
/// Dashed underline (`SGR 4:5`).
pub const DECO_DASHED: f32 = 2.0;
/// Curly / wavy underline (`SGR 4:3`).
pub const DECO_CURLY: f32 = 3.0;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GlyphSlot {
    /// Atlas uv min: top-left in `[0, 1]`.
    pub uv_min: [f32; 2],
    /// Atlas uv max: bottom-right in `[0, 1]`.
    pub uv_max: [f32; 2],
    /// Pixel size of the glyph bitmap (not the cell).
    pub size_px: [u32; 2],
    /// `(left, top)` from the cell's `(0, baseline)` origin, per swash.
    pub offset_px: [i32; 2],
    /// True when the bitmap lives in the RGBA color atlas.
    pub is_color: bool,
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
        glyph_id: felis_shaping::GlyphId,
        cell_height_px: u32,
        sizing: SizingKey,
    ) -> Option<GlyphSlot>;

    /// The slot of a cluster glyph shrunk to `px` to fit its cells;
    /// see [`crate::glyphs::ShapedCell::Cluster`].
    fn fitted_glyph_id_slot(
        &self,
        glyph_id: felis_shaping::GlyphId,
        px: u16,
        cell_height_px: u32,
        sizing: SizingKey,
    ) -> Option<GlyphSlot>;

    /// The shaped glyphs of a multi-scalar overlay cluster, `None` until
    /// the overlay's text was populated.
    fn overlay_cluster(&self, text: &str) -> Option<&[crate::glyphs::ClusterGlyph]>;
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

/// Cell count is governed by the integer `s` (and `w`) alone; `n/d`
/// scales the bitmap, not the cell count (Kitty: "the fractional scale
/// does not affect the number of cells the text occupies"). Matches
/// the screen's `dispatch_osc_66` block stamping.
#[must_use]
fn sizing_scale_for_layout(sizing: Sizing) -> f32 {
    f32::from(sizing.scale().max(1))
}

/// Delegated to [`SizingKey::effective_scale`] so glyphs are placed at
/// exactly the scale the rasterizer sized their bitmaps with.
#[must_use]
fn sizing_glyph_scale(sizing: Sizing) -> f32 {
    sizing_key_from(sizing).effective_scale()
}

#[derive(Debug, Default, PartialEq)]
pub struct InstanceBuffers {
    pub bg: Vec<BgInstance>,
    pub fg: Vec<FgInstance>,
    /// Drawn after the glyph pass so strikethrough sits over the ink.
    pub deco: Vec<DecorationInstance>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreeditSpan {
    pub row: u16,
    pub col_start: u16,
    /// One past the last column the composition covers.
    pub col_end: u16,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CursorPaint {
    /// The row and first column of the character under the cursor.
    pub(crate) cell: Option<(u16, u16)>,
    last_col: u16,
    style: CursorStyle,
}

impl CursorPaint {
    fn covers(&self, row: u16, col: u16) -> bool {
        matches!(self.cell, Some((r, first)) if r == row && (first..=self.last_col).contains(&col))
    }
}

struct CellRef<'a> {
    row: u16,
    col: u16,
    cell: &'a Cell,
    attrs: &'a felis_grid::Attributes,
}

struct CellColors {
    fg: [f32; 4],
    bg: [f32; 4],
    /// For `Underline` / `Bar`; `Block` folds into `fg` / `bg`.
    cursor_marker: Option<BgInstance>,
    is_bidi: bool,
    faint: bool,
}

pub struct CellPainter<'a, A: AtlasView> {
    screen: &'a ScreenBuffer,
    theme: &'a ResolvedTheme,
    atlas: &'a A,
    shape_frame: &'a crate::glyphs::ShapeFrame,
    metrics: CellMetrics,
    cw: f32,
    ch: f32,
    ascent: f32,
    cursor_visible: bool,
    /// Resolved at the start of the walk; meaningless before.
    pub(crate) cursor: CursorPaint,
    selection: Option<SelectionRange>,
    viewport: u32,
    viewport_max: u32,
    reserved_bottom_rows: u16,
    preedit: Option<PreeditSpan>,
    reverse_video: bool,
}

impl<'a, A: AtlasView> CellPainter<'a, A> {
    /// Overlays default to absent and layer on through `with_*`.
    ///
    /// The provided `shape_frame` determines whether cells render via glyph IDs, clusters,
    /// or the per-char fallback.
    #[must_use]
    pub fn new(
        screen: &'a ScreenBuffer,
        metrics: CellMetrics,
        theme: &'a ResolvedTheme,
        atlas: &'a A,
        shape_frame: &'a crate::glyphs::ShapeFrame,
    ) -> Self {
        Self {
            screen,
            theme,
            atlas,
            shape_frame,
            metrics,
            cw: metrics.width as f32,
            ch: metrics.height as f32,
            ascent: f32::from(u16::try_from(metrics.ascent).unwrap_or(u16::MAX)),
            cursor_visible: false,
            cursor: CursorPaint::default(),
            selection: None,
            viewport: 0,
            viewport_max: 0,
            reserved_bottom_rows: 0,
            preedit: None,
            reverse_video: false,
        }
    }

    /// `?5` DECSCNM as the daemon reports it, xor'd against each cell's
    /// SGR 7.
    #[must_use]
    pub const fn with_reverse_video(mut self, on: bool) -> Self {
        self.reverse_video = on;
        self
    }

    /// True only while focused and the blink clock is in its visible
    /// phase; both hide the cursor identically, so they arrive folded.
    #[must_use]
    pub const fn with_cursor_visible(mut self, visible: bool) -> Self {
        self.cursor_visible = visible;
        self
    }

    #[must_use]
    pub const fn with_selection(mut self, selection: Option<SelectionRange>) -> Self {
        self.selection = selection;
        self
    }

    /// `viewport` rows above the live bottom out of `viewport_max`; `0`
    /// is the live bottom.
    #[must_use]
    pub const fn with_viewport(mut self, viewport: u32, viewport_max: u32) -> Self {
        self.viewport = viewport;
        self.viewport_max = viewport_max;
        self
    }

    /// Rows trimmed off the bottom so bottom-anchored chrome (the search
    /// bar) owns them without the cells doubling through.
    #[must_use]
    pub const fn with_reserved_bottom_rows(mut self, rows: u16) -> Self {
        self.reserved_bottom_rows = rows;
        self
    }

    /// The span (from [`preedit_covered_cols`]) whose cells are skipped
    /// so the underlying glyph does not draw through the opaque overlay
    /// in the later fg pass.
    #[must_use]
    pub const fn with_preedit(mut self, span: Option<PreeditSpan>) -> Self {
        self.preedit = span;
        self
    }

    /// Appends into caller-owned scratch (kept across frames so the
    /// steady state allocates nothing); callers clear first for a fresh
    /// frame. An atlas miss skips only the fg quad; the bg still lands.
    pub fn extend_instances(self, out: &mut InstanceBuffers) {
        let painter = self.resolved();
        let cells = usize::from(painter.screen.rows()) * usize::from(painter.screen.cols());
        out.bg.reserve(cells);
        out.fg.reserve(cells);
        for r in 0..painter.screen.rows() {
            painter.extend_row(out, r);
        }
        painter.push_scrollbar_lane(out);
    }

    /// Fixes the cursor paint, which every row reads.
    #[must_use]
    pub(crate) fn resolved(mut self) -> Self {
        self.cursor = self.resolve_cursor();
        self
    }

    /// Everything a row emits depends on that row's cells and the
    /// painter's frame-wide inputs alone, which is what lets
    /// [`crate::row_cache::RowCache`] repaint a row in isolation.
    pub(crate) fn extend_row(&self, out: &mut InstanceBuffers, r: u16) {
        // Cell instances must not land under the bottom chrome: a glyph
        // rides the fg pass and would draw on top of the bar. Capping the
        // walk is the only fix; there is no fg occlusion across passes.
        if r >= self.screen.rows().saturating_sub(self.reserved_bottom_rows) {
            return;
        }
        for c in 0..self.screen.cols() {
            // Same two-pass occlusion limit: a cell emitted under the
            // composition overlay draws through it in the fg pass.
            let glyph_under_preedit = match self.preedit {
                Some(span) if r == span.row => {
                    if c >= span.col_start && c < span.col_end {
                        continue;
                    }
                    // The lead of a wide character half under the overlay
                    // keeps its background but not its glyph, which inks
                    // across both halves.
                    let (first, last) = self.screen.char_span(r, c);
                    last >= span.col_start && first < span.col_end
                }
                _ => false,
            };
            // Straight off the shadow's screen: the shadow has no
            // scrollback ring, so when viewport > 0 the daemon ships
            // composed RowDeltas against composed-view rows. Composing
            // here (`cell_at_viewport`) would fall back to live cells.
            let Some(cell) = self.screen.cell(r, c) else {
                continue;
            };
            let at = CellRef {
                row: r,
                col: c,
                cell,
                attrs: self.screen.style(cell.style),
            };
            let fg_color = self.push_cell_chrome(out, &at);
            // CONCEAL (SGR 8). SECURITY: this must gate every fg push.
            // The bg / cursor / selection / decoration instances above
            // stay (a concealed cell reads as a space).
            if at.attrs.flags.contains(AttrFlags::CONCEAL) || glyph_under_preedit {
                continue;
            }
            // Shape-frame lookup precedes char-path decoding so a
            // `Trailing` cell skips its fg entirely.
            let shaped = self.shape_frame.cell_at(r, c);
            if matches!(shaped, crate::glyphs::ShapedCell::Trailing) {
                continue;
            }
            if let crate::glyphs::ShapedCell::Cluster { start, len, fit_px } = shaped {
                self.push_cluster_cell(out, &at, fg_color, (start, len), fit_px);
                continue;
            }
            self.push_char_cell(out, &at, fg_color, shaped);
        }
    }

    fn origin(&self, at: &CellRef<'_>) -> [f32; 2] {
        [f32::from(at.col) * self.cw, f32::from(at.row) * self.ch]
    }

    /// `pending_wrap` can bump `col` past the right edge for one print;
    /// the in-bounds check skips that. Focus loss hides the cursor, and
    /// so does viewport > 0 (Kitty / Alacritty's cursor-in-scrollback
    /// behavior): the live row has scrolled off the visible window.
    fn resolve_cursor(&self) -> CursorPaint {
        let cursor = self.screen.cursor();
        let in_bounds = cursor.row < self.screen.rows() && cursor.col < self.screen.cols();
        let (first, last) = self.screen.char_span(cursor.row, cursor.col);
        CursorPaint {
            cell: (cursor.visible && in_bounds && self.cursor_visible && self.viewport == 0)
                .then_some((cursor.row, first)),
            last_col: last,
            style: self.screen.cursor_style(),
        }
    }

    /// Every path that needs a cell's foreground goes through here, the
    /// chrome pass and the per-cell slicing of a shaped quad alike, so
    /// the precedence chain has one spelling.
    fn resolve_cell_colors(&self, at: &CellRef<'_>) -> CellColors {
        // A wide character is painted as one: its Spacer takes the
        // lead's bidi warning, and the cursor or a selection on either
        // half covers both.
        let is_bidi = match at.cell.grapheme {
            Grapheme::Spacer if at.col > 0 => self
                .screen
                .cell(at.row, at.col - 1)
                .is_some_and(|lead| cell_contains_bidi_override(lead.grapheme, self.screen)),
            g => cell_contains_bidi_override(g, self.screen),
        };
        let (mut fg, mut bg) = resolve_pair(
            at.attrs.fg,
            at.attrs.bg,
            self.theme,
            at.attrs.flags,
            self.reverse_video,
        );
        // Pushed after the cell's bg quad so it draws over the bg but
        // under the glyph.
        let mut cursor_marker = if self.cursor.covers(at.row, at.col) {
            self.apply_cursor_style(at, &mut fg, &mut bg)
        } else {
            None
        };
        // After the cursor swap so the security signal wins on the
        // cursor cell too (CVE-2021-42574).
        if is_bidi {
            bg = BIDI_MARKER_BG;
            fg = BIDI_MARKER_FG;
            // The marker would obstruct the warning.
            cursor_marker = None;
        } else if self.selection_covers(at) {
            bg = SELECTION_BG;
        }
        // After cursor / bidi / selection so it dims the effective fg,
        // and never the bidi warning.
        let faint = at.attrs.flags.contains(AttrFlags::FAINT) && !is_bidi;
        if faint {
            fg = dim_toward_bg(fg, bg);
        }
        CellColors {
            fg,
            bg,
            cursor_marker,
            is_bidi,
            faint,
        }
    }

    fn selection_covers(&self, at: &CellRef<'_>) -> bool {
        if self.selection.is_none() {
            return false;
        }
        let (first, last) = self.screen.char_span(at.row, at.col);
        selection_contains(self.selection, at.row, first)
            || selection_contains(self.selection, at.row, last)
    }

    fn push_cell_chrome(&self, out: &mut InstanceBuffers, at: &CellRef<'_>) -> [f32; 4] {
        let colors = self.resolve_cell_colors(at);
        let origin = self.origin(at);
        out.bg.push(BgInstance {
            origin_px: origin,
            size_px: [self.cw, self.ch],
            color: colors.bg,
        });
        // Before the cursor marker so a cursor underline on a link cell
        // wins; skipped on bidi cells so the warning stays unobstructed.
        if at.cell.link.is_some() && !colors.is_bidi {
            out.bg.push(BgInstance {
                origin_px: [origin[0], origin[1] + self.ch - LINK_UNDERLINE_PX],
                size_px: [self.cw, LINK_UNDERLINE_PX],
                color: LINK_UNDERLINE,
            });
        }
        if let Some(marker) = colors.cursor_marker {
            out.bg.push(marker);
        }
        // The decoration pass draws after the glyph pass, so precedence
        // against the bg-pass overlays is enforced by suppression, not
        // z-order: a bidi cell drops every decoration, and the SGR
        // underline yields its slot to the link underline and the cursor
        // marker.
        if !colors.is_bidi {
            let suppress_underline = at.cell.link.is_some() || self.cursor.covers(at.row, at.col);
            self.push_cell_decorations(&mut out.deco, at, &colors, suppress_underline);
        }
        colors.fg
    }

    fn apply_cursor_style(
        &self,
        at: &CellRef<'_>,
        fg_color: &mut [f32; 4],
        bg_color: &mut [f32; 4],
    ) -> Option<BgInstance> {
        let origin = self.origin(at);
        let original_bg = *bg_color;
        let cursor_color = self.theme.cursor.unwrap_or(*fg_color);
        match self.cursor.style {
            CursorStyle::Block => {
                // With `theme.cursor`: bg = cursor color, glyph = original
                // cell bg for contrast. Without: reverse-video swap.
                if self.theme.cursor.is_some() {
                    *bg_color = cursor_color;
                    *fg_color = original_bg;
                } else {
                    std::mem::swap(fg_color, bg_color);
                }
                None
            }
            CursorStyle::Underline => Some(BgInstance {
                origin_px: [origin[0], origin[1] + self.ch - CURSOR_MARKER_PX],
                size_px: [self.cw, CURSOR_MARKER_PX],
                color: cursor_color,
            }),
            CursorStyle::Bar if self.cursor.cell.is_some_and(|(_, first)| first != at.col) => None,
            CursorStyle::Bar => Some(BgInstance {
                origin_px: origin,
                size_px: [CURSOR_MARKER_PX, self.ch],
                color: cursor_color,
            }),
        }
    }

    /// One instance per shaped glyph at the cell origin plus pen advance
    /// and GPOS offset; a single-glyph cluster places identically to the
    /// equivalent `Char` cell.
    fn push_cluster_cell(
        &self,
        out: &mut InstanceBuffers,
        at: &CellRef<'_>,
        fg_color: [f32; 4],
        (start, len): (u32, u16),
        fit_px: u16,
    ) {
        // `shape_clusters` primed each glyph with the sized key, so
        // emission rebuilds the same key and scales pen / GPOS by the
        // same `glyph_scale`.
        let sizing = self
            .screen
            .cell_sizing(at.row, at.col)
            .copied()
            .unwrap_or_default();
        let base_sizing = sizing_key_from(sizing).with_style(font_style_of(at.attrs.flags));
        let glyph_scale = sizing_glyph_scale(sizing);
        let scaled_ascent = self.ascent * glyph_scale;
        let pen = ClusterPen {
            glyphs: self.shape_frame.cluster_slice(start, len),
            base_sizing,
            fit_px,
            glyph_scale,
            origin: [0.0, scaled_ascent],
            color: fg_color,
        };
        let block_shift = self.cluster_block_shift(at, pen, sizing);
        let origin = self.origin(at);
        push_cluster_glyphs(
            self.atlas,
            self.metrics.height,
            ClusterPen {
                origin: [
                    origin[0] + block_shift[0],
                    origin[1] + block_shift[1] + scaled_ascent,
                ],
                ..pen
            },
            &mut out.fg,
        );
    }

    /// Top/Left needs no shift: the early-out keeps default-sized output
    /// byte-identical rather than relying on the ink formula's float
    /// reduction. The other alignments align the union ink box, so a mark
    /// past its base stays inside the block. Top/Left centres a fitted
    /// cluster, as `fitted_bitmap` centres a fitted char.
    fn cluster_block_shift(
        &self,
        at: &CellRef<'_>,
        pen: ClusterPen<'_>,
        sizing: Sizing,
    ) -> [f32; 2] {
        let fitted = pen.fit_px > 0;
        if !fitted
            && matches!(sizing.halign(), HAlign::Left)
            && matches!(sizing.valign(), VAlign::Top)
        {
            return [0.0, 0.0];
        }
        let Some([ink_x0, ink_y0, ink_x1, ink_y1]) =
            cluster_ink_box(self.atlas, self.metrics.height, pen)
        else {
            return [0.0, 0.0];
        };
        let ink_w = ink_x1 - ink_x0;
        let ink_h = ink_y1 - ink_y0;
        let layout_scale = sizing_scale_for_layout(sizing);
        let block_w =
            self.cw * f32::from(cluster_block_cols(self.screen, at.row, at.col)) * layout_scale;
        let block_h = self.ch * layout_scale;
        let centre_x = (block_w - ink_w) / 2.0 - ink_x0;
        let centre_y = (block_h - ink_h) / 2.0 - ink_y0;
        let shift_x = match sizing.halign() {
            HAlign::Left if fitted => centre_x,
            HAlign::Left => 0.0,
            HAlign::Right => block_w - ink_x1,
            HAlign::Center => centre_x,
        };
        let shift_y = match sizing.valign() {
            VAlign::Top if fitted => centre_y,
            VAlign::Top => 0.0,
            VAlign::Bottom => block_h - ink_y1,
            VAlign::Center => centre_y,
        };
        [shift_x, shift_y]
    }

    fn push_char_cell(
        &self,
        out: &mut InstanceBuffers,
        at: &CellRef<'_>,
        fg_color: [f32; 4],
        shaped: crate::glyphs::ShapedCell,
    ) {
        let shape_primary = match shaped {
            crate::glyphs::ShapedCell::Primary {
                glyph_id,
                span_cols,
            } => Some((glyph_id, span_cols)),
            _ => None,
        };
        let glyph = match &at.cell.grapheme {
            // A spacer is the right half of a wide glyph its neighbor drew.
            Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => return,
            // The image-tile pass paints placeholders; a glyph here would
            // render tofu under the image.
            Grapheme::Char(c) if *c == felis_protocol::kitty_graphics::placeholder::PLACEHOLDER => {
                return;
            }
            Grapheme::Ascii(b) => *b as char,
            Grapheme::Char(c) => *c,
            // Reached only when the shape frame carries no `Cluster` entry:
            // shaping degenerately produced no glyphs (`shape_clusters` then
            // primed the base char at the cell's sizing) or the handle no
            // longer resolves (docs/explanation/data-model/grid-and-cells.md
            // "Cluster interning").
            Grapheme::Cluster(id) => match crate::glyphs::cluster_base_char(self.screen, *id) {
                Some(c) if c == felis_protocol::kitty_graphics::placeholder::PLACEHOLDER => {
                    return;
                }
                Some(c) => c,
                None => return,
            },
        };
        // REQ-603: the screen reserves a `per_char_w × scale × scale`
        // block per sized char via `Grapheme::SizedSpacer` continuation
        // cells, which short-circuit above, so only the primary's glyph
        // is emitted (rasterized once at scale, spilling into the spanned
        // slots) while each spacer still paints its bg quad.
        let sizing = self
            .screen
            .cell_sizing(at.row, at.col)
            .copied()
            .unwrap_or_default();
        // `populate` / `apply_shaped_run` key the styled-face slot the
        // same way.
        let sizing_key = sizing_key_from(sizing).with_style(font_style_of(at.attrs.flags));
        let slot_lookup = match shape_primary {
            Some((glyph_id, _span_cols)) => {
                self.atlas
                    .glyph_id_slot(glyph_id, self.metrics.height, sizing_key)
            }
            None => self.atlas.slot(glyph, self.metrics.height, sizing_key),
        };
        let Some(slot) = slot_lookup else {
            return;
        };
        // `layout_scale` (= `s`) matches the screen's block stamping in
        // `dispatch_osc_66`; `glyph_scale` (= `s × n/d`, or `s`) is what
        // swash rasterized against and scales the baseline.
        let layout_scale = sizing_scale_for_layout(sizing);
        let glyph_scale = sizing_glyph_scale(sizing);
        let scaled_ascent = self.ascent * glyph_scale;
        let bitmap_w = slot.size_px[0] as f32;
        let bitmap_h = slot.size_px[1] as f32;
        // `w == 0` falls back to the char's natural cell width.
        let override_w = if sizing.cell_width() > 0 {
            f32::from(sizing.cell_width())
        } else {
            f32::from(char_cell_width(glyph).max(1))
        };
        let block_w = self.cw * override_w * layout_scale;
        let block_h = self.ch * layout_scale;
        // `Top` / `Left` are the spec defaults and must reproduce the
        // baseline + left-bearing math exactly.
        let h_offset = match sizing.halign() {
            HAlign::Left => slot.offset_px[0] as f32,
            HAlign::Right => block_w - bitmap_w,
            HAlign::Center => (block_w - bitmap_w) / 2.0,
        };
        let v_offset = match sizing.valign() {
            VAlign::Top => scaled_ascent - slot.offset_px[1] as f32,
            VAlign::Bottom => block_h - bitmap_h,
            VAlign::Center => (block_h - bitmap_h) / 2.0,
        };
        let origin = self.origin(at);
        let draw_origin = [origin[0] + h_offset, origin[1] + v_offset];
        let quad = FgInstance {
            origin_px: draw_origin,
            size_px: [slot.size_px[0] as f32, slot.size_px[1] as f32],
            uv_min: slot.uv_min,
            uv_max: slot.uv_max,
            is_color: if slot.is_color { 1.0 } else { 0.0 },
            _pad: [0.0; 3],
            color: fg_color,
        };
        // Shape runs do not split at color changes (`next_shape_run`),
        // so a glyph inking across cells is sliced per cell with each
        // strip's own color (kitty's per-cell sprite behavior). Color
        // bitmaps skip this: the shader ignores their tint.
        if shape_primary.is_some() && !slot.is_color {
            self.push_sliced_by_cell(out, quad, at);
        } else {
            out.fg.push(quad);
        }
    }

    /// Absent at the live bottom. Gated on `viewport_max > rows` as
    /// defense-in-depth: the daemon should already have clamped.
    pub(crate) fn push_scrollbar_lane(&self, out: &mut InstanceBuffers) {
        let rows_u32 = u32::from(self.screen.rows());
        if self.viewport > 0 && self.viewport_max > rows_u32 {
            let max_scroll = (self.viewport_max - rows_u32) as f32;
            let total_w = f32::from(self.screen.cols()) * self.cw;
            let total_h = f32::from(self.screen.rows()) * self.ch;
            let lane_x = total_w - SCROLLBAR_LANE_PX;
            // Track first so the thumb wins at overlap.
            out.bg.push(BgInstance {
                origin_px: [lane_x, 0.0],
                size_px: [SCROLLBAR_LANE_PX, total_h],
                color: SCROLLBAR_TRACK,
            });
            // Floored at SCROLLBAR_LANE_PX so a long scrollback still
            // leaves a grabbable indicator.
            let thumb_h_raw = total_h * (rows_u32 as f32) / (self.viewport_max as f32);
            let thumb_h = thumb_h_raw.max(SCROLLBAR_LANE_PX).min(total_h);
            let thumb_top_band = total_h - thumb_h;
            let thumb_top = thumb_top_band * (max_scroll - self.viewport as f32) / max_scroll;
            out.bg.push(BgInstance {
                origin_px: [lane_x, thumb_top],
                size_px: [SCROLLBAR_LANE_PX, thumb_h],
                color: SCROLLBAR_THUMB,
            });
        }
    }

    /// Underlines sit in the descender band, strikethrough at ~x-height,
    /// overline at the top edge. `suppress_underline` drops only the
    /// underline family (the link underline or cursor marker owns that
    /// slot).
    fn push_cell_decorations(
        &self,
        out: &mut Vec<DecorationInstance>,
        at: &CellRef<'_>,
        colors: &CellColors,
        suppress_underline: bool,
    ) {
        let flags = at.attrs.flags;
        let thickness = decoration_thickness(self.ch);
        let origin = self.origin(at);
        let ox = origin[0];
        let oy = origin[1];
        let cw = self.cw;
        let solid = |out: &mut Vec<DecorationInstance>, top: f32, color: [f32; 4]| {
            out.push(DecorationInstance {
                origin_px: [ox, top.clamp(oy, oy + self.ch - thickness)],
                size_px: [cw, thickness],
                color,
                kind: DECO_SOLID,
                thickness_px: thickness,
                period_px: 0.0,
                _pad: 0.0,
            });
        };

        if flags.contains(AttrFlags::UNDERLINE) && !suppress_underline {
            let color = underline_deco_color(
                at.attrs.underline_color,
                colors.fg,
                colors.bg,
                self.theme,
                colors.faint,
            );
            // Clamped so a font whose ascent nearly fills the cell keeps
            // the line on-cell.
            let descent_band = (self.ch - self.ascent).max(thickness);
            let underline_top = (oy + self.ascent + (descent_band * 0.3).round().max(1.0))
                .min(oy + self.ch - thickness)
                .max(oy);
            match at.attrs.underline_style {
                UnderlineStyle::Single => solid(out, underline_top, color),
                UnderlineStyle::Double => {
                    solid(out, underline_top, color);
                    solid(out, thickness.mul_add(-2.0, underline_top), color);
                }
                UnderlineStyle::Dotted => out.push(DecorationInstance {
                    origin_px: [ox, underline_top],
                    size_px: [cw, thickness],
                    color,
                    kind: DECO_DOTTED,
                    thickness_px: thickness,
                    period_px: (thickness * 3.0).max(3.0),
                    _pad: 0.0,
                }),
                UnderlineStyle::Dashed => out.push(DecorationInstance {
                    origin_px: [ox, underline_top],
                    size_px: [cw, thickness],
                    color,
                    kind: DECO_DASHED,
                    thickness_px: thickness,
                    period_px: (cw * 0.75).max(6.0),
                    _pad: 0.0,
                }),
                UnderlineStyle::Curly => {
                    // Taller band so the sine can swing ± its amplitude.
                    let amplitude = thickness;
                    let band_h = amplitude.mul_add(2.0, thickness);
                    let center = thickness.mul_add(0.5, underline_top);
                    let band_top = band_h
                        .mul_add(-0.5, center)
                        .clamp(oy, oy + self.ch - band_h);
                    out.push(DecorationInstance {
                        origin_px: [ox, band_top],
                        size_px: [cw, band_h],
                        color,
                        kind: DECO_CURLY,
                        thickness_px: thickness,
                        period_px: cw.max(thickness * 4.0),
                        _pad: 0.0,
                    });
                }
            }
        }
        if flags.contains(AttrFlags::STRIKETHROUGH) {
            // 0.6 of the ascent reads as through the middle of most glyphs.
            let strike_top = thickness.mul_add(-0.5, oy + (self.ascent * 0.6).round());
            solid(out, strike_top, colors.fg);
        }
        if flags.contains(AttrFlags::OVERLINE) {
            solid(out, oy, colors.fg);
        }
    }

    /// Shape runs do not end at color boundaries (`next_shape_run`), and
    /// one instance carries one tint, so a glyph covering
    /// differently-colored cells is split into per-cell strips (kitty's
    /// per-cell sprites). A uniform-color run emits the original quad.
    fn push_sliced_by_cell(&self, out: &mut InstanceBuffers, quad: FgInstance, at: &CellRef<'_>) {
        let x0 = quad.origin_px[0];
        let x1 = x0 + quad.size_px[0];
        if quad.size_px[0] <= 0.0 {
            out.fg.push(quad);
            return;
        }
        // Overflow past either edge stays attached to the boundary
        // column's slice, so clamping never drops pixels.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let col_first_raw = (x0 / self.cw).floor().max(0.0) as u16;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let col_last_raw = ((x1 / self.cw).ceil() - 1.0).max(0.0) as u16;
        let col_last = col_last_raw.min(self.screen.cols().saturating_sub(1));
        let col_first = col_first_raw.min(col_last);
        if col_first == col_last {
            out.fg.push(quad);
            return;
        }
        let fg_at = |col: u16| -> [f32; 4] {
            if col == at.col {
                return quad.color;
            }
            let Some(cell) = self.screen.cell(at.row, col) else {
                return quad.color;
            };
            self.resolve_cell_colors(&CellRef {
                row: at.row,
                col,
                cell,
                attrs: self.screen.style(cell.style),
            })
            .fg
        };
        // Both sides come from the same `resolve_pair` lookups, so equal
        // colors are equal bit patterns; an epsilon would blur distinct
        // palette entries.
        #[allow(clippy::float_cmp)]
        let uniform = (col_first..=col_last).all(|col| fg_at(col) == quad.color);
        if uniform {
            out.fg.push(quad);
            return;
        }
        let u_span = quad.uv_max[0] - quad.uv_min[0];
        let w = x1 - x0;
        for col in col_first..=col_last {
            let sx0 = if col == col_first {
                x0
            } else {
                f32::from(col) * self.cw
            };
            let sx1 = if col == col_last {
                x1
            } else {
                f32::from(col + 1) * self.cw
            };
            if sx1 <= sx0 {
                continue;
            }
            let t0 = (sx0 - x0) / w;
            let t1 = (sx1 - x0) / w;
            out.fg.push(FgInstance {
                origin_px: [sx0, quad.origin_px[1]],
                size_px: [sx1 - sx0, quad.size_px[1]],
                uv_min: [u_span.mul_add(t0, quad.uv_min[0]), quad.uv_min[1]],
                uv_max: [u_span.mul_add(t1, quad.uv_min[0]), quad.uv_max[1]],
                color: fg_at(col),
                ..quad
            });
        }
    }
}

/// Mirrors `Selection::contains` in `felis-client-core` but stays
/// local so this crate takes no UI-state dep
/// (docs/explanation/architecture/overview.md
/// "Workspace: the crate-boundary decision record").
const fn selection_contains(selection: Option<SelectionRange>, row: u16, col: u16) -> bool {
    let Some(sel) = selection else {
        return false;
    };
    let (sr, sc) = sel.start;
    let (er, ec) = sel.end;
    if row < sr || row > er {
        return false;
    }
    if sel.rectangle {
        return col >= sc && col <= ec;
    }
    if row == sr && col < sc {
        return false;
    }
    if row == er && col > ec {
        return false;
    }
    true
}

/// The nine UAX #9 explicit-formatting characters. Cluster cells walk
/// every codepoint so a base plus combining RLO surfaces.
fn cell_contains_bidi_override(grapheme: Grapheme, screen: &ScreenBuffer) -> bool {
    match grapheme {
        Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer | Grapheme::Ascii(_) => false,
        Grapheme::Char(c) => bidi::is_override(c),
        // (docs/explanation/data-model/grid-and-cells.md "Cluster
        // interning"); an unresolved handle carries no override.
        Grapheme::Cluster(id) => screen
            .cluster_str(id)
            .is_some_and(|s| bidi::iter_contains_override(s.chars())),
    }
}

/// The fg pass runs after the bg pass, so a cell's glyph can only be
/// hidden by not emitting it. The walk mirrors
/// `extend_preedit_instances` so the skipped span matches the painted
/// span (`preedit_span_matches_painted_extent` pins the two).
pub fn preedit_covered_cols(overlay: &PreeditOverlay, grid_cols: u16) -> Option<PreeditSpan> {
    let (row, start) = overlay.anchor;
    let col = overlay_cells(&overlay.text, start, grid_cols)
        .last()?
        .col_end();
    Some(PreeditSpan {
        row,
        col_start: start,
        col_end: col,
    })
}

/// Stops before the first cluster that does not fit the row: pre-edit
/// panels stay on one line in every platform IME. Cells whose glyph
/// misses the atlas still get bg + underline; the renderer primes the
/// overlay text for the next frame.
pub fn extend_preedit_instances<A: AtlasView>(
    overlay: &PreeditOverlay,
    metrics: CellMetrics,
    grid_rows: u16,
    grid_cols: u16,
    atlas: &A,
    out: &mut InstanceBuffers,
) {
    let (row, start) = overlay.anchor;
    if row >= grid_rows {
        return;
    }
    let cw = metrics.width as f32;
    let ch = metrics.height as f32;
    // Mozc / fcitx / iBus surface an active segment on multi-segment
    // compositions.
    let active_range = overlay.cursor.filter(|(s, e)| e > s);
    let first_glyph = out.fg.len();
    for cell in overlay_cells(&overlay.text, start, grid_cols) {
        let origin = [f32::from(cell.col) * cw, f32::from(row) * ch];
        let span_px = [cw * f32::from(cell.width), ch];
        let underline_px =
            if active_range.is_some_and(|(s, e)| cell.bytes.start < e && cell.bytes.end > s) {
                PREEDIT_ACTIVE_UNDERLINE_PX
            } else {
                PREEDIT_UNDERLINE_PX
            };
        out.bg.push(BgInstance {
            origin_px: origin,
            size_px: span_px,
            color: PREEDIT_BG,
        });
        out.bg.push(BgInstance {
            origin_px: [origin[0], origin[1] + ch - underline_px],
            size_px: [span_px[0], underline_px],
            color: theme_default_fg(),
        });
        push_overlay_glyphs(
            atlas,
            metrics,
            &overlay.text[cell.bytes],
            origin,
            &mut out.fg,
        );
    }
    clip_fg_right_of(&mut out.fg, first_glyph, f32::from(grid_cols) * cw);
}

/// One cell of overlay text placed on the grid.
struct OverlayCell {
    bytes: std::ops::Range<usize>,
    col: u16,
    /// Columns drawn: the cluster's width, or fewer when the row's right
    /// edge clips it.
    width: u8,
    clipped: bool,
}

impl OverlayCell {
    fn col_end(&self) -> u16 {
        self.col + u16::from(self.width)
    }
}

/// Lays `text` out from `start` the way the grid would print it. A wide
/// cluster reaching past `grid_cols` is clipped to the columns left and
/// ends the walk: shown in part rather than wrapped, which would lie
/// about where the committed text lands, or dropped.
fn overlay_cells(text: &str, start: u16, grid_cols: u16) -> impl Iterator<Item = OverlayCell> {
    let mut col = start;
    felis_grid::text_cells(text).map_while(move |(bytes, natural)| {
        let left = grid_cols.checked_sub(col).filter(|&left| left > 0)?;
        let width = u8::try_from(left).map_or(natural, |left| natural.min(left));
        let cell = OverlayCell {
            bytes,
            col,
            width,
            clipped: width < natural,
        };
        col = if cell.clipped {
            grid_cols
        } else {
            cell.col_end()
        };
        Some(cell)
    })
}

/// Pre-edit and the bars have no OSC 66 surface, so every glyph takes
/// the default sizing key. A cluster draws its shaped glyphs, or its
/// base char before the renderer has shaped it.
fn push_overlay_glyphs<A: AtlasView>(
    atlas: &A,
    metrics: CellMetrics,
    cluster: &str,
    origin: [f32; 2],
    out: &mut Vec<FgInstance>,
) {
    let ascent = f32::from(u16::try_from(metrics.ascent).unwrap_or(u16::MAX));
    let mut chars = cluster.chars();
    let Some(base) = chars.next() else {
        return;
    };
    if chars.next().is_some()
        && let Some(glyphs) = atlas.overlay_cluster(cluster).filter(|g| !g.is_empty())
    {
        push_cluster_glyphs(
            atlas,
            metrics.height,
            ClusterPen {
                glyphs,
                base_sizing: SizingKey::default(),
                fit_px: 0,
                glyph_scale: 1.0,
                origin: [origin[0], origin[1] + ascent],
                color: theme_default_fg(),
            },
            out,
        );
        return;
    }
    if let Some(slot) = atlas.slot(base, metrics.height, SizingKey::default()) {
        out.push(FgInstance {
            origin_px: [
                origin[0] + slot.offset_px[0] as f32,
                origin[1] + ascent - slot.offset_px[1] as f32,
            ],
            size_px: [slot.size_px[0] as f32, slot.size_px[1] as f32],
            uv_min: slot.uv_min,
            uv_max: slot.uv_max,
            is_color: if slot.is_color { 1.0 } else { 0.0 },
            _pad: [0.0; 3],
            color: theme_default_fg(),
        });
    }
}

/// A shaped cluster's glyphs and where its pen starts: `origin` is the
/// left edge on the baseline. A non-zero `fit_px` reads the glyphs'
/// fitted slots.
#[derive(Clone, Copy)]
struct ClusterPen<'g> {
    glyphs: &'g [crate::glyphs::ClusterGlyph],
    base_sizing: SizingKey,
    fit_px: u16,
    glyph_scale: f32,
    origin: [f32; 2],
    color: [f32; 4],
}

/// Each placed glyph's slot and top-left corner; a glyph with no slot
/// still advances the pen, so later glyphs keep their place.
fn cluster_glyph_quads<'g, A: AtlasView>(
    atlas: &'g A,
    cell_height: u32,
    pen: ClusterPen<'g>,
) -> impl Iterator<Item = (GlyphSlot, [f32; 2])> + 'g {
    pen.glyphs
        .iter()
        .scan(0.0f32, move |x, cg| {
            let key = pen.base_sizing.with_font_id(cg.font_id as usize);
            let slot = match pen.fit_px {
                0 => atlas.glyph_id_slot(cg.glyph_id, cell_height, key),
                px => atlas.fitted_glyph_id_slot(cg.glyph_id, px, cell_height, key),
            };
            let quad = slot.map(|slot| {
                // `slot.offset_px` already comes from the scaled raster, so
                // it is not re-scaled. `mul_add(scale, base)` keeps the
                // default path (`scale == 1`) byte-identical to `b + x`.
                let x_base = pen.origin[0] + *x;
                let gx = cg.x_offset_px.mul_add(pen.glyph_scale, x_base) + slot.offset_px[0] as f32;
                // swash reports GPOS y in font space (y-up); screen space
                // is y-down.
                let y_base = pen.origin[1] - slot.offset_px[1] as f32;
                let gy = cg.y_offset_px.mul_add(-pen.glyph_scale, y_base);
                (slot, [gx, gy])
            });
            *x = cg.advance_px.mul_add(pen.glyph_scale, *x);
            Some(quad)
        })
        .flatten()
}

/// The union of the placed glyphs' ink as `[x0, y0, x1, y1]`.
fn cluster_ink_box<A: AtlasView>(
    atlas: &A,
    cell_height: u32,
    pen: ClusterPen<'_>,
) -> Option<[f32; 4]> {
    cluster_glyph_quads(atlas, cell_height, pen)
        .map(|(slot, [x0, y0])| {
            [
                x0,
                y0,
                x0 + slot.size_px[0] as f32,
                y0 + slot.size_px[1] as f32,
            ]
        })
        .reduce(|[a, b, c, d], [x0, y0, x1, y1]| [a.min(x0), b.min(y0), c.max(x1), d.max(y1)])
}

fn push_cluster_glyphs<A: AtlasView>(
    atlas: &A,
    cell_height: u32,
    pen: ClusterPen<'_>,
    out: &mut Vec<FgInstance>,
) {
    out.extend(
        cluster_glyph_quads(atlas, cell_height, pen).map(|(slot, origin_px)| FgInstance {
            origin_px,
            size_px: [slot.size_px[0] as f32, slot.size_px[1] as f32],
            uv_min: slot.uv_min,
            uv_max: slot.uv_max,
            is_color: if slot.is_color { 1.0 } else { 0.0 },
            _pad: [0.0; 3],
            color: pen.color,
        }),
    );
}

/// The columns a cluster's block spans before the OSC 66 scale: `w`,
/// else the grid's span, so a VS16 widen the grid refused gets one.
pub(crate) fn cluster_block_cols(screen: &ScreenBuffer, row: u16, col: u16) -> u16 {
    match screen.cell_sizing(row, col).map_or(0, |s| s.cell_width()) {
        0 => {
            let (first, last) = screen.char_span(row, col);
            last - first + 1
        }
        w => u16::from(w),
    }
}

/// Hard-coded rather than `theme.fg` so the overlay stays high-contrast
/// against `PREEDIT_BG` whatever the theme.
const fn theme_default_fg() -> [f32; 4] {
    [1.0, 1.0, 1.0, 1.0]
}

/// Appended after the cell pass so highlights and bar paint on top.
/// Atlas misses in the label are skipped; `Renderer::render` primes the
/// label before building instances. Highlights on the bar row are still
/// emitted but covered by the bar.
pub fn extend_search_instances<A: AtlasView>(
    overlay: &SearchOverlay,
    metrics: CellMetrics,
    grid_rows: u16,
    grid_cols: u16,
    draw_bar: bool,
    atlas: &A,
    out: &mut InstanceBuffers,
) {
    if grid_rows == 0 || grid_cols == 0 {
        return;
    }
    let cw = metrics.width as f32;
    let ch = metrics.height as f32;

    for hit in &overlay.visible_hits {
        if hit.row >= grid_rows {
            continue;
        }
        let col_start = hit.col_start.min(grid_cols);
        let col_end = hit.col_end.min(grid_cols);
        if col_end <= col_start {
            continue;
        }
        let color = if hit.is_current {
            SEARCH_CURRENT_BG
        } else {
            SEARCH_MATCH_BG
        };
        out.bg.push(BgInstance {
            origin_px: [f32::from(col_start) * cw, f32::from(hit.row) * ch],
            size_px: [f32::from(col_end - col_start) * cw, ch],
            color,
        });
    }

    // The hit highlights are the search's own rows and always paint;
    // `draw_bar` is only about the one row the bars contend for.
    if !draw_bar || overlay.label.is_empty() {
        return;
    }
    push_bottom_bar(
        &overlay.label,
        SEARCH_BAR_BG,
        metrics,
        grid_rows,
        grid_cols,
        atlas,
        out,
    );
}

/// Same chrome surface as the search bar (the caller reserves the
/// bottom row identically), with [`CONFIRM_BAR_BG`] so a destructive
/// prompt never reads as a search.
#[derive(Debug, Clone, Default)]
pub struct ConfirmOverlay {
    pub label: String,
}

/// Same emission contract as [`extend_search_instances`]'s bar step.
pub fn extend_confirm_instances<A: AtlasView>(
    overlay: &ConfirmOverlay,
    metrics: CellMetrics,
    grid_rows: u16,
    grid_cols: u16,
    atlas: &A,
    out: &mut InstanceBuffers,
) {
    if grid_rows == 0 || grid_cols == 0 || overlay.label.is_empty() {
        return;
    }
    push_bottom_bar(
        &overlay.label,
        CONFIRM_BAR_BG,
        metrics,
        grid_rows,
        grid_cols,
        atlas,
        out,
    );
}

/// The Ctrl-hover OSC 8 activation preview (`docs/explanation/security-model.md`
/// "OSC 8 hyperlinks and OSC 7 CWD"): `text` is already
/// `ActivationTarget::preview`'s output, so it carries no control or
/// bidi codepoint by construction.
#[derive(Debug, Clone, Default)]
pub struct LinkPreviewOverlay {
    pub text: String,
}

/// Same emission contract as [`extend_search_instances`]'s bar step.
/// Reuses [`SEARCH_BAR_BG`] rather than [`CONFIRM_BAR_BG`]: the preview
/// is informational, not a destructive prompt, so it must not read like
/// one.
pub fn extend_link_preview_instances<A: AtlasView>(
    overlay: &LinkPreviewOverlay,
    metrics: CellMetrics,
    grid_rows: u16,
    grid_cols: u16,
    atlas: &A,
    out: &mut InstanceBuffers,
) {
    if grid_rows == 0 || grid_cols == 0 || overlay.text.is_empty() {
        return;
    }
    push_bottom_bar(
        &fit_bar_text_to_cells(&overlay.text, grid_cols),
        SEARCH_BAR_BG,
        metrics,
        grid_rows,
        grid_cols,
        atlas,
        out,
    );
}

/// Cuts the cell pass off at `max_y`, the first pixel row owned by client chrome.
///
/// Glyphs anchored above reserved rows can reach into them via tall bitmaps or OSC 66
/// runs (`docs/reference/protocols/kitty-text-sizing.md`), sharing the foreground pass with chrome.
pub fn clip_cell_instances_above(out: &mut InstanceBuffers, max_y: f32) {
    out.bg.retain_mut(|quad| {
        clip_extent_at(quad.origin_px[1], &mut quad.size_px[1], max_y).is_some()
    });
    out.fg.retain_mut(|quad| {
        let Some(kept) = clip_extent_at(quad.origin_px[1], &mut quad.size_px[1], max_y) else {
            return false;
        };
        quad.uv_max[1] = (quad.uv_max[1] - quad.uv_min[1]).mul_add(kept, quad.uv_min[1]);
        true
    });
    out.deco.retain_mut(|quad| {
        clip_extent_at(quad.origin_px[1], &mut quad.size_px[1], max_y).is_some()
    });
}

/// Cuts the glyph quads from `from` on at `max_x`, the window's right
/// edge, so overlay text never draws past it.
fn clip_fg_right_of(quads: &mut Vec<FgInstance>, from: usize, max_x: f32) {
    let mut i = from;
    while i < quads.len() {
        let quad = &mut quads[i];
        let Some(kept) = clip_extent_at(quad.origin_px[0], &mut quad.size_px[0], max_x) else {
            quads.remove(i);
            continue;
        };
        quad.uv_max[0] = (quad.uv_max[0] - quad.uv_min[0]).mul_add(kept, quad.uv_min[0]);
        i += 1;
    }
}

/// Shrinks `extent`, along one axis, so the quad ends at `max`,
/// reporting the fraction of it that survived, or `None` when nothing
/// does.
fn clip_extent_at(origin: f32, extent: &mut f32, max: f32) -> Option<f32> {
    if *extent <= 0.0 {
        return None;
    }
    if origin + *extent <= max {
        return Some(1.0);
    }
    let visible = max - origin;
    if visible <= 0.0 {
        return None;
    }
    let kept = visible / *extent;
    *extent = visible;
    Some(kept)
}

/// Cuts an image quad off at `max_y`, the first pixel row the client's
/// chrome owns, and drops one that starts at or below it. The atlas `v`
/// axis is linear in the quad's `y`, so the visible fraction of the
/// height is the visible fraction of the source; nothing is rescaled,
/// the tail is simply not sampled.
#[must_use]
pub fn clip_img_quad_above(mut quad: ImgInstance, max_y: f32) -> Option<ImgInstance> {
    let kept = clip_extent_at(quad.origin_px[1], &mut quad.size_px[1], max_y)?;
    quad.uv_max[1] = (quad.uv_max[1] - quad.uv_min[1]).mul_add(kept, quad.uv_min[1]);
    Some(quad)
}

/// Truncation indicator for chrome bars.
///
/// Public so the renderer primes it in the glyph atlas. Synthesized after overlay text
/// population; unprimed glyphs are skipped silently, which would omit the truncation mark.
pub const BAR_ELLIPSIS: char = '…';

/// Fits `text` into `cols` display cells, ending in [`BAR_ELLIPSIS`] when truncated.
///
/// Measured in cells rather than chars to handle wide glyphs, ensuring the ellipsis occupies
/// a full cell rather than half of a two-cell character.
fn fit_bar_text_to_cells(text: &str, cols: u16) -> Cow<'_, str> {
    let width: u32 = felis_grid::text_cells(text)
        .map(|(_, w)| u32::from(w))
        .sum();
    if width <= u32::from(cols) {
        return Cow::Borrowed(text);
    }
    let head_end = overlay_cells(text, 0, cols.saturating_sub(1))
        .take_while(|cell| !cell.clipped)
        .last()
        .map_or(0, |cell| cell.bytes.end);
    let mut head = text[..head_end].to_owned();
    head.push(BAR_ELLIPSIS);
    Cow::Owned(head)
}

/// How the bottom-row overlays share the one row they all draw into.
/// Reserving the row without anything to draw in it is not a state:
/// the cell painter would leave a blank row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomBarClaim {
    /// Nothing holds the row; the cell painter uses every row.
    Free,
    /// A chrome bar holds it.
    Chrome,
    /// The link preview holds it.
    LinkPreview,
}

impl BottomBarClaim {
    /// Rows the cell painter must leave unpainted, so an overlay's
    /// opaque background never has a grid glyph showing through it.
    #[must_use]
    pub const fn reserved_rows(self) -> u16 {
        match self {
            Self::Free => 0,
            Self::Chrome | Self::LinkPreview => 1,
        }
    }

    /// Whether the link preview may emit into that row.
    #[must_use]
    pub const fn draw_link_preview(self) -> bool {
        matches!(self, Self::LinkPreview)
    }
}

/// Resolves bottom-row overlay precedence (`docs/explanation/input.md` "Link preview").
///
/// Passive link previews yield to questions, active searches, or IME preedits to prevent
/// conflicting overlays from drawing over each other. Pure logic testable without a wgpu device.
#[must_use]
pub const fn bottom_bar_claim(
    chrome: ChromeBar,
    preedit_active: bool,
    link_preview_active: bool,
) -> BottomBarClaim {
    if !matches!(chrome, ChromeBar::None) {
        return BottomBarClaim::Chrome;
    }
    if link_preview_active && !preedit_active {
        return BottomBarClaim::LinkPreview;
    }
    BottomBarClaim::Free
}

/// Which deliberate chrome bar holds the bottom row, if either (the
/// resolved winner, not the raw state). A chord can arm confirmation while
/// a search composes; `Confirm` takes precedence because the next keystroke answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeBar {
    None,
    Search,
    Confirm,
}

impl ChromeBar {
    #[must_use]
    pub const fn from_flags(search_active: bool, confirm_active: bool) -> Self {
        if confirm_active {
            Self::Confirm
        } else if search_active {
            Self::Search
        } else {
            Self::None
        }
    }
}

/// Shared by the search and confirmation bars so the two cannot drift
/// in geometry.
fn push_bottom_bar<A: AtlasView>(
    label: &str,
    bar_bg: [f32; 4],
    metrics: CellMetrics,
    grid_rows: u16,
    grid_cols: u16,
    atlas: &A,
    out: &mut InstanceBuffers,
) {
    let cw = metrics.width as f32;
    let ch = metrics.height as f32;
    let bar_row = grid_rows - 1;
    let bar_origin_y = f32::from(bar_row) * ch;
    out.bg.push(BgInstance {
        origin_px: [0.0, bar_origin_y],
        size_px: [f32::from(grid_cols) * cw, ch],
        color: bar_bg,
    });

    // Whitespace advances the column without an fg emission, like the
    // cell pass's atlas-miss handling.
    let first_glyph = out.fg.len();
    for cell in overlay_cells(label, 0, grid_cols) {
        let origin = [f32::from(cell.col) * cw, bar_origin_y];
        push_overlay_glyphs(atlas, metrics, &label[cell.bytes], origin, &mut out.fg);
    }
    clip_fg_right_of(&mut out.fg, first_glyph, f32::from(grid_cols) * cw);
}

/// The XOR makes `?5` compose with SGR 7 the way every modern terminal
/// treats it. Both halves swap the *resolved* colors, so an indexed or
/// direct-RGB pair swaps too.
fn resolve_pair(
    fg: Color,
    bg: Color,
    theme: &ResolvedTheme,
    flags: AttrFlags,
    screen_reverse: bool,
) -> ([f32; 4], [f32; 4]) {
    if flags.contains(AttrFlags::REVERSE) == screen_reverse {
        (resolve_fg(fg, theme), resolve_bg(bg, theme))
    } else {
        (resolve_bg(bg, theme), resolve_fg(fg, theme))
    }
}

/// A 50/50 mix roughly halves perceived brightness on a dark theme,
/// matching kitty's dim intensity.
const FAINT_FG_WEIGHT: f32 = 0.5;

/// Mixes toward the *cell* bg, not black, so a dimmed glyph stays
/// legible on a light or colored background; alpha is untouched.
fn dim_toward_bg(fg: [f32; 4], bg: [f32; 4]) -> [f32; 4] {
    let w = FAINT_FG_WEIGHT;
    [
        fg[0].mul_add(w, bg[0] * (1.0 - w)),
        fg[1].mul_add(w, bg[1] * (1.0 - w)),
        fg[2].mul_add(w, bg[2] * (1.0 - w)),
        fg[3],
    ]
}

/// One px at a ~16 px cell, two at ~32 px.
fn decoration_thickness(ch: f32) -> f32 {
    (ch / 16.0).round().max(1.0)
}

/// An explicit SGR 58 color is dimmed too when the cell is faint so the
/// whole cell reads at one intensity.
fn underline_deco_color(
    underline_color: Color,
    fg_color: [f32; 4],
    bg_color: [f32; 4],
    theme: &ResolvedTheme,
    faint: bool,
) -> [f32; 4] {
    match underline_color {
        Color::Default => fg_color,
        other => {
            let base = resolve_fg(other, theme);
            if faint {
                dim_toward_bg(base, bg_color)
            } else {
                base
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use std::collections::HashMap;

    use felis_grid::{Color, Grid};
    use felis_vt::Parser;

    use super::*;
    use crate::palette::Theme;

    /// Whitespace returns `None`, mimicking swash.
    struct MockAtlas {
        map: HashMap<char, GlyphSlot>,
        gid_map: HashMap<felis_shaping::GlyphId, GlyphSlot>,
        fitted_map: HashMap<(felis_shaping::GlyphId, u16), GlyphSlot>,
        clusters: HashMap<String, Vec<crate::glyphs::ClusterGlyph>>,
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

        fn overlay_cluster(&self, text: &str) -> Option<&[crate::glyphs::ClusterGlyph]> {
            self.clusters.get(text).map(Vec::as_slice)
        }
    }

    fn metrics() -> CellMetrics {
        CellMetrics {
            width: 8,
            height: 16,
            ascent: 12,
        }
    }

    fn slot() -> GlyphSlot {
        GlyphSlot {
            uv_min: [0.0, 0.0],
            uv_max: [0.1, 0.1],
            size_px: [6, 10],
            offset_px: [1, 10],
            is_color: false,
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
    ) -> InstanceBuffers {
        build_instances_with_reverse(screen, metrics, theme, atlas, false)
    }

    fn build_instances_with_reverse<A: AtlasView>(
        screen: &ScreenBuffer,
        metrics: CellMetrics,
        theme: &Theme,
        atlas: &A,
        screen_reverse: bool,
    ) -> InstanceBuffers {
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            screen,
            metrics,
            &ResolvedTheme::new(theme),
            atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_viewport(0, u32::from(screen.rows()))
        .with_reverse_video(screen_reverse)
        .extend_instances(&mut buffers);
        buffers
    }

    #[test]
    fn osc_66_valign_bottom_drops_bitmap_to_block_bottom() {
        // `v=1` (bottom) puts the 10 px bitmap's top at
        // `block_h - bitmap_h = 16 - 10 = 6`.
        let g = drive(1, 4, b"\x1b]66;v=1;A\x07");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.fg.len(), 1);
        assert_eq!(buffers.fg[0].origin_px, [1.0, 6.0]);
    }

    #[test]
    fn osc_66_halign_right_with_w_override_pushes_bitmap_to_block_right_edge() {
        // `w=3, h=1`: block 24 px wide, bitmap 6 px, so its left edge sits
        // at 18; v=Top keeps the baseline placement at 2.
        let g = drive(1, 4, b"\x1b]66;w=3:h=1;A\x07");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.fg.len(), 1);
        assert_eq!(buffers.fg[0].origin_px, [18.0, 2.0]);
    }

    #[test]
    fn osc_66_center_alignment_centers_the_bitmap_in_the_block() {
        // `w=3, h=2, v=2`: centering puts the 6×10 bitmap at
        // `((24 - 6) / 2, (16 - 10) / 2) = (9, 3)`.
        let g = drive(1, 4, b"\x1b]66;w=3:h=2:v=2;A\x07");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.fg.len(), 1);
        assert_eq!(buffers.fg[0].origin_px, [9.0, 3.0]);
    }

    #[test]
    fn osc_66_fractional_scale_shrinks_the_glyph_without_shrinking_the_block() {
        let atlas = MockAtlas::full(slot());
        // `s=2` alone: the glyph rasterizes at 2×, so the v=Top baseline
        // sits at `ascent × 2 - offset_px[1] = 24 - 10 = 14`.
        let integer = drive(2, 4, b"\x1b]66;s=2;A\x07");
        let integer = build_instances(integer.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(integer.fg.len(), 1);
        assert_eq!(integer.fg[0].origin_px, [1.0, 14.0]);

        // `s=2:n=1:d=2`: effective glyph scale `s × n/d = 1`, so the
        // baseline drops back to 2.
        let fractional = drive(2, 4, b"\x1b]66;s=2:n=1:d=2;A\x07");
        let fractional = build_instances(fractional.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(fractional.fg.len(), 1);
        assert_eq!(fractional.fg[0].origin_px, [1.0, 2.0]);

        // The block still measures `s` cells: v=1 anchors at
        // `ch × s - bitmap_h = 32 - 10 = 22`.
        let bottom = drive(2, 4, b"\x1b]66;s=2:n=1:d=2:v=1;A\x07");
        let bottom = build_instances(bottom.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(bottom.fg.len(), 1);
        assert_eq!(bottom.fg[0].origin_px, [1.0, 22.0]);
    }

    #[test]
    fn osc_66_sized_spacer_cells_emit_bg_but_not_fg() {
        // REQ-603: only the primary feeds the fg pass; the three
        // SizedSpacers still emit bg quads.
        let g = drive(2, 4, b"\x1b]66;s=2;X\x07");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.bg.len(), 8);
        assert_eq!(buffers.fg.len(), 1);
        let inst = &buffers.fg[0];
        assert_eq!(inst.origin_px, [1.0, 14.0]);
        assert_eq!(inst.size_px, [6.0, 10.0]);
    }

    /// The slice arithmetic accumulates one f32 rounding step.
    #[track_caller]
    fn assert_uv_close(got: f32, want: f32) {
        assert!(
            (got - want).abs() < 1e-6,
            "uv mismatch: got {got}, want {want}"
        );
    }

    /// 24 px of ink spanning three 8 px cells.
    fn ligature_slot(left: i32) -> GlyphSlot {
        GlyphSlot {
            uv_min: [0.0, 0.0],
            uv_max: [0.3, 0.1],
            size_px: [24, 10],
            offset_px: [left, 10],
            is_color: false,
        }
    }

    /// Cursor hidden so the Block-cursor fg swap cannot leak into the
    /// color assertions.
    fn build_shaped(
        screen: &ScreenBuffer,
        atlas: &MockAtlas,
        frame: &crate::glyphs::ShapeFrame,
    ) -> InstanceBuffers {
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            screen,
            metrics(),
            &ResolvedTheme::new(&Theme::default()),
            atlas,
            frame,
        )
        .with_cursor_visible(false)
        .with_viewport(0, u32::from(screen.rows()))
        .extend_instances(&mut buffers);
        buffers
    }

    /// A shaped ligature quad covering differently-colored cells splits
    /// into per-cell strips (kitty's behavior; fish recolors the `>` of
    /// `<->`).
    #[test]
    fn shaped_quad_slices_per_cell_when_colors_differ() {
        use crate::glyphs::{ShapeFrame, ShapedCell};
        let g = drive(1, 3, b"<-\x1b[31m>");
        let mut atlas = MockAtlas::full(slot());
        atlas.gid_map.insert(42, ligature_slot(0));
        let frame = ShapeFrame::from_cells(
            vec![
                ShapedCell::Primary {
                    glyph_id: 42,
                    span_cols: 3,
                },
                ShapedCell::Trailing,
                ShapedCell::Trailing,
            ],
            3,
        );
        let buffers = build_shaped(g.screen(), &atlas, &frame);
        assert_eq!(buffers.fg.len(), 3, "one strip per covered cell");
        for (i, inst) in buffers.fg.iter().enumerate() {
            let i_f = i as f32;
            assert_eq!(inst.origin_px, [8.0 * i_f, 2.0]);
            assert_eq!(inst.size_px, [8.0, 10.0]);
            assert_uv_close(inst.uv_min[0], 0.1 * i_f);
            assert_uv_close(inst.uv_max[0], 0.1 * (i_f + 1.0));
            assert_eq!(inst.uv_min[1], 0.0);
            assert_eq!(inst.uv_max[1], 0.1);
        }
        let red = resolve_fg(Color::Indexed(1), &ResolvedTheme::new(&Theme::default()));
        assert_eq!(buffers.fg[0].color, buffers.fg[1].color);
        assert_eq!(buffers.fg[2].color, red);
        assert_ne!(buffers.fg[0].color, red);
    }

    /// Uniform colors take the unsliced fast path.
    #[test]
    fn shaped_quad_stays_unsliced_when_colors_match() {
        use crate::glyphs::{ShapeFrame, ShapedCell};
        let g = drive(1, 3, b"<->");
        let mut atlas = MockAtlas::full(slot());
        atlas.gid_map.insert(42, ligature_slot(0));
        let frame = ShapeFrame::from_cells(
            vec![
                ShapedCell::Primary {
                    glyph_id: 42,
                    span_cols: 3,
                },
                ShapedCell::Trailing,
                ShapedCell::Trailing,
            ],
            3,
        );
        let buffers = build_shaped(g.screen(), &atlas, &frame);
        assert_eq!(buffers.fg.len(), 1, "uniform colors must not slice");
        let inst = &buffers.fg[0];
        assert_eq!(inst.origin_px, [0.0, 2.0]);
        assert_eq!(inst.size_px, [24.0, 10.0]);
        assert_eq!(inst.uv_min, [0.0, 0.0]);
        assert_eq!(inst.uv_max, [0.3, 0.1]);
    }

    /// Monaspace-style pieces reach back across earlier cells via
    /// negative left bearing; the slices carry those cells' own colors.
    #[test]
    fn shaped_quad_with_negative_bearing_slices_back_across_cells() {
        use crate::glyphs::{ShapeFrame, ShapedCell};
        let g = drive(1, 3, b"\x1b[31m<\x1b[39m->");
        let mut atlas = MockAtlas::full(slot());
        atlas.gid_map.insert(43, ligature_slot(-16));
        let frame = ShapeFrame::from_cells(
            vec![
                // Blank lead pieces have no atlas slot, as in the real
                // pipeline.
                ShapedCell::Primary {
                    glyph_id: 41,
                    span_cols: 1,
                },
                ShapedCell::Primary {
                    glyph_id: 41,
                    span_cols: 1,
                },
                ShapedCell::Primary {
                    glyph_id: 43,
                    span_cols: 1,
                },
            ],
            3,
        );
        let buffers = build_shaped(g.screen(), &atlas, &frame);
        assert_eq!(buffers.fg.len(), 3, "one strip per reached-back cell");
        let red = resolve_fg(Color::Indexed(1), &ResolvedTheme::new(&Theme::default()));
        assert_eq!(buffers.fg[0].origin_px, [0.0, 2.0]);
        assert_eq!(
            buffers.fg[0].color, red,
            "reached-back cell keeps its own red"
        );
        assert_eq!(
            buffers.fg[1].color, buffers.fg[2].color,
            "default-fg cells match"
        );
        assert_ne!(buffers.fg[1].color, red);
    }

    /// A combining mark (advance 0, reaching-back GPOS x, positive GPOS
    /// y) lands over its base. Hand-rolled `ClusterGlyph`s so the
    /// placement cannot drift with a real font's GPOS.
    #[test]
    fn cluster_cell_emits_one_instance_per_glyph_stacked_by_gpos() {
        use crate::glyphs::{ClusterGlyph, ShapeFrame, ShapedCell};
        let g = drive(1, 4, b"x");
        let mut atlas = MockAtlas::full(slot());
        atlas.gid_map.insert(100, slot());
        atlas.gid_map.insert(200, slot());
        let frame = ShapeFrame::from_cluster_cells(
            vec![
                ShapedCell::Cluster {
                    start: 0,
                    len: 2,
                    fit_px: 0,
                },
                ShapedCell::None,
                ShapedCell::None,
                ShapedCell::None,
            ],
            4,
            vec![
                ClusterGlyph {
                    glyph_id: 100,
                    font_id: 0,
                    advance_px: 8.0,
                    x_offset_px: 0.0,
                    y_offset_px: 0.0,
                },
                ClusterGlyph {
                    glyph_id: 200,
                    font_id: 0,
                    advance_px: 0.0,
                    x_offset_px: -8.0,
                    y_offset_px: 4.0,
                },
            ],
        );
        let buffers = build_shaped(g.screen(), &atlas, &frame);
        assert_eq!(buffers.fg.len(), 2, "one instance per cluster glyph");
        assert_eq!(buffers.fg[0].origin_px, [1.0, 2.0]);
        assert_eq!(buffers.fg[1].origin_px, [1.0, -2.0]);
        assert_eq!(
            buffers.fg[0].origin_px[0], buffers.fg[1].origin_px[0],
            "mark shares the base's x column",
        );
        assert!(
            buffers.fg[1].origin_px[1] < buffers.fg[0].origin_px[1],
            "mark sits above its base (smaller screen y)",
        );
    }

    /// A sized (OSC 66 `s=2`) composited cluster scales pen advance and
    /// GPOS by the glyph scale and lifts the baseline to `ascent × scale`.
    #[test]
    fn sized_cluster_scales_pen_and_gpos_by_glyph_scale() {
        use crate::glyphs::{ClusterGlyph, ShapeFrame, ShapedCell};
        let g = drive(2, 4, b"\x1b]66;s=2;X\x07");
        let mut atlas = MockAtlas::full(slot());
        atlas.gid_map.insert(100, slot());
        atlas.gid_map.insert(200, slot());
        let glyphs = vec![
            ClusterGlyph {
                glyph_id: 100,
                font_id: 0,
                advance_px: 8.0,
                x_offset_px: 0.0,
                y_offset_px: 0.0,
            },
            ClusterGlyph {
                glyph_id: 200,
                font_id: 0,
                advance_px: 0.0,
                x_offset_px: -8.0,
                y_offset_px: 4.0,
            },
        ];
        let mut cells = vec![ShapedCell::None; 8];
        cells[0] = ShapedCell::Cluster {
            start: 0,
            len: 2,
            fit_px: 0,
        };
        let frame = ShapeFrame::from_cluster_cells(cells, 4, glyphs);
        let buffers = build_shaped(g.screen(), &atlas, &frame);
        assert_eq!(buffers.fg.len(), 2, "one instance per cluster glyph");
        assert_eq!(buffers.fg[0].origin_px, [1.0, 14.0]);
        assert_eq!(buffers.fg[1].origin_px, [1.0, 6.0]);
        let rise = buffers.fg[0].origin_px[1] - buffers.fg[1].origin_px[1];
        assert!(
            (rise - 8.0).abs() < f32::EPSILON,
            "mark rises 2× the unscaled 4 px"
        );
        assert_eq!(
            buffers.fg[0].origin_px[0], buffers.fg[1].origin_px[0],
            "mark still shares the base's x column at scale",
        );
    }

    /// Non-default alignment aligns the cluster's union ink box, not the
    /// base glyph's.
    #[test]
    fn sized_cluster_alignment_uses_the_union_ink_box() {
        use crate::glyphs::{ClusterGlyph, ShapeFrame, ShapedCell};
        let g = drive(1, 4, b"\x1b]66;w=2:h=1:v=1;X\x07");
        let mut atlas = MockAtlas::full(slot());
        atlas.gid_map.insert(100, slot());
        atlas.gid_map.insert(
            200,
            GlyphSlot {
                uv_min: [0.0, 0.0],
                uv_max: [0.1, 0.1],
                size_px: [10, 12],
                offset_px: [0, 11],
                is_color: false,
            },
        );
        let glyphs = vec![
            ClusterGlyph {
                glyph_id: 100,
                font_id: 0,
                advance_px: 8.0,
                x_offset_px: 0.0,
                y_offset_px: 0.0,
            },
            ClusterGlyph {
                glyph_id: 200,
                font_id: 0,
                advance_px: 0.0,
                x_offset_px: -8.0,
                y_offset_px: 0.0,
            },
        ];
        let mut cells = vec![ShapedCell::None; 4];
        cells[0] = ShapedCell::Cluster {
            start: 0,
            len: 2,
            fit_px: 0,
        };
        let frame = ShapeFrame::from_cluster_cells(cells, 4, glyphs);
        let buffers = build_shaped(g.screen(), &atlas, &frame);
        assert_eq!(buffers.fg.len(), 2, "one instance per cluster glyph");
        // Union ink box [0,1]..[10,13]: Right shifts 16 − 10 = 6, Bottom
        // 16 − 13 = 3 (base-referenced alignment would shift 9 and
        // overflow by 3 px).
        assert_eq!(buffers.fg[0].origin_px, [7.0, 5.0], "base rides the shift");
        assert_eq!(
            buffers.fg[1].origin_px,
            [6.0, 4.0],
            "the mark's ink ends flush with the block's right/bottom edges",
        );
    }

    /// The block is the grid's span, not the base char's width: VS16
    /// widens a width-1 heart to two cells unless the last column or an
    /// OSC 66 block placed before it refuses it, and OSC 66 `w`
    /// overrides both; `s` scales the block after.
    #[test]
    fn a_cluster_block_spans_the_cells_the_grid_gives_it() {
        let cols = |bytes: &[u8], col| {
            let g = drive(2, 8, bytes);
            assert!(!matches!(
                g.screen().cell(0, col).map(|c| c.grapheme),
                Some(Grapheme::Empty)
            ));
            cluster_block_cols(g.screen(), 0, col)
        };
        assert_eq!(cols("\u{2764}\u{FE0F}".as_bytes(), 0), 2, "widened");
        assert_eq!(cols("\x1b[8G\u{2764}\u{FE0F}".as_bytes(), 7), 1, "refused");
        assert_eq!(
            cols("\x1b]66;w=3;e\u{301}\x07".as_bytes(), 0),
            3,
            "OSC 66 w"
        );
        assert_eq!(
            cols("\x1b]66;s=2;\u{2764}\u{FE0F}\x07".as_bytes(), 0),
            2,
            "sized, VS16 in the run"
        );
        assert_eq!(
            cols("\x1b]66;s=2;\u{2764}\x07\u{FE0F}".as_bytes(), 0),
            1,
            "sized, VS16 printed after"
        );
        assert_eq!(
            cols("\x1b]66;s=2;\u{5B57}\u{301}\x07".as_bytes(), 0),
            2,
            "sized, wide base"
        );
    }

    /// A centred cluster centres in the two cells a widened VS16 heart
    /// takes, where its width-1 base would centre it in one.
    #[test]
    fn a_centred_cluster_centres_in_its_grid_span() {
        use crate::glyphs::{ClusterGlyph, ShapeFrame, ShapedCell};
        let g = drive(1, 4, "\x1b]66;h=2;\u{2764}\u{FE0F}\x07".as_bytes());
        assert_eq!(g.screen().char_span(0, 0), (0, 1), "the grid widened it");
        let mut atlas = MockAtlas::full(slot());
        atlas.gid_map.insert(100, slot());
        let glyph = ClusterGlyph {
            glyph_id: 100,
            font_id: 0,
            advance_px: 8.0,
            x_offset_px: 0.0,
            y_offset_px: 0.0,
        };
        let mut cells = vec![ShapedCell::None; 4];
        cells[0] = ShapedCell::Cluster {
            start: 0,
            len: 1,
            fit_px: 0,
        };
        let frame = ShapeFrame::from_cluster_cells(cells, 4, vec![glyph]);
        let buffers = build_shaped(g.screen(), &atlas, &frame);
        // Ink 6 px wide from x = 1: (16 − 6) / 2 − 1 = 4, plus the bearing.
        assert_eq!(buffers.fg[0].origin_px[0], 5.0);
    }

    /// A fitted cluster reads the slots rasterized at its fitted size,
    /// never the native ones, and Top/Left centres its ink in the block
    /// as a fitted char's bitmap is centred.
    #[test]
    fn a_fitted_cluster_draws_its_fitted_slots_centred() {
        use crate::glyphs::{ClusterGlyph, ShapeFrame, ShapedCell};
        let g = drive(1, 4, b"x");
        let mut atlas = MockAtlas::full(slot());
        atlas.gid_map.insert(100, slot());
        atlas.fitted_map.insert(
            (100, 9),
            GlyphSlot {
                size_px: [4, 6],
                offset_px: [3, 9],
                ..slot()
            },
        );
        let glyph = ClusterGlyph {
            glyph_id: 100,
            font_id: 0,
            advance_px: 8.0,
            x_offset_px: 0.0,
            y_offset_px: 0.0,
        };
        let mut cells = vec![ShapedCell::None; 4];
        cells[0] = ShapedCell::Cluster {
            start: 0,
            len: 1,
            fit_px: 9,
        };
        let frame = ShapeFrame::from_cluster_cells(cells, 4, vec![glyph]);
        let buffers = build_shaped(g.screen(), &atlas, &frame);
        assert_eq!(buffers.fg.len(), 1);
        assert_eq!(buffers.fg[0].size_px, [4.0, 6.0], "the fitted slot");
        // An 8×16 cell centres a 4×6 ink at (2, 5).
        assert_eq!(buffers.fg[0].origin_px, [2.0, 5.0]);
    }

    /// A single-glyph cluster places identically to the equivalent
    /// `Char` cell.
    #[test]
    fn single_glyph_cluster_matches_char_placement() {
        use crate::glyphs::{ClusterGlyph, ShapeFrame, ShapedCell};
        let char_grid = drive(1, 4, b"A");
        let atlas = MockAtlas::full(slot());
        let char_buffers =
            build_instances(char_grid.screen(), metrics(), &Theme::default(), &atlas);
        let char_inst = char_buffers.fg[0];
        let g = drive(1, 4, b"x");
        let mut cluster_atlas = MockAtlas::full(slot());
        cluster_atlas.gid_map.insert(100, slot());
        let frame = ShapeFrame::from_cluster_cells(
            vec![
                ShapedCell::Cluster {
                    start: 0,
                    len: 1,
                    fit_px: 0,
                },
                ShapedCell::None,
                ShapedCell::None,
                ShapedCell::None,
            ],
            4,
            vec![ClusterGlyph {
                glyph_id: 100,
                font_id: 0,
                advance_px: 8.0,
                x_offset_px: 0.0,
                y_offset_px: 0.0,
            }],
        );
        let buffers = build_shaped(g.screen(), &cluster_atlas, &frame);
        assert_eq!(
            buffers.fg.len(),
            1,
            "single-glyph cluster emits one instance"
        );
        assert_eq!(buffers.fg[0].origin_px, char_inst.origin_px);
        assert_eq!(buffers.fg[0].size_px, char_inst.size_px);
        assert_eq!(buffers.fg[0].uv_min, char_inst.uv_min);
        assert_eq!(buffers.fg[0].uv_max, char_inst.uv_max);
    }

    #[test]
    fn empty_grid_emits_one_bg_per_cell_and_no_fg() {
        let g = drive(2, 3, b"");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.bg.len(), 6);
        assert_eq!(buffers.fg, Vec::<FgInstance>::new());
        let last = buffers.bg.last().unwrap();
        assert_eq!(last.origin_px, [16.0, 16.0]);
        assert_eq!(last.size_px, [8.0, 16.0]);
    }

    #[test]
    fn ascii_glyph_emits_a_fg_instance_at_baseline_offset() {
        let g = drive(1, 3, b"A");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.bg.len(), 3);
        assert_eq!(buffers.fg.len(), 1);
        let inst = &buffers.fg[0];
        assert_eq!(inst.origin_px, [1.0, 2.0]);
        assert_eq!(inst.size_px, [6.0, 10.0]);
        assert_eq!(inst.uv_min, [0.0, 0.0]);
        assert_eq!(inst.uv_max, [0.1, 0.1]);
    }

    #[test]
    fn reverse_video_swaps_fg_and_bg_colors() {
        // Cursor hidden so its reverse-video pass does not stack on SGR 7.
        let g = drive(1, 2, b"\x1b[?25lA\x1b[7mB");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let theme = Theme::default();
        assert_eq!(buffers.fg[0].color, theme.fg);
        assert_eq!(buffers.bg[0].color, theme.bg);
        assert_eq!(buffers.fg[1].color, theme.bg);
        assert_eq!(buffers.bg[1].color, theme.fg);
    }

    /// DECSCNM and SGR 7 compose by XOR on the *resolved* pair.
    #[test]
    fn decscnm_xors_with_sgr_reverse_over_the_resolved_pair() {
        let g = drive(1, 4, b"\x1b[?25lA\x1b[7mB\x1b[0;38;5;1;48;5;4mC\x1b[7mD");
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let resolved = ResolvedTheme::new(&theme);
        let pal = |i: u8| resolve_fg(Color::Indexed(i), &resolved);
        let (dfg, dbg) = (theme.fg, theme.bg);
        let (efg, ebg) = (pal(1), pal(4));

        let plain = build_instances_with_reverse(g.screen(), metrics(), &theme, &atlas, false);
        let pairs: Vec<_> = (0..4)
            .map(|i| (plain.fg[i].color, plain.bg[i].color))
            .collect();
        assert_eq!(
            pairs,
            vec![(dfg, dbg), (dbg, dfg), (efg, ebg), (ebg, efg)],
            "without DECSCNM only SGR 7 swaps",
        );

        let reversed = build_instances_with_reverse(g.screen(), metrics(), &theme, &atlas, true);
        let pairs: Vec<_> = (0..4)
            .map(|i| (reversed.fg[i].color, reversed.bg[i].color))
            .collect();
        assert_eq!(
            pairs,
            vec![(dbg, dfg), (dfg, dbg), (ebg, efg), (efg, ebg)],
            "DECSCNM flips every cell, SGR 7 cells back to normal",
        );
    }

    #[test]
    fn atlas_miss_drops_only_the_fg_instance() {
        let mut atlas_map = HashMap::new();
        atlas_map.insert('A', slot());
        let atlas = MockAtlas {
            map: atlas_map,
            gid_map: HashMap::new(),
            fitted_map: HashMap::new(),
            clusters: HashMap::new(),
        };
        let g = drive(1, 2, b"AB");
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.bg.len(), 2, "every cell still has a bg");
        assert_eq!(buffers.fg.len(), 1, "B produced no glyph instance");
    }

    #[test]
    fn sgr_indexed_color_is_resolved_via_palette() {
        let g = drive(1, 1, b"\x1b[?25l\x1b[31mZ");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let expected = resolve_fg(Color::Indexed(1), &ResolvedTheme::new(&Theme::default()));
        assert_eq!(buffers.fg[0].color, expected);
    }

    #[test]
    fn cursor_cell_renders_as_reverse_video_block() {
        let g = drive(1, 3, b"A");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let theme = Theme::default();
        assert_eq!(g.cursor().row, 0);
        assert_eq!(g.cursor().col, 1);
        assert_eq!(
            buffers.bg[1].color, theme.fg,
            "cursor cell bg must be the theme fg (reverse video)"
        );
        assert_eq!(buffers.bg[0].color, theme.bg);
        assert_eq!(buffers.fg[0].color, theme.fg);
    }

    #[test]
    fn hidden_cursor_skips_reverse_video_swap() {
        // `?25l`: the cursor cell renders like any other empty cell.
        let g = drive(1, 3, b"\x1b[?25lA");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let theme = Theme::default();
        assert!(!g.cursor().visible);
        assert_eq!(buffers.bg[1].color, theme.bg);
    }

    #[test]
    fn bidi_override_cell_paints_with_warning_marker() {
        // The cell carrying U+202E paints the marker whether the parser
        // places it as its own cell or folds it into a Cluster.
        let mut g = Grid::new(1, 4);
        let mut p = Parser::new();
        p.advance(&mut g, "\x1b[?25lA\u{202E}B".as_bytes());
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let mut marked = false;
        for r in 0..g.rows() {
            for c in 0..g.cols() {
                let Some(cell) = g.cell(r, c) else { continue };
                if cell_contains_bidi_override(cell.grapheme, g.screen()) {
                    let idx = usize::from(r) * usize::from(g.cols()) + usize::from(c);
                    assert_eq!(
                        buffers.bg[idx].color, BIDI_MARKER_BG,
                        "bidi-override cell at ({r},{c}) must paint with marker bg"
                    );
                    marked = true;
                }
            }
        }
        assert!(
            marked,
            "parser should have placed the RLO in some cell — neither Char nor Cluster found"
        );
    }

    #[test]
    fn bidi_override_at_column_zero_marks_the_character_after_it() {
        let g = drive(1, 4, "\x1b[?25l\u{202E}AB".as_bytes());
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
        assert_eq!(buffers.bg[0].color, BIDI_MARKER_BG);
        assert_eq!(buffers.bg[1].color, theme.bg);
    }

    #[test]
    fn ordinary_text_does_not_trigger_bidi_marker() {
        // RTL script content is text, not an attack vector.
        let mut g = Grid::new(1, 8);
        let mut p = Parser::new();
        p.advance(&mut g, b"\x1b[?25lhello");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let theme = Theme::default();
        for inst in &buffers.bg {
            assert_ne!(
                inst.color, BIDI_MARKER_BG,
                "non-bidi text must not paint the warning marker"
            );
        }
        assert_eq!(buffers.bg[0].color, theme.bg);
    }

    #[test]
    fn bidi_marker_overrides_cursor_swap() {
        // The cursor's reverse-video pass must not mask the marker.
        let g = drive(1, 4, "A\u{202E}\x1b[1;1H".as_bytes());
        assert!(cell_contains_bidi_override(
            g.cell(0, 0).unwrap().grapheme,
            g.screen()
        ));
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.bg[0].color, BIDI_MARKER_BG);
        assert!(
            !buffers.fg.is_empty(),
            "the `A` under the cursor draws a glyph"
        );
        assert!(
            buffers.fg.iter().all(|f| f.color == BIDI_MARKER_FG),
            "the glyph on the cursor cell keeps the marker fg"
        );
    }

    #[test]
    fn underline_cursor_appends_a_thin_bar_at_the_bottom_of_the_cell() {
        let g = drive(1, 3, b"\x1b[4 qA");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let theme = Theme::default();
        assert_eq!(g.cursor().col, 1);
        assert_eq!(buffers.bg[0].color, theme.bg, "cell 0 normal");
        assert_eq!(buffers.bg[1].color, theme.bg, "cursor cell bg unchanged");
        let marker = buffers
            .bg
            .iter()
            .find(|b| b.size_px[1] == CURSOR_MARKER_PX)
            .expect("underline marker present");
        assert_eq!(marker.size_px, [metrics().width as f32, CURSOR_MARKER_PX]);
        let expected_y = metrics().height as f32 - CURSOR_MARKER_PX;
        assert_eq!(marker.origin_px[1], expected_y);
    }

    #[test]
    fn bar_cursor_appends_a_thin_vertical_strip_at_the_left_of_the_cell() {
        let g = drive(1, 3, b"\x1b[6 qA");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let marker = buffers
            .bg
            .iter()
            .find(|b| b.size_px[0] == CURSOR_MARKER_PX)
            .expect("bar marker present");
        assert_eq!(marker.size_px, [CURSOR_MARKER_PX, metrics().height as f32]);
        let cell_origin = [metrics().width as f32 * 1.0, 0.0];
        assert_eq!(marker.origin_px, cell_origin);
    }

    #[test]
    fn block_cursor_keeps_reverse_video_swap_no_marker_quad() {
        let g = drive(1, 3, b"AB"); // default style stays Block
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.bg.len(), 3);
    }

    #[test]
    fn underline_cursor_uses_theme_cursor_color_when_configured() {
        let theme = Theme {
            cursor: Some([0.5, 0.7, 0.9, 1.0]),
            ..Theme::default()
        };
        let g = drive(1, 3, b"\x1b[4 qA");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
        let marker = buffers
            .bg
            .iter()
            .find(|b| b.size_px[1] == CURSOR_MARKER_PX)
            .expect("underline marker present");
        assert_eq!(marker.color, [0.5, 0.7, 0.9, 1.0]);
    }

    #[test]
    fn bar_cursor_uses_theme_cursor_color_when_configured() {
        let theme = Theme {
            cursor: Some([0.5, 0.7, 0.9, 1.0]),
            ..Theme::default()
        };
        let g = drive(1, 3, b"\x1b[6 qA");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
        let marker = buffers
            .bg
            .iter()
            .find(|b| b.size_px[0] == CURSOR_MARKER_PX)
            .expect("bar marker present");
        assert_eq!(marker.color, [0.5, 0.7, 0.9, 1.0]);
    }

    #[test]
    fn bidi_marker_overrides_underline_cursor_marker() {
        // The bidi warning paints the whole cell; the marker must not
        // also fire.
        let mut g = Grid::new(1, 4);
        let mut p = Parser::new();
        p.advance(&mut g, "\x1b[4 qA\u{202E}\x1b[1;1H".as_bytes());
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let marker = buffers.bg.iter().find(|b| b.size_px[1] == CURSOR_MARKER_PX);
        assert!(
            marker.is_none(),
            "bidi marker must suppress the underline marker"
        );
    }

    #[test]
    fn bidi_marker_overrides_bar_cursor_marker() {
        let mut g = Grid::new(1, 4);
        let mut p = Parser::new();
        p.advance(&mut g, "\x1b[6 qA\u{202E}\x1b[1;1H".as_bytes());
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let marker = buffers.bg.iter().find(|b| b.size_px[0] == CURSOR_MARKER_PX);
        assert!(
            marker.is_none(),
            "bidi marker must suppress the bar marker too"
        );
    }

    #[test]
    fn unfocused_window_suppresses_the_cursor_block() {
        let g = drive(1, 3, b"AB");
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(false)
        .with_viewport(0, u32::from(g.rows()))
        .extend_instances(&mut buffers);
        assert_eq!(buffers.bg.len(), 3);
        for inst in &buffers.bg {
            assert_eq!(inst.color, theme.bg, "no cell painted with cursor swap");
        }
    }

    #[test]
    fn reserved_bottom_rows_keeps_bar_row_free_of_cells() {
        // Cells under the bar must not be emitted, or their glyphs
        // double through the label.
        let g = drive(2, 3, b"AB\r\nCD");
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_viewport(0, u32::from(g.rows()))
        .with_reserved_bottom_rows(1)
        .extend_instances(&mut buffers);
        assert_eq!(
            buffers.bg.len(),
            3,
            "bottom row's bg cells must be suppressed"
        );
        let bar_y = f32::from(g.rows() - 1) * metrics().height as f32;
        for inst in &buffers.bg {
            assert!(
                inst.origin_px[1] < bar_y,
                "bg cell leaked onto the reserved bar row"
            );
        }
        for inst in &buffers.fg {
            assert!(
                inst.origin_px[1] < bar_y,
                "fg glyph leaked onto the reserved bar row"
            );
        }
    }

    #[test]
    fn preedit_overlay_span_is_reserved_from_the_cell_walk() {
        // Cells under the composition overlay must not be emitted, or
        // their glyph draws through it in the later fg pass.
        let g = drive(1, 5, b"ABCDE");
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 1),
            text: "あ".to_string(),
            cursor: None,
        };
        let span = preedit_covered_cols(&overlay, g.cols());
        assert_eq!(
            span,
            Some(PreeditSpan {
                row: 0,
                col_start: 1,
                col_end: 3,
            }),
            "wide char covers cols 1 and 2",
        );
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_viewport(0, u32::from(g.rows()))
        .with_preedit(span)
        .extend_instances(&mut buffers);
        let cw = metrics().width as f32;
        let leaked = |x: f32| {
            let col = (x / cw).round() as u16;
            (col == 1 || col == 2).then_some(col)
        };
        for inst in &buffers.bg {
            assert!(
                leaked(inst.origin_px[0]).is_none(),
                "cell bg leaked under the pre-edit overlay at col {:?}",
                leaked(inst.origin_px[0]),
            );
        }
        for inst in &buffers.fg {
            assert!(
                leaked(inst.origin_px[0]).is_none(),
                "glyph leaked under the pre-edit overlay at col {:?}",
                leaked(inst.origin_px[0]),
            );
        }
    }

    #[test]
    fn preedit_span_matches_painted_extent() {
        // The reserved span and the painted span walk the same clamping
        // formula; pin them to the same right edge.
        let atlas = MockAtlas::full(slot());
        let overlay = PreeditOverlay {
            anchor: (0, 1),
            text: "あxい".to_string(), // 2 + 1 + 2 = 5 cells from col 1
            cursor: None,
        };
        let cols = 6; // col 1 + 5 cells == exactly the right edge
        let mut buffers = InstanceBuffers::default();
        extend_preedit_instances(&overlay, metrics(), 1, cols, &atlas, &mut buffers);
        let cw = metrics().width as f32;
        let painted_end = buffers
            .bg
            .iter()
            .map(|b| b.origin_px[0] + b.size_px[0])
            .fold(0.0_f32, f32::max);
        let span_end = preedit_covered_cols(&overlay, cols)
            .expect("non-empty overlay")
            .col_end;
        assert_eq!(
            f32::from(span_end) * cw,
            painted_end,
            "reserved span must end where the painted overlay ends",
        );
    }

    #[test]
    fn unfocused_window_suppresses_underline_marker_too() {
        let g = drive(1, 3, b"\x1b[4 qA");
        let atlas = MockAtlas::full(slot());
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&Theme::default()),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(false)
        .with_viewport(0, u32::from(g.rows()))
        .extend_instances(&mut buffers);
        let marker = buffers.bg.iter().find(|b| b.size_px[1] == CURSOR_MARKER_PX);
        assert!(
            marker.is_none(),
            "underline marker must drop when unfocused"
        );
    }

    #[test]
    fn unfocused_window_suppresses_bar_marker_too() {
        let g = drive(1, 3, b"\x1b[6 qA");
        let atlas = MockAtlas::full(slot());
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&Theme::default()),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(false)
        .with_viewport(0, u32::from(g.rows()))
        .extend_instances(&mut buffers);
        let marker = buffers.bg.iter().find(|b| b.size_px[0] == CURSOR_MARKER_PX);
        assert!(marker.is_none(), "bar marker must drop when unfocused");
    }

    #[test]
    fn selection_highlight_paints_cells_inside_the_range() {
        let g = drive(1, 5, b"\x1b[?25l"); // hide cursor for isolation
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_selection(Some(SelectionRange {
            start: (0, 1),
            end: (0, 3),
            rectangle: false,
        }))
        .with_viewport(0, u32::from(g.rows()))
        .extend_instances(&mut buffers);
        assert_eq!(buffers.bg.len(), 5);
        assert_eq!(buffers.bg[0].color, theme.bg, "cell 0 outside selection");
        for (i, buffer) in buffers.bg.iter().enumerate().skip(1).take(3) {
            assert_eq!(buffer.color, SELECTION_BG, "cell {i} should be highlighted");
        }
        assert_eq!(buffers.bg[4].color, theme.bg, "cell 4 outside selection");
    }

    #[test]
    fn selection_highlight_clips_to_boundary_columns_across_rows() {
        let g = drive(3, 4, b"\x1b[?25l");
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_selection(Some(SelectionRange {
            start: (0, 2),
            end: (2, 1),
            rectangle: false,
        }))
        .with_viewport(0, u32::from(g.rows()))
        .extend_instances(&mut buffers);
        let inside = |idx: usize| matches!(idx, 2..=9);
        for (idx, inst) in buffers.bg.iter().enumerate() {
            if inside(idx) {
                assert_eq!(inst.color, SELECTION_BG, "idx {idx} should be highlighted");
            } else {
                assert_eq!(inst.color, theme.bg, "idx {idx} outside selection");
            }
        }
    }

    #[test]
    fn no_selection_leaves_every_cell_on_theme_bg() {
        let g = drive(1, 5, b"\x1b[?25l");
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_viewport(0, u32::from(g.rows()))
        .extend_instances(&mut buffers);
        for inst in &buffers.bg {
            assert_ne!(inst.color, SELECTION_BG);
        }
    }

    #[test]
    fn bidi_marker_wins_over_selection_highlight() {
        // A bidi-override cell inside a selection paints the marker, or
        // an attacker could hide behind the selection tint.
        let mut g = Grid::new(1, 4);
        let mut p = Parser::new();
        p.advance(&mut g, "\x1b[?25lA\u{202E}B".as_bytes());
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_selection(Some(SelectionRange {
            start: (0, 0),
            end: (0, 3),
            rectangle: false,
        }))
        .with_viewport(0, u32::from(g.rows()))
        .extend_instances(&mut buffers);
        for r in 0..g.rows() {
            for c in 0..g.cols() {
                let Some(cell) = g.cell(r, c) else { continue };
                if cell_contains_bidi_override(cell.grapheme, g.screen()) {
                    let idx = usize::from(r) * usize::from(g.cols()) + usize::from(c);
                    assert_eq!(
                        buffers.bg[idx].color, BIDI_MARKER_BG,
                        "bidi cell at ({r},{c}) must keep marker bg",
                    );
                }
            }
        }
    }

    #[test]
    fn cursor_uses_theme_cursor_color_when_configured() {
        let theme = Theme {
            cursor: Some([0.5, 0.7, 0.9, 1.0]),
            ..Theme::default()
        };
        let g = drive(1, 3, b"AB");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
        assert_eq!(g.cursor().col, 2);
        assert_eq!(buffers.bg[2].color, [0.5, 0.7, 0.9, 1.0]);
    }

    #[test]
    fn cursor_glyph_renders_in_reversed_fg_color() {
        let g = drive(1, 3, b"AB\x1b[1;1H");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let theme = Theme::default();
        assert_eq!(g.cursor().row, 0);
        assert_eq!(g.cursor().col, 0);
        assert_eq!(buffers.bg[0].color, theme.fg);
        assert_eq!(
            buffers.fg[0].color, theme.bg,
            "cursor cell glyph must paint in reversed fg color"
        );
    }

    #[test]
    fn extend_preedit_empty_text_is_noop() {
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 0),
            text: String::new(),
            cursor: None,
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 5, 80, &atlas, &mut out);
        assert_eq!(out.bg, Vec::<BgInstance>::new());
        assert_eq!(out.fg, Vec::<FgInstance>::new());
    }

    #[test]
    fn extend_preedit_emits_bg_underline_glyph_per_ascii_char() {
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (1, 2),
            text: "ab".to_owned(),
            cursor: None,
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 5, 80, &atlas, &mut out);
        assert_eq!(out.bg.len(), 4);
        assert_eq!(out.fg.len(), 2);
        assert_eq!(out.bg[0].origin_px, [16.0, 16.0]);
        assert_eq!(out.bg[0].size_px, [8.0, 16.0]);
        assert_eq!(out.bg[0].color, PREEDIT_BG);
        assert_eq!(out.bg[1].origin_px, [16.0, 30.0]);
        assert_eq!(out.bg[1].size_px[1], PREEDIT_UNDERLINE_PX);
        assert_eq!(out.bg[2].origin_px, [24.0, 16.0]);
    }

    #[test]
    fn extend_preedit_advances_two_cells_for_wide_glyph() {
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 0),
            text: "あい".to_owned(),
            cursor: None,
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 5, 80, &atlas, &mut out);
        assert_eq!(out.bg[0].origin_px, [0.0, 0.0]);
        assert_eq!(out.bg[0].size_px, [16.0, 16.0]);
        assert_eq!(out.bg[2].origin_px, [16.0, 0.0]);
        assert_eq!(out.bg[2].size_px, [16.0, 16.0]);
    }

    #[test]
    fn extend_preedit_anchor_out_of_range_is_noop() {
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (10, 0),
            text: "x".to_owned(),
            cursor: None,
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 5, 80, &atlas, &mut out);
        assert_eq!(out.bg, Vec::<BgInstance>::new());
    }

    #[test]
    fn extend_preedit_col_at_or_past_grid_cols_is_noop() {
        // A daemon-side cursor briefly reporting `col == cols` must not
        // push off-screen instances.
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 80),
            text: "abc".to_owned(),
            cursor: None,
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 5, 80, &atlas, &mut out);
        assert!(out.bg.is_empty(), "no bg pushed when col >= grid_cols");
        assert!(out.fg.is_empty(), "no glyph pushed when col >= grid_cols");
    }

    #[test]
    fn extend_preedit_active_segment_paints_thicker_underline() {
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 0),
            text: "abcde".to_owned(),
            cursor: Some((1, 3)),
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 1, 80, &atlas, &mut out);
        assert!(
            (out.bg[1].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
            "char 0 'a' outside active range: default underline",
        );
        assert!(
            (out.bg[3].size_px[1] - PREEDIT_ACTIVE_UNDERLINE_PX).abs() < f32::EPSILON,
            "char 1 'b' inside active range: thicker underline",
        );
        assert!(
            (out.bg[5].size_px[1] - PREEDIT_ACTIVE_UNDERLINE_PX).abs() < f32::EPSILON,
            "char 2 'c' inside active range: thicker underline",
        );
        assert!(
            (out.bg[7].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
            "char 3 'd' outside active range: default underline",
        );
    }

    #[test]
    fn extend_preedit_empty_active_range_falls_back_to_default_underline() {
        // start == end is a position, not a range.
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 0),
            text: "ab".to_owned(),
            cursor: Some((1, 1)),
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 1, 80, &atlas, &mut out);
        for i in [1, 3] {
            assert!(
                (out.bg[i].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
                "no thicker underline when cursor is a single point",
            );
        }
    }

    #[test]
    fn extend_preedit_active_segment_handles_multibyte_chars() {
        // Each char is 3 bytes; (3, 6) covers exactly the second.
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 0),
            text: "あいう".to_owned(),
            cursor: Some((3, 6)),
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 1, 80, &atlas, &mut out);
        assert!(
            (out.bg[1].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
            "あ outside active range: default underline",
        );
        assert!(
            (out.bg[3].size_px[1] - PREEDIT_ACTIVE_UNDERLINE_PX).abs() < f32::EPSILON,
            "い inside active range: thicker underline",
        );
        assert!(
            (out.bg[5].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
            "う outside active range: default underline",
        );
    }

    #[test]
    fn extend_preedit_clips_at_right_edge_of_row() {
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 4),
            text: "abc".to_owned(),
            cursor: None,
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 1, 5, &atlas, &mut out);
        assert_eq!(out.bg.len(), 2);
        assert_eq!(out.fg.len(), 1);
    }

    #[test]
    fn extend_preedit_clips_wide_glyph_at_right_edge_to_one_cell() {
        // A wide glyph at the last cell clips rather than wrapping (which
        // would lie about where the committed char lands) or vanishing.
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 4),
            text: "あ".to_owned(),
            cursor: None,
        };
        let atlas = MockAtlas::full(slot());
        extend_preedit_instances(&overlay, metrics(), 1, 5, &atlas, &mut out);
        // MockAtlas only knows ASCII, so the fg pass is empty; the bg /
        // underline geometry carries the clip.
        assert_eq!(out.bg.len(), 2, "bg quad + underline only");
        assert_eq!(out.bg[0].size_px, [8.0, 16.0], "bg clipped to 1 cell");
        assert_eq!(out.bg[1].size_px[0], 8.0, "underline clipped to 1 cell");
    }

    #[test]
    fn link_cells_emit_an_extra_underline_instance() {
        use crate::palette::LINK_UNDERLINE;
        let g = drive(
            1,
            4,
            b"\x1b[?25l\x1b]8;;https://example.com\x07ab\x1b]8;;\x07cd",
        );
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.bg.len(), 6, "two link cells each add an underline");
        assert_eq!(buffers.bg[1].color, LINK_UNDERLINE);
        assert_eq!(buffers.bg[1].size_px[1], LINK_UNDERLINE_PX);
        assert_eq!(buffers.bg[3].color, LINK_UNDERLINE);
        assert_eq!(buffers.bg[4].color, Theme::default().bg);
        assert_eq!(buffers.bg[5].color, Theme::default().bg);
    }

    /// REQ-910: a `javascript:` OSC 8 emits no `LINK_UNDERLINE`.
    #[test]
    fn disallowed_scheme_emits_no_link_underline_instance() {
        use crate::palette::LINK_UNDERLINE;
        let g = drive(
            1,
            6,
            b"\x1b[?25l\x1b]8;;javascript:alert(1)\x07evil\x1b]8;;\x07ok",
        );
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert!(
            buffers.bg.iter().all(|b| b.color != LINK_UNDERLINE),
            "javascript: scheme leaked past dispatch_osc_8 and produced a link underline",
        );
    }

    /// `https://good` then `javascript:bad`: the bad open clears the pen
    /// rather than inheriting the safe URI.
    #[test]
    fn disallowed_scheme_after_safe_open_clears_underline_run() {
        use crate::palette::LINK_UNDERLINE;
        let g = drive(
            1,
            8,
            b"\x1b[?25l\x1b]8;;https://good\x07safe\x1b]8;;javascript:bad\x07evil\x1b]8;;\x07",
        );
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let underline_count = buffers
            .bg
            .iter()
            .filter(|b| b.color == LINK_UNDERLINE)
            .count();
        assert_eq!(
            underline_count, 4,
            "underline run leaked past the javascript: pen-clear into `evil`",
        );
    }

    #[test]
    fn link_underline_skipped_on_bidi_marker_cells() {
        // The bidi marker drops the link underline so the warning stays
        // unobstructed.
        use crate::palette::{BIDI_MARKER_BG, LINK_UNDERLINE};
        let g = drive(
            1,
            3,
            "\x1b[?25l\x1b]8;;https://example.test/\u{0007}A\u{202E}\x1b]8;;\u{0007}".as_bytes(),
        );
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        assert_eq!(buffers.bg[0].color, BIDI_MARKER_BG);
        assert!(
            buffers.bg.iter().all(|b| b.color != LINK_UNDERLINE),
            "link underline must not paint over bidi marker",
        );
    }

    #[test]
    fn rectangle_selection_clips_every_row_to_column_range() {
        let g = drive(3, 4, b"\x1b[?25l");
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_selection(Some(SelectionRange {
            start: (0, 1),
            end: (2, 2),
            rectangle: true,
        }))
        .with_viewport(0, u32::from(g.rows()))
        .extend_instances(&mut buffers);
        let inside = |r: u16, c: u16| (1..=2).contains(&c) && (0..=2).contains(&r);
        for r in 0..3u16 {
            for c in 0..4u16 {
                let idx = (usize::from(r) * 4) + usize::from(c);
                let want_highlight = inside(r, c);
                let got = buffers.bg[idx].color;
                if want_highlight {
                    assert_eq!(got, SELECTION_BG, "({r},{c}) should be highlighted");
                } else {
                    assert_eq!(got, theme.bg, "({r},{c}) should NOT be highlighted");
                }
            }
        }
    }

    #[test]
    fn selection_contains_is_false_for_every_cell_when_nothing_is_selected() {
        for row in 0..4u16 {
            for col in 0..4u16 {
                assert!(!selection_contains(None, row, col));
            }
        }
    }

    proptest::proptest! {
        /// `selection_contains` mirrors
        /// `felis_client_core::Selection::contains`; the crates cannot
        /// depend on each other, so each is pinned against the same
        /// stated predicates.
        #[test]
        fn selection_contains_matches_its_stated_ranges(
            rectangle: bool,
            ar in 0u16..30,
            ac in 0u16..30,
            er in 0u16..30,
            ec in 0u16..30,
            r  in 0u16..30,
            c  in 0u16..30,
        ) {
            // `SelectionRange` arrives normalized (client-core's
            // `Selection::range` does it).
            let (start, end) = if rectangle {
                ((ar.min(er), ac.min(ec)), (ar.max(er), ac.max(ec)))
            } else if (ar, ac) <= (er, ec) {
                ((ar, ac), (er, ec))
            } else {
                ((er, ec), (ar, ac))
            };
            let range = SelectionRange { start, end, rectangle };

            let want = if rectangle {
                r >= start.0 && r <= end.0 && c >= start.1 && c <= end.1
            } else {
                let scalar = |(r, c): (u16, u16)| u32::from(r) * 1_000 + u32::from(c);
                scalar(start) <= scalar((r, c)) && scalar((r, c)) <= scalar(end)
            };
            proptest::prop_assert_eq!(selection_contains(Some(range), r, c), want);
        }
    }

    #[test]
    fn extend_preedit_atlas_miss_still_paints_bg_and_underline() {
        let mut out = InstanceBuffers::default();
        let overlay = PreeditOverlay {
            anchor: (0, 0),
            text: "?".to_owned(),
            cursor: None,
        };
        let atlas = MockAtlas {
            map: HashMap::new(),
            gid_map: HashMap::new(),
            fitted_map: HashMap::new(),
            clusters: HashMap::new(),
        };
        extend_preedit_instances(&overlay, metrics(), 1, 5, &atlas, &mut out);
        assert_eq!(out.bg.len(), 2, "bg + underline survive an atlas miss");
        assert!(out.fg.is_empty(), "fg pass skipped on atlas miss");
    }

    #[test]
    fn extend_search_empty_overlay_is_noop() {
        // The App can push a fresh-but-empty overlay between
        // Off and Composing without polluting the frame.
        let mut out = InstanceBuffers::default();
        let overlay = SearchOverlay::default();
        let atlas = MockAtlas::full(slot());
        extend_search_instances(&overlay, metrics(), 5, 80, true, &atlas, &mut out);
        assert_eq!(out.bg, Vec::<BgInstance>::new());
        assert_eq!(out.fg, Vec::<FgInstance>::new());
    }

    #[test]
    fn extend_search_keeps_its_highlights_when_it_loses_the_bar_row() {
        // A chord can arm a confirmation while a search is composing.
        // The question takes the row; the hit highlights are the
        // search's own rows and stay.
        let mut out = InstanceBuffers::default();
        let overlay = SearchOverlay {
            label: "search: ab".to_owned(),
            visible_hits: vec![SearchHitSpan {
                row: 2,
                col_start: 3,
                col_end: 7,
                is_current: true,
            }],
        };
        let atlas = MockAtlas::full(slot());
        extend_search_instances(&overlay, metrics(), 5, 80, false, &atlas, &mut out);
        assert_eq!(out.bg.len(), 1, "the highlight, and no bar background");
        assert_eq!(out.bg[0].color, SEARCH_CURRENT_BG);
        assert!(out.fg.is_empty(), "no label glyphs without the bar");
    }

    #[test]
    fn extend_search_paints_match_bg_for_visible_hit() {
        let mut out = InstanceBuffers::default();
        let overlay = SearchOverlay {
            label: String::new(),
            visible_hits: vec![SearchHitSpan {
                row: 2,
                col_start: 3,
                col_end: 7,
                is_current: false,
            }],
        };
        let atlas = MockAtlas::full(slot());
        extend_search_instances(&overlay, metrics(), 5, 80, true, &atlas, &mut out);
        assert_eq!(out.bg.len(), 1);
        assert_eq!(out.bg[0].origin_px, [24.0, 32.0]);
        assert_eq!(out.bg[0].size_px, [32.0, 16.0]);
        assert_eq!(out.bg[0].color, SEARCH_MATCH_BG);
    }

    #[test]
    fn extend_search_current_hit_paints_in_vivid_color() {
        let mut out = InstanceBuffers::default();
        let overlay = SearchOverlay {
            label: String::new(),
            visible_hits: vec![SearchHitSpan {
                row: 0,
                col_start: 0,
                col_end: 2,
                is_current: true,
            }],
        };
        let atlas = MockAtlas::full(slot());
        extend_search_instances(&overlay, metrics(), 5, 80, true, &atlas, &mut out);
        assert_eq!(out.bg.len(), 1);
        assert_eq!(out.bg[0].color, SEARCH_CURRENT_BG);
    }

    #[test]
    fn extend_search_clips_hit_past_grid_edges() {
        // The daemon's viewport edits and the client's hit list can race
        // during a rapid scroll; a panic there would lose a frame.
        let mut out = InstanceBuffers::default();
        let overlay = SearchOverlay {
            label: String::new(),
            visible_hits: vec![
                SearchHitSpan {
                    row: 99,
                    col_start: 0,
                    col_end: 4,
                    is_current: false,
                },
                SearchHitSpan {
                    row: 1,
                    col_start: 6,
                    col_end: 999,
                    is_current: false,
                },
            ],
        };
        let atlas = MockAtlas::full(slot());
        extend_search_instances(&overlay, metrics(), 5, 8, true, &atlas, &mut out);
        assert_eq!(out.bg.len(), 1);
        assert_eq!(out.bg[0].size_px, [16.0, 16.0]);
    }

    #[test]
    fn extend_search_paints_bar_at_bottom_row_with_label_glyphs() {
        let mut out = InstanceBuffers::default();
        let overlay = SearchOverlay {
            label: "ab".to_owned(),
            visible_hits: vec![],
        };
        let atlas = MockAtlas::full(slot());
        extend_search_instances(&overlay, metrics(), 3, 10, true, &atlas, &mut out);
        assert_eq!(out.bg.len(), 1, "1 bar bg quad");
        assert_eq!(out.bg[0].origin_px, [0.0, 32.0]);
        assert_eq!(out.bg[0].size_px, [80.0, 16.0]);
        assert_eq!(out.bg[0].color, SEARCH_BAR_BG);
        assert_eq!(out.fg.len(), 2, "1 fg quad per label char");
    }

    #[test]
    fn extend_search_label_clips_at_right_edge() {
        let mut out = InstanceBuffers::default();
        let overlay = SearchOverlay {
            label: "abcdef".to_owned(),
            visible_hits: vec![],
        };
        let atlas = MockAtlas::full(slot());
        extend_search_instances(&overlay, metrics(), 2, 3, true, &atlas, &mut out);
        assert_eq!(out.fg.len(), 3);
    }

    #[test]
    fn extend_search_zero_grid_is_noop() {
        let mut out = InstanceBuffers::default();
        let overlay = SearchOverlay {
            label: "find: x".to_owned(),
            visible_hits: vec![SearchHitSpan {
                row: 0,
                col_start: 0,
                col_end: 1,
                is_current: false,
            }],
        };
        let atlas = MockAtlas::full(slot());
        extend_search_instances(&overlay, metrics(), 0, 80, true, &atlas, &mut out);
        assert_eq!(out.bg, Vec::<BgInstance>::new());
        assert_eq!(out.fg, Vec::<FgInstance>::new());
        let mut out2 = InstanceBuffers::default();
        extend_search_instances(&overlay, metrics(), 5, 0, true, &atlas, &mut out2);
        assert_eq!(out2.bg, Vec::<BgInstance>::new());
        assert_eq!(out2.fg, Vec::<FgInstance>::new());
    }

    #[test]
    fn extend_link_preview_paints_bar_at_bottom_row_with_search_bar_color() {
        // Same neutral chrome as the search bar: a hover preview is
        // informational, not the destructive-prompt CONFIRM_BAR_BG.
        let mut out = InstanceBuffers::default();
        let overlay = LinkPreviewOverlay {
            text: "https://e".to_owned(),
        };
        let atlas = MockAtlas::full(slot());
        extend_link_preview_instances(&overlay, metrics(), 3, 10, &atlas, &mut out);
        assert_eq!(out.bg.len(), 1, "1 bar bg quad");
        assert_eq!(out.bg[0].origin_px, [0.0, 32.0]);
        assert_eq!(out.bg[0].color, SEARCH_BAR_BG);
        assert_eq!(out.fg.len(), overlay.text.chars().count());
    }

    fn img_quad(origin_y: f32, height: f32) -> ImgInstance {
        ImgInstance {
            origin_px: [0.0, origin_y],
            size_px: [32.0, height],
            uv_min: [0.0, 0.25],
            uv_max: [0.5, 0.75],
            opacity: 1.0,
            _pad: [0.0; 3],
        }
    }

    #[test]
    fn an_image_overlapping_the_chrome_row_is_cut_at_its_top_edge() {
        // A producer that paints over the link preview could show a URL
        // other than the one Ctrl+Click activates.
        let clipped = clip_img_quad_above(img_quad(0.0, 64.0), 32.0).expect("upper half survives");
        assert_eq!(clipped.size_px[1], 32.0);
        // Half the height kept, so half the source `v` span.
        assert!((clipped.uv_max[1] - 0.5).abs() < f32::EPSILON);
        assert_eq!(clipped.uv_min[1], 0.25);
    }

    #[test]
    fn a_glyph_reaching_down_into_the_chrome_row_is_cut_at_its_top_edge() {
        // An OSC 66 run scaled across rows is bottom-aligned inside its
        // block, so a producer can aim ink into the reserved row from
        // the row above; it rides the same pass as the bar's own text.
        let mut out = InstanceBuffers::default();
        out.fg.push(FgInstance {
            origin_px: [0.0, 16.0],
            size_px: [8.0, 32.0],
            uv_min: [0.0, 0.0],
            uv_max: [0.1, 0.2],
            is_color: 0.0,
            _pad: [0.0; 3],
            color: [1.0; 4],
        });
        out.fg.push(FgInstance {
            origin_px: [0.0, 34.0],
            size_px: [8.0, 10.0],
            uv_min: [0.0, 0.0],
            uv_max: [0.1, 0.2],
            is_color: 0.0,
            _pad: [0.0; 3],
            color: [1.0; 4],
        });
        clip_cell_instances_above(&mut out, 32.0);
        assert_eq!(out.fg.len(), 1, "the quad starting inside the row is gone");
        assert_eq!(out.fg[0].size_px[1], 16.0);
        // Half the height kept, so half the source `v` span.
        assert!((out.fg[0].uv_max[1] - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn an_image_entirely_inside_the_chrome_row_is_dropped() {
        assert_eq!(clip_img_quad_above(img_quad(32.0, 16.0), 32.0), None);
        assert_eq!(clip_img_quad_above(img_quad(48.0, 16.0), 32.0), None);
    }

    #[test]
    fn an_image_clear_of_the_chrome_row_is_untouched() {
        let quad = img_quad(0.0, 32.0);
        assert_eq!(clip_img_quad_above(quad, 32.0), Some(quad));
    }

    #[test]
    fn link_preview_wider_than_the_grid_ends_in_an_ellipsis() {
        // The whole point of the preview is that the user reads the true
        // target; a cut that looks like the end of the URI would be read
        // as one.
        let mut out = InstanceBuffers::default();
        let overlay = LinkPreviewOverlay {
            text: "https://example.com/very/long/path".to_owned(),
        };
        // `Renderer::render` primes the ellipsis alongside the text, so
        // the atlas answers for it here too.
        let mut atlas = MockAtlas::full(slot());
        atlas.map.insert(BAR_ELLIPSIS, slot());
        extend_link_preview_instances(&overlay, metrics(), 3, 10, &atlas, &mut out);
        assert_eq!(fit_bar_text_to_cells(&overlay.text, 10), "https://e…");
        assert_eq!(out.fg.len(), 10, "one quad per column");
        // The mark occupies the last cell, so a clip is visible rather
        // than a URI that merely stops.
        let cw = metrics().width as f32;
        let offset_x = slot().offset_px[0] as f32;
        assert_eq!(out.fg[9].origin_px[0], 9.0f32.mul_add(cw, offset_x));
    }

    #[test]
    fn link_preview_within_the_grid_is_left_verbatim() {
        assert_eq!(
            fit_bar_text_to_cells("https://e", 10),
            Cow::Borrowed("https://e")
        );
        // Exactly filling the row is not truncation.
        assert_eq!(
            fit_bar_text_to_cells("https://ex", 10),
            Cow::Borrowed("https://ex")
        );
    }

    #[test]
    fn link_preview_never_puts_half_a_wide_glyph_in_the_last_cell() {
        // A two-cell glyph with one cell left would otherwise get its
        // span clamped to one while its full quad painted past the bar.
        assert_eq!(fit_bar_text_to_cells("ab漢字", 5), "ab漢…");
        assert_eq!(fit_bar_text_to_cells("漢字漢", 5), "漢字…");
        // A single column can hold nothing but the truncation mark.
        assert_eq!(fit_bar_text_to_cells("漢字", 1), "…");
    }

    #[test]
    fn bottom_bar_claim_gives_the_row_to_search_and_confirm_over_the_preview() {
        // The preview's bg is opaque, so drawing it into a row a search
        // or confirm bar already claimed would hide that bar's text.
        for (search, confirm) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                bottom_bar_claim(ChromeBar::from_flags(search, confirm), false, true),
                BottomBarClaim::Chrome
            );
        }
    }

    #[test]
    fn bottom_bar_claim_gives_the_row_to_an_ime_composition_over_the_preview() {
        // A preedit anchored on the bottom row emits its glyphs before
        // the preview's, and every fg quad draws after every bg one, so
        // the two would overprint. The composition is the deliberate
        // interaction, so the preview yields the row entirely.
        assert_eq!(
            bottom_bar_claim(ChromeBar::None, true, true),
            BottomBarClaim::Free
        );
    }

    #[test]
    fn bottom_bar_claim_draws_the_preview_when_no_other_bar_is_open() {
        assert_eq!(
            bottom_bar_claim(ChromeBar::None, false, true),
            BottomBarClaim::LinkPreview
        );
    }

    #[test]
    fn bottom_bar_claim_reserves_nothing_with_every_bar_closed() {
        assert_eq!(
            bottom_bar_claim(ChromeBar::None, false, false),
            BottomBarClaim::Free
        );
    }

    #[test]
    fn extend_link_preview_empty_text_is_noop() {
        let mut out = InstanceBuffers::default();
        let overlay = LinkPreviewOverlay::default();
        let atlas = MockAtlas::full(slot());
        extend_link_preview_instances(&overlay, metrics(), 3, 10, &atlas, &mut out);
        assert_eq!(out.bg, Vec::<BgInstance>::new());
        assert_eq!(out.fg, Vec::<FgInstance>::new());
    }

    #[test]
    fn extend_link_preview_zero_grid_is_noop() {
        let mut out = InstanceBuffers::default();
        let overlay = LinkPreviewOverlay {
            text: "https://example.com".to_owned(),
        };
        let atlas = MockAtlas::full(slot());
        extend_link_preview_instances(&overlay, metrics(), 0, 80, &atlas, &mut out);
        assert_eq!(out.bg, Vec::<BgInstance>::new());
        assert_eq!(out.fg, Vec::<FgInstance>::new());
    }

    /// `extend_instances` at viewport=0 is byte-identical to the live
    /// path.
    #[test]
    fn extend_instances_viewport_zero_paints_same_bg_as_live_path() {
        let g = drive(2, 4, b"\x1b[?25lAB\r\nCD");
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_viewport(0, u32::from(g.rows()))
        .extend_instances(&mut buffers);
        assert_eq!(
            buffers.bg.len(),
            8,
            "viewport=0 must paint cells only — no scrollbar lane",
        );
        for inst in &buffers.bg {
            assert_ne!(inst.color, SCROLLBAR_TRACK);
            assert_ne!(inst.color, SCROLLBAR_THUMB);
        }
    }

    /// While `viewport > 0` a track and thumb paint along the right
    /// edge, track behind thumb.
    #[test]
    fn extend_instances_paints_scrollbar_lane_while_browsing() {
        let mut g = Grid::new(2, 4);
        let mut p = Parser::new();
        p.advance(&mut g, b"\x1b[?25l");
        p.advance(&mut g, b"row1\r\nrow2\r\nrow3");
        assert!(!g.scrollback().is_empty());
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_viewport(1, 3)
        .extend_instances(&mut buffers);
        assert_eq!(
            buffers.bg.len(),
            8 + 2,
            "browse mode must add a track and a thumb to the cell quads",
        );
        let last_two = &buffers.bg[buffers.bg.len() - 2..];
        assert_eq!(last_two[0].color, SCROLLBAR_TRACK, "track painted first");
        assert_eq!(last_two[1].color, SCROLLBAR_THUMB, "thumb wins on top");
        let lane_x = (g.cols() as f32).mul_add(metrics().width as f32, -SCROLLBAR_LANE_PX);
        assert_eq!(last_two[0].origin_px[0], lane_x);
        assert_eq!(last_two[1].origin_px[0], lane_x);
    }

    /// While browsing, the cursor would smear onto whichever scrollback
    /// row landed at its `(row, col)`, so it is hidden.
    #[test]
    fn extend_instances_hides_cursor_while_viewport_above_zero() {
        let mut g = Grid::new(2, 4);
        let mut p = Parser::new();
        p.advance(&mut g, b"row1\r\nrow2\r\nrow3");
        let atlas = MockAtlas::full(slot());
        let theme = Theme::default();
        let mut buffers_live = InstanceBuffers::default();
        let mut buffers_browse = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_viewport(0, 3)
        .extend_instances(&mut buffers_live);
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_viewport(1, 3)
        .extend_instances(&mut buffers_browse);
        let cursor = g.cursor();
        let idx = usize::from(cursor.row) * usize::from(g.cols()) + usize::from(cursor.col);
        assert_eq!(
            buffers_live.bg[idx].color, theme.fg,
            "live cursor paints with the swap (cursor bg = theme fg)",
        );
        assert_ne!(
            buffers_browse.bg[idx].color, theme.fg,
            "cursor must hide in browse mode — no swap on the cursor cell",
        );
    }

    fn deco_for(sgr: &[u8]) -> Vec<DecorationInstance> {
        let mut bytes = sgr.to_vec();
        bytes.push(b'A');
        let g = drive(1, 4, &bytes);
        let atlas = MockAtlas::full(slot());
        build_instances(g.screen(), metrics(), &Theme::default(), &atlas).deco
    }

    #[test]
    fn single_underline_emits_one_solid_line_in_the_descender_band() {
        let deco = deco_for(b"\x1b[4m");
        assert_eq!(deco.len(), 1);
        let d = deco[0];
        assert_eq!(d.kind, DECO_SOLID);
        assert_eq!(d.size_px[0], metrics().width as f32, "spans the cell width");
        assert_eq!(
            d.color,
            Theme::default().fg,
            "default underline color is the cell fg"
        );
        let ascent = metrics().ascent as f32;
        let ch = metrics().height as f32;
        assert!(
            d.origin_px[1] >= ascent && d.origin_px[1] <= ch - d.thickness_px,
            "underline sits in the descender band, got y={}",
            d.origin_px[1],
        );
    }

    #[test]
    fn double_underline_emits_two_stacked_lines() {
        let deco = deco_for(b"\x1b[21m");
        assert_eq!(deco.len(), 2);
        assert!(deco.iter().all(|d| d.kind == DECO_SOLID));
        assert_ne!(
            deco[0].origin_px[1], deco[1].origin_px[1],
            "two distinct rows"
        );
    }

    #[test]
    fn curly_dotted_dashed_underlines_pick_the_procedural_kinds() {
        assert_eq!(deco_for(b"\x1b[4:3m")[0].kind, DECO_CURLY);
        assert_eq!(deco_for(b"\x1b[4:4m")[0].kind, DECO_DOTTED);
        assert_eq!(deco_for(b"\x1b[4:5m")[0].kind, DECO_DASHED);
        let curly = deco_for(b"\x1b[4:3m")[0];
        assert!(
            curly.size_px[1] > curly.thickness_px,
            "curly band must exceed the thickness so the wave fits",
        );
        let dotted = deco_for(b"\x1b[4:4m")[0];
        assert_eq!(
            dotted.size_px[1], dotted.thickness_px,
            "dotted stays one line tall"
        );
        assert!(
            dotted.period_px > 0.0,
            "a dotted pattern needs a non-zero period"
        );
    }

    #[test]
    fn underline_color_uses_sgr_58_when_set() {
        let deco = deco_for(b"\x1b[4;58:2::255:0:0m");
        assert_eq!(deco.len(), 1);
        let want = crate::palette::srgb_to_linear_rgba([255, 0, 0], 1.0);
        assert_eq!(
            deco[0].color, want,
            "SGR 58 underline color must be honored"
        );
        assert_ne!(deco[0].color, Theme::default().fg);
    }

    #[test]
    fn strikethrough_emits_a_mid_cell_line_in_the_fg_color() {
        let deco = deco_for(b"\x1b[9m");
        assert_eq!(deco.len(), 1);
        let d = deco[0];
        assert_eq!(d.kind, DECO_SOLID);
        assert_eq!(d.color, Theme::default().fg);
        let ascent = metrics().ascent as f32;
        assert!(
            d.origin_px[1] < ascent,
            "strike crosses the glyph body, above the baseline"
        );
    }

    #[test]
    fn overline_emits_a_line_at_the_cell_top() {
        let deco = deco_for(b"\x1b[53m");
        assert_eq!(deco.len(), 1);
        assert_eq!(deco[0].kind, DECO_SOLID);
        assert_eq!(deco[0].origin_px[1], 0.0, "overline seats at the cell top");
        assert_eq!(deco[0].color, Theme::default().fg);
    }

    #[test]
    fn faint_dims_the_glyph_toward_the_background() {
        let theme = Theme::default();
        let atlas = MockAtlas::full(slot());
        let plain = build_instances(drive(1, 4, b"A").screen(), metrics(), &theme, &atlas);
        let faint = build_instances(drive(1, 4, b"\x1b[2mA").screen(), metrics(), &theme, &atlas);
        assert_eq!(plain.fg.len(), 1);
        assert_eq!(faint.fg.len(), 1);
        assert_eq!(
            plain.fg[0].color, theme.fg,
            "baseline glyph paints the theme fg"
        );
        assert_eq!(
            faint.fg[0].color,
            dim_toward_bg(theme.fg, theme.bg),
            "faint glyph mixes the fg toward the bg",
        );
        assert_ne!(
            faint.fg[0].color, theme.fg,
            "faint must actually change the color"
        );
    }

    #[test]
    fn conceal_hides_the_glyph_but_keeps_the_background() {
        let theme = Theme::default();
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(drive(1, 4, b"\x1b[8mA").screen(), metrics(), &theme, &atlas);
        assert!(buffers.fg.is_empty(), "concealed glyph must leave no ink");
        assert!(
            !buffers.bg.is_empty(),
            "the cell background must still paint"
        );
    }

    #[test]
    fn conceal_hides_the_glyph_yet_keeps_the_underline() {
        // Conceal suppresses the glyph, not the cell's decorations.
        let deco = deco_for(b"\x1b[4;8m");
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(
            drive(1, 4, b"\x1b[4;8mA").screen(),
            metrics(),
            &Theme::default(),
            &atlas,
        );
        assert!(buffers.fg.is_empty(), "conceal still hides the glyph");
        assert_eq!(deco.len(), 1, "the underline survives conceal");
    }

    #[test]
    fn a_link_cell_yields_the_underline_slot_but_keeps_strikethrough() {
        use std::num::NonZeroU16;
        let mut g = Grid::new(1, 4);
        let style = g.style_table_mut().intern(felis_grid::Attributes {
            flags: AttrFlags::UNDERLINE | AttrFlags::STRIKETHROUGH,
            ..felis_grid::Attributes::default()
        });
        g.set_cell(
            0,
            0,
            Cell {
                grapheme: Grapheme::Ascii(b'A'),
                style,
                link: NonZeroU16::new(1),
                sizing: None,
            },
        );
        let atlas = MockAtlas::full(slot());
        let deco = build_instances(g.screen(), metrics(), &Theme::default(), &atlas).deco;
        assert_eq!(
            deco.len(),
            1,
            "link suppresses the SGR underline but not the strike"
        );
        assert!(
            deco[0].origin_px[1] < metrics().ascent as f32,
            "the surviving decoration is the strikethrough",
        );
    }

    #[test]
    fn a_bidi_override_cell_suppresses_every_decoration() {
        let mut g = Grid::new(1, 4);
        let style = g.style_table_mut().intern(felis_grid::Attributes {
            flags: AttrFlags::UNDERLINE | AttrFlags::STRIKETHROUGH | AttrFlags::OVERLINE,
            ..felis_grid::Attributes::default()
        });
        g.set_cell(
            0,
            0,
            Cell {
                grapheme: Grapheme::Char('\u{202e}'), // RIGHT-TO-LEFT OVERRIDE
                style,
                link: None,
                sizing: None,
            },
        );
        let atlas = MockAtlas::full(slot());
        let deco = build_instances(g.screen(), metrics(), &Theme::default(), &atlas).deco;
        assert!(
            deco.is_empty(),
            "a bidi-override cell drops every decoration"
        );
    }

    proptest::proptest! {
        /// No fitted bar text overflows its budget, and a truncated one
        /// is a prefix of the original plus the mark: a wide glyph must
        /// not be half-drawn into the cell the mark needs.
        #[test]
        fn fitted_bar_text_never_overflows_its_budget(
            text in "\\PC{0,40}",
            cols in 1u16..40,
        ) {
            let fitted = fit_bar_text_to_cells(&text, cols);
            let width: u32 = felis_grid::text_cells(&fitted)
                .map(|(_, w)| u32::from(w))
                .sum();
            proptest::prop_assert!(
                width <= u32::from(cols),
                "{fitted:?} occupies {width} cells, past {cols}"
            );
            if fitted.as_ref() != text {
                let head = fitted
                    .strip_suffix(BAR_ELLIPSIS)
                    .expect("truncated text ends in the truncation mark");
                proptest::prop_assert!(text.starts_with(head));
            }
        }

        /// A clipped image quad never reaches below the row the chrome
        /// owns, keeps its origin, and samples a sub-span of its source.
        #[test]
        fn a_clipped_image_quad_stays_above_the_chrome_row(
            origin_y in -256.0f32..256.0,
            height in -16.0f32..256.0,
            max_y in -256.0f32..256.0,
        ) {
            let quad = ImgInstance {
                origin_px: [0.0, origin_y],
                size_px: [32.0, height],
                ..img_quad(origin_y, height)
            };
            match clip_img_quad_above(quad, max_y) {
                None => proptest::prop_assert!(height <= 0.0 || max_y <= origin_y),
                Some(clipped) => {
                    proptest::prop_assert!(height > 0.0 && max_y > origin_y);
                    proptest::prop_assert_eq!(clipped.origin_px, quad.origin_px);
                    proptest::prop_assert!(clipped.size_px[1] <= height);
                    // `max_y - origin_y` is the height the clip keeps;
                    // comparing the sum back against `max_y` would fail
                    // on f32 rounding alone.
                    proptest::prop_assert!(clipped.size_px[1] <= max_y - origin_y);
                    proptest::prop_assert!(clipped.uv_max[1] <= quad.uv_max[1]);
                    proptest::prop_assert!(clipped.uv_max[1] >= quad.uv_min[1]);
                }
            }
        }
    }

    /// The per-cell background quads, without the cursor or link markers
    /// pushed between them.
    fn cell_bgs(buffers: &InstanceBuffers) -> Vec<[f32; 4]> {
        let m = metrics();
        buffers
            .bg
            .iter()
            .filter(|b| b.size_px == [m.width as f32, m.height as f32])
            .map(|b| b.color)
            .collect()
    }

    fn fg_in_col(buffers: &InstanceBuffers, col: u16) -> Option<&FgInstance> {
        let cw = metrics().width as f32;
        let left = f32::from(col) * cw;
        buffers
            .fg
            .iter()
            .find(|q| q.origin_px[0] >= left && q.origin_px[0] < left + cw)
    }

    fn build_with_selection(screen: &ScreenBuffer, range: SelectionRange) -> InstanceBuffers {
        let theme = Theme::default();
        let atlas = MockAtlas::full(slot());
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            screen,
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_selection(Some(range))
        .with_viewport(0, u32::from(screen.rows()))
        .extend_instances(&mut buffers);
        buffers
    }

    #[test]
    fn block_cursor_on_a_wide_character_covers_both_halves() {
        let theme = Theme::default();
        let mut atlas = MockAtlas::full(slot());
        atlas.map.insert('字', slot());
        // Addressed onto the lead, then onto the Spacer.
        for to_col in [b"\x1b[2G", b"\x1b[3G"] {
            let g = drive(1, 6, &["a字b".as_bytes(), to_col].concat());
            let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
            let bgs = cell_bgs(&buffers);
            assert_eq!(bgs[0], theme.bg);
            assert_eq!(bgs[1], theme.fg, "lead under the cursor");
            assert_eq!(bgs[2], theme.fg, "Spacer under the cursor");
            assert_eq!(bgs[3], theme.bg);
            let glyph = fg_in_col(&buffers, 1).expect("字 draws");
            assert_eq!(glyph.color, theme.bg, "the glyph takes the swapped fg");
        }
    }

    #[test]
    fn underline_cursor_on_a_wide_character_spans_both_halves() {
        let g = drive(1, 6, "\x1b[4 qa字b\x1b[3G".as_bytes());
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let cw = metrics().width as f32;
        let xs: Vec<f32> = buffers
            .bg
            .iter()
            .filter(|b| b.size_px == [cw, CURSOR_MARKER_PX])
            .map(|b| b.origin_px[0])
            .collect();
        assert_eq!(xs, [cw, 2.0 * cw]);
    }

    #[test]
    fn bar_cursor_on_a_wide_character_sits_at_its_left_edge() {
        let g = drive(1, 6, "\x1b[6 qa字b\x1b[3G".as_bytes());
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let xs: Vec<f32> = buffers
            .bg
            .iter()
            .filter(|b| b.size_px[0] == CURSOR_MARKER_PX)
            .map(|b| b.origin_px[0])
            .collect();
        assert_eq!(xs, [metrics().width as f32]);
    }

    #[test]
    fn a_selection_touching_either_half_highlights_the_whole_character() {
        let g = drive(1, 6, "\x1b[?25la字b".as_bytes());
        let theme = Theme::default();
        let ends_on_lead = build_with_selection(
            g.screen(),
            SelectionRange {
                start: (0, 0),
                end: (0, 1),
                rectangle: false,
            },
        );
        assert_eq!(
            cell_bgs(&ends_on_lead)[..4],
            [SELECTION_BG, SELECTION_BG, SELECTION_BG, theme.bg]
        );
        let starts_on_spacer = build_with_selection(
            g.screen(),
            SelectionRange {
                start: (0, 2),
                end: (0, 3),
                rectangle: false,
            },
        );
        assert_eq!(
            cell_bgs(&starts_on_spacer)[..5],
            [theme.bg, SELECTION_BG, SELECTION_BG, SELECTION_BG, theme.bg]
        );
    }

    #[test]
    fn a_rectangle_selection_widens_per_row_to_the_characters_it_touches() {
        let g = drive(2, 4, "\x1b[?25la字b\r\n字cd".as_bytes());
        let theme = Theme::default();
        let buffers = build_with_selection(
            g.screen(),
            SelectionRange {
                start: (0, 1),
                end: (1, 1),
                rectangle: true,
            },
        );
        let bgs = cell_bgs(&buffers);
        assert_eq!(bgs[..4], [theme.bg, SELECTION_BG, SELECTION_BG, theme.bg]);
        assert_eq!(bgs[4..], [SELECTION_BG, SELECTION_BG, theme.bg, theme.bg]);
    }

    #[test]
    fn a_wide_cluster_carrying_a_bidi_override_is_marked_on_both_halves() {
        let g = drive(1, 6, "\x1b[?25la字\u{202E}b".as_bytes());
        let atlas = MockAtlas::full(slot());
        let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
        let bgs = cell_bgs(&buffers);
        assert_eq!(bgs[1], BIDI_MARKER_BG);
        assert_eq!(bgs[2], BIDI_MARKER_BG);
        assert_ne!(bgs[3], BIDI_MARKER_BG);
    }

    #[test]
    fn a_preedit_over_the_spacer_hides_the_lead_glyph_but_keeps_its_background() {
        let g = drive(1, 6, "\x1b[?25la\x1b[41m字\x1b[mb".as_bytes());
        let theme = Theme::default();
        let atlas = MockAtlas::full(slot());
        let mut buffers = InstanceBuffers::default();
        CellPainter::new(
            g.screen(),
            metrics(),
            &ResolvedTheme::new(&theme),
            &atlas,
            &crate::glyphs::ShapeFrame::empty(),
        )
        .with_cursor_visible(true)
        .with_viewport(0, u32::from(g.rows()))
        .with_preedit(Some(PreeditSpan {
            row: 0,
            col_start: 2,
            col_end: 4,
        }))
        .extend_instances(&mut buffers);
        assert!(fg_in_col(&buffers, 0).is_some(), "a draws");
        assert!(
            fg_in_col(&buffers, 1).is_none(),
            "字 would ink under the overlay"
        );
        let cw = metrics().width as f32;
        assert!(
            buffers
                .bg
                .iter()
                .any(|b| b.origin_px[0] == cw && b.size_px[0] == cw && b.color != theme.bg),
            "the lead keeps its red background"
        );
    }

    fn preedit(text: &str, col: u16) -> PreeditOverlay {
        PreeditOverlay {
            anchor: (0, col),
            text: text.to_owned(),
            cursor: None,
        }
    }

    #[test]
    fn preedit_gives_a_cluster_the_cells_the_grid_gives_it() {
        for (text, cells) in [
            ("e\u{301}x", 2),
            ("❤\u{FE0F}", 2),
            ("👍🏻", 2),
            ("🇯🇵", 2),
            ("a字", 3),
        ] {
            let span = preedit_covered_cols(&preedit(text, 1), 20).expect("non-empty");
            assert_eq!(span.col_end - span.col_start, cells, "{text:?}");
        }
    }

    #[test]
    fn preedit_draws_a_cluster_with_its_shaped_glyphs() {
        let mut atlas = MockAtlas::full(slot());
        for (id, flag) in [(1, "🇯🇵"), (2, "🇯🇲")] {
            atlas.clusters.insert(
                flag.to_owned(),
                vec![crate::glyphs::ClusterGlyph {
                    glyph_id: id,
                    font_id: 0,
                    advance_px: 16.0,
                    x_offset_px: 0.0,
                    y_offset_px: 0.0,
                }],
            );
            atlas.gid_map.insert(
                id,
                GlyphSlot {
                    uv_min: [f32::from(id) / 10.0, 0.0],
                    ..slot()
                },
            );
        }
        let uv_of = |text: &str| {
            let mut out = InstanceBuffers::default();
            extend_preedit_instances(&preedit(text, 0), metrics(), 1, 10, &atlas, &mut out);
            assert_eq!(out.fg.len(), 1, "{text:?} draws one shaped glyph");
            out.fg[0].uv_min
        };
        assert_ne!(uv_of("🇯🇵"), uv_of("🇯🇲"));
    }

    #[test]
    fn overlay_glyphs_never_draw_past_the_right_edge() {
        let mut atlas = MockAtlas::full(slot());
        let wide = GlyphSlot {
            size_px: [14, 10],
            ..slot()
        };
        atlas.map.insert('字', wide);
        let cols = 5;
        let edge = f32::from(cols) * metrics().width as f32;
        let mut out = InstanceBuffers::default();
        extend_preedit_instances(&preedit("ab字", 2), metrics(), 1, cols, &atlas, &mut out);
        extend_search_instances(
            &SearchOverlay {
                label: "abcd字".to_owned(),
                visible_hits: Vec::new(),
            },
            metrics(),
            2,
            cols,
            true,
            &atlas,
            &mut out,
        );
        assert_eq!(
            out.fg.len(),
            3 + 5,
            "each clipped 字 keeps its visible half"
        );
        for q in &out.fg {
            assert!(q.origin_px[0] + q.size_px[0] <= edge, "{q:?}");
        }
    }
}

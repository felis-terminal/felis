//! The cell painter: turns the screen and the frame's overlays into
//! the display list of [`crate::display_list`].

use std::borrow::Cow;

use felis_grid::{
    Cell, CursorStyle, Grapheme, HAlign, ScreenBuffer, Sizing, UnderlineStyle, VAlign,
    char_cell_width,
};
use felis_shaping::{CellMetrics, SizingKey};

pub use crate::appearance::{CursorPaint, PreeditSpan};
use crate::{
    SelectionRange,
    appearance::{
        CellAppearance, CursorMarkerKind, Disposition, FrameState, RowCells, RowFrame, resolve_cell,
    },
    contrast::relative_luminance,
    display_list::{AtlasKind, DecoKind, Decoration, GlyphDraw, Rect, RowPaint},
    glyphs::{
        AtlasView, ClusterGlyph, GlyphSlot, ShapeFrame, ShapedCell, cluster_base_char,
        cluster_block_cols, font_style_of, sizing_key_from,
    },
    overlay::{ConfirmOverlay, LinkPreviewOverlay, PreeditOverlay, SearchOverlay},
    palette::{
        CONFIRM_BAR_BG, PREEDIT_BG, ResolvedTheme, SCROLLBAR_THUMB, SCROLLBAR_TRACK, SEARCH_BAR_BG,
        SEARCH_CURRENT_BG, SEARCH_MATCH_BG,
    },
};

pub const CURSOR_MARKER_PX: f32 = 2.0;

/// One pixel less than the cursor marker so a cursor underline on a
/// link cell still visually wins.
const LINK_UNDERLINE_PX: f32 = 1.0;

const PREEDIT_UNDERLINE_PX: f32 = 2.0;

/// The Mozc / fcitx / iBus convention: a thicker bar under the segment
/// being edited.
const PREEDIT_ACTIVE_UNDERLINE_PX: f32 = 4.0;

const SCROLLBAR_LANE_PX: f32 = 6.0;

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

/// `(x, y, width, height)` in pixels of the cursor the cell pass paints
/// over the columns `span` covers.
#[must_use]
pub fn cursor_rect_px(
    metrics: CellMetrics,
    origin_px: [f32; 2],
    row: u16,
    (first, last): (u16, u16),
    style: CursorStyle,
) -> [f32; 4] {
    let (cw, ch) = (metrics.width as f32, metrics.height as f32);
    let x = f32::from(first).mul_add(cw, origin_px[0]);
    let y = f32::from(row).mul_add(ch, origin_px[1]);
    let w = f32::from(last - first + 1) * cw;
    match style {
        CursorStyle::Block => [x, y, w, ch],
        CursorStyle::Underline => [x, y + ch - CURSOR_MARKER_PX, w, CURSOR_MARKER_PX],
        CursorStyle::Bar => [x, y, CURSOR_MARKER_PX, ch],
    }
}

struct CellRef<'a> {
    row: u16,
    col: u16,
    cell: &'a Cell,
    attrs: &'a felis_grid::Attributes,
    cells: &'a RowCells<'a>,
    frame: &'a RowFrame,
}

pub struct CellPainter<'a, A: AtlasView> {
    screen: &'a ScreenBuffer,
    theme: &'a ResolvedTheme,
    atlas: &'a A,
    shape_frame: &'a ShapeFrame,
    metrics: CellMetrics,
    cw: f32,
    ch: f32,
    ascent: f32,
    cursor_visible: bool,
    viewport: u32,
    viewport_max: u32,
    frame: FrameState,
    compact: bool,
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
        shape_frame: &'a ShapeFrame,
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
            viewport: 0,
            viewport_max: 0,
            frame: FrameState {
                cursor: CursorPaint::default(),
                selection: None,
                screen_reverse: false,
                preedit: None,
                reserved_bottom_rows: 0,
                rows: screen.rows(),
            },
            compact: true,
        }
    }

    /// One rect per painted cell, the reference the compacted rects must
    /// match pixel for pixel.
    #[cfg(test)]
    pub(crate) const fn without_compaction(mut self) -> Self {
        self.compact = false;
        self
    }

    /// `?5` DECSCNM as the daemon reports it, xor'd against each cell's
    /// SGR 7.
    #[must_use]
    pub const fn with_reverse_video(mut self, on: bool) -> Self {
        self.frame.screen_reverse = on;
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
        self.frame.selection = selection;
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
        self.frame.reserved_bottom_rows = rows;
        self
    }

    /// The span (from [`preedit_covered_cols`]) whose cells are skipped
    /// so the underlying glyph does not draw through the opaque overlay
    /// in the later fg pass.
    #[must_use]
    pub const fn with_preedit(mut self, span: Option<PreeditSpan>) -> Self {
        self.frame.preedit = span;
        self
    }

    /// Appends into caller-owned scratch (kept across frames so the
    /// steady state allocates nothing); callers clear first for a fresh
    /// frame. An atlas miss skips only the fg quad; the bg still lands.
    pub fn extend_screen(self, out: &mut RowPaint) {
        let painter = self.resolved();
        let cells = usize::from(painter.screen.rows()) * usize::from(painter.screen.cols());
        out.rects.reserve(cells);
        out.glyphs.reserve(cells);
        for r in 0..painter.screen.rows() {
            painter.extend_row(out, r);
        }
        painter.push_scrollbar_lane(out);
    }

    /// Fixes the cursor paint, which every row reads.
    #[must_use]
    pub fn resolved(mut self) -> Self {
        self.frame = FrameState::for_screen(
            self.screen,
            self.cursor_visible,
            self.viewport,
            self.frame.selection,
            self.frame.screen_reverse,
            self.frame.preedit,
            self.frame.reserved_bottom_rows,
        );
        self
    }

    /// Resolved by [`Self::resolved`]; meaningless before.
    #[must_use]
    pub const fn cursor(&self) -> CursorPaint {
        self.frame.cursor
    }

    /// Everything a row emits depends on that row's cells and the
    /// painter's frame-wide inputs alone, which is what lets
    /// a renderer's row cache repaint a row in isolation.
    pub fn extend_row(&self, out: &mut RowPaint, r: u16) {
        // Straight off the shadow's screen: the shadow has no
        // scrollback ring, so when viewport > 0 the daemon ships
        // composed RowDeltas against composed-view rows. Composing
        // here (`cell_at_viewport`) would fall back to live cells.
        let Some(cells) = RowCells::of_screen(self.screen, r) else {
            return;
        };
        let frame = self.frame.row(r, &cells);
        // Cell draws must not land under the bottom chrome: a glyph
        // rides the fg pass and would draw on top of the bar. Capping the
        // walk is the only fix; there is no fg occlusion across passes.
        if !frame.painted {
            return;
        }
        for c in 0..cells.cols {
            let look = resolve_cell(&cells, &frame, self.theme, c);
            if look.disposition == Disposition::Skip {
                continue;
            }
            let cell = cells.cell(c);
            let at = CellRef {
                row: r,
                col: c,
                cell,
                attrs: cells.styles.resolve(cell.style),
                cells: &cells,
                frame: &frame,
            };
            self.push_cell_chrome(out, &at, &look);
            // CONCEAL (SGR 8). SECURITY: this must gate every fg push.
            // The bg / cursor / selection / decoration draws above
            // stay (a concealed cell reads as a space).
            if !look.glyph_visible {
                continue;
            }
            // Shape-frame lookup precedes char-path decoding so a
            // `Trailing` cell skips its fg entirely.
            let shaped = self.shape_frame.cell_at(r, c);
            if matches!(shaped, ShapedCell::Trailing) {
                continue;
            }
            let ink = self.ink(look.fg, look.bg);
            if let ShapedCell::Cluster { start, len, fit_px } = shaped {
                self.push_cluster_cell(out, &at, ink, (start, len), fit_px);
                continue;
            }
            self.push_char_cell(out, &at, ink, shaped);
        }
    }

    /// Judged against the straight background, so a translucent window
    /// weights text as it would over its configured, opaque color.
    fn ink(&self, color: [f32; 4], bg: [f32; 4]) -> Ink {
        Ink {
            color,
            ground_luminance: relative_luminance(self.theme.contrast_ground(bg)),
        }
    }

    fn origin(&self, at: &CellRef<'_>) -> [f32; 2] {
        [f32::from(at.col) * self.cw, f32::from(at.row) * self.ch]
    }

    /// A ground cell's quad would rewrite the clear color the pass starts
    /// with, as the first bg-tier draw over its pixels. A cell narrower
    /// than [`CURSOR_MARKER_PX`] lets a bar marker spill into the next
    /// cell, whose quad erases the spill.
    #[allow(
        clippy::float_cmp,
        reason = "identity with the clear value the pass is cleared to, not a numeric closeness test"
    )]
    fn draws_bg(&self, look: &CellAppearance) -> bool {
        !(self.compact
            && look.bg_is_ground
            && self.cw >= CURSOR_MARKER_PX
            && look.bg == self.theme.clear_color(self.frame.screen_reverse))
    }

    fn push_rect(&self, out: &mut RowPaint, rect: Rect) {
        if self.compact {
            out.push_rect_merged(rect);
        } else {
            out.rects.push(rect);
        }
    }

    fn push_cell_chrome(&self, out: &mut RowPaint, at: &CellRef<'_>, look: &CellAppearance) {
        let origin = self.origin(at);
        if self.draws_bg(look) {
            self.push_rect(
                out,
                Rect {
                    origin_px: origin,
                    size_px: [self.cw, self.ch],
                    color: look.bg,
                },
            );
        }
        // Before the cursor marker so a cursor underline on a link cell
        // wins.
        if let Some(color) = look.link_underline {
            self.push_rect(
                out,
                Rect {
                    origin_px: [origin[0], origin[1] + self.ch - LINK_UNDERLINE_PX],
                    size_px: [self.cw, LINK_UNDERLINE_PX],
                    color,
                },
            );
        }
        // Pushed after the cell's bg quad so it draws over the bg but
        // under the glyph.
        match look.cursor_marker {
            Some((CursorMarkerKind::Underline, color)) => self.push_rect(
                out,
                Rect {
                    origin_px: [origin[0], origin[1] + self.ch - CURSOR_MARKER_PX],
                    size_px: [self.cw, CURSOR_MARKER_PX],
                    color,
                },
            ),
            Some((CursorMarkerKind::Bar, color)) => self.push_rect(
                out,
                Rect {
                    origin_px: origin,
                    size_px: [CURSOR_MARKER_PX, self.ch],
                    color,
                },
            ),
            None => {}
        }
        self.push_cell_decorations(&mut out.decos, at, look);
    }

    /// One draw per shaped glyph at the cell origin plus pen advance
    /// and GPOS offset; a single-glyph cluster places identically to the
    /// equivalent `Char` cell.
    fn push_cluster_cell(
        &self,
        out: &mut RowPaint,
        at: &CellRef<'_>,
        ink: Ink,
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
        let scaled_ascent = self
            .metrics
            .baseline(glyph_scale, sizing_scale_for_layout(sizing));
        let pen = ClusterPen {
            glyphs: self.shape_frame.cluster_slice(start, len),
            base_sizing,
            fit_px,
            glyph_scale,
            origin: [0.0, scaled_ascent],
            ink,
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
            &mut out.glyphs,
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
        let layout_scale = sizing_scale_for_layout(sizing);
        let block_cols = cluster_block_cols(self.screen, at.row, at.col);
        let span_pad = self.span_pad(f32::from(block_cols), layout_scale);
        if !fitted
            && matches!(sizing.halign(), HAlign::Left)
            && matches!(sizing.valign(), VAlign::Top)
        {
            return [span_pad, 0.0];
        }
        let Some([ink_x0, ink_y0, ink_x1, ink_y1]) =
            cluster_ink_box(self.atlas, self.metrics.height, pen)
        else {
            return [0.0, 0.0];
        };
        let ink_w = ink_x1 - ink_x0;
        let ink_h = ink_y1 - ink_y0;
        let block_w = self.cw * f32::from(block_cols) * layout_scale;
        let block_h = self.ch * layout_scale;
        let centre_x = (block_w - ink_w) / 2.0 - ink_x0;
        let centre_y = (block_h - ink_h) / 2.0 - ink_y0;
        let [inset_right, inset_bottom] = self.content_insets(layout_scale);
        let shift_x = match sizing.halign() {
            HAlign::Left if fitted => centre_x,
            HAlign::Left => span_pad,
            HAlign::Right => block_w - ink_x1 - inset_right,
            HAlign::Center => centre_x,
        };
        let shift_y = match sizing.valign() {
            VAlign::Top if fitted => centre_y,
            VAlign::Top => 0.0,
            VAlign::Bottom => block_h - ink_y1 - inset_bottom,
            VAlign::Center => centre_y,
        };
        [shift_x, shift_y]
    }

    /// The slot carries one cell's `pad`; a glyph spanning more cells sits
    /// in their joint glyph box, which every cell widens by `pad`.
    fn span_pad(&self, cells: f32, layout_scale: f32) -> f32 {
        (cells - 1.0).max(0.0) * self.metrics.pad[0] as f32 * layout_scale
    }

    /// The OSC 66 block's right and bottom padding: never negative, so an
    /// aligned glyph stays inside its block when a scale shrinks cells.
    fn content_insets(&self, layout_scale: f32) -> [f32; 2] {
        self.metrics
            .trailing_pad()
            .map(|pad| pad.max(0) as f32 * layout_scale)
    }

    fn push_char_cell(&self, out: &mut RowPaint, at: &CellRef<'_>, ink: Ink, shaped: ShapedCell) {
        let shape_primary = match shaped {
            ShapedCell::Primary {
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
            Grapheme::Cluster(id) => match cluster_base_char(self.screen, *id) {
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
        // the glyph was rasterized at and scales the baseline.
        let layout_scale = sizing_scale_for_layout(sizing);
        let glyph_scale = sizing_glyph_scale(sizing);
        let scaled_ascent = self.metrics.baseline(glyph_scale, layout_scale);
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
        let span = shape_primary.map_or(override_w, |(_, span_cols)| f32::from(span_cols));
        let (span_pad, [inset_right, inset_bottom]) = if slot.fills_cell {
            (0.0, [0.0, 0.0])
        } else {
            (
                self.span_pad(span, layout_scale),
                self.content_insets(layout_scale),
            )
        };
        // `Top` / `Left` are the spec defaults and must reproduce the
        // baseline + left-bearing math exactly.
        let h_offset = match sizing.halign() {
            HAlign::Left => slot.offset_px[0] as f32 + span_pad,
            HAlign::Right => block_w - bitmap_w - inset_right,
            HAlign::Center => (block_w - bitmap_w) / 2.0,
        };
        let v_offset = match sizing.valign() {
            VAlign::Top => scaled_ascent - slot.offset_px[1] as f32,
            VAlign::Bottom => block_h - bitmap_h - inset_bottom,
            VAlign::Center => (block_h - bitmap_h) / 2.0,
        };
        let origin = self.origin(at);
        let draw_origin = [origin[0] + h_offset, origin[1] + v_offset];
        let quad = glyph_draw(slot, draw_origin, ink);
        // Shape runs do not split at color changes (`next_shape_run`),
        // so a glyph inking across cells is sliced per cell with each
        // strip's own color (kitty's per-cell sprite behavior). Color
        // bitmaps skip this: the shader ignores their tint.
        if shape_primary.is_some() && !slot.is_color {
            self.push_sliced_by_cell(out, quad, at);
        } else {
            out.glyphs.push(quad);
        }
    }

    /// Absent at the live bottom. Gated on `viewport_max > rows` as
    /// defense-in-depth: the daemon should already have clamped.
    pub fn push_scrollbar_lane(&self, out: &mut RowPaint) {
        let rows_u32 = u32::from(self.screen.rows());
        if self.viewport > 0 && self.viewport_max > rows_u32 {
            let max_scroll = (self.viewport_max - rows_u32) as f32;
            let total_w = f32::from(self.screen.cols()) * self.cw;
            let total_h = f32::from(self.screen.rows()) * self.ch;
            let lane_x = total_w - SCROLLBAR_LANE_PX;
            // Track first so the thumb wins at overlap.
            out.rects.push(Rect {
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
            out.rects.push(Rect {
                origin_px: [lane_x, thumb_top],
                size_px: [SCROLLBAR_LANE_PX, thumb_h],
                color: SCROLLBAR_THUMB,
            });
        }
    }

    /// Underlines sit in the descender band, strikethrough at ~x-height,
    /// overline at the top edge.
    fn push_cell_decorations(
        &self,
        out: &mut Vec<Decoration>,
        at: &CellRef<'_>,
        look: &CellAppearance,
    ) {
        let glyph_h = self.metrics.glyph_box[1] as f32;
        let glyph_top = self.metrics.pad[1] as f32;
        let glyph_ascent = self.metrics.glyph_ascent();
        let thickness = decoration_thickness(glyph_h);
        let origin = self.origin(at);
        let ox = origin[0];
        let oy = origin[1];
        let cw = self.cw;
        let ground_luminance = self.ink(look.fg, look.bg).ground_luminance;
        let solid = |out: &mut Vec<Decoration>, top: f32, color: [f32; 4]| {
            out.push(Decoration {
                origin_px: [ox, top.clamp(oy, oy + self.ch - thickness)],
                size_px: [cw, thickness],
                color,
                kind: DecoKind::Solid,
                thickness_px: thickness,
                period_px: 0.0,
                ground_luminance,
            });
        };

        if let Some((style, color)) = look.underline {
            // Clamped so a font whose ascent nearly fills the cell keeps
            // the line on-cell.
            let descent_band = (glyph_h - glyph_ascent).max(thickness);
            let underline_top = (oy + self.ascent + (descent_band * 0.3).round().max(1.0))
                .min(oy + self.ch - thickness)
                .max(oy);
            match style {
                UnderlineStyle::Single => solid(out, underline_top, color),
                UnderlineStyle::Double => {
                    solid(out, underline_top, color);
                    solid(out, thickness.mul_add(-2.0, underline_top), color);
                }
                UnderlineStyle::Dotted => out.push(Decoration {
                    origin_px: [ox, underline_top],
                    size_px: [cw, thickness],
                    color,
                    kind: DecoKind::Dotted,
                    thickness_px: thickness,
                    period_px: (thickness * 3.0).max(3.0),
                    ground_luminance,
                }),
                UnderlineStyle::Dashed => out.push(Decoration {
                    origin_px: [ox, underline_top],
                    size_px: [cw, thickness],
                    color,
                    kind: DecoKind::Dashed,
                    thickness_px: thickness,
                    period_px: (cw * 0.75).max(6.0),
                    ground_luminance,
                }),
                UnderlineStyle::Curly => {
                    // Taller band so the sine can swing ± its amplitude.
                    let amplitude = thickness;
                    let band_h = amplitude.mul_add(2.0, thickness);
                    let center = thickness.mul_add(0.5, underline_top);
                    let band_top = band_h
                        .mul_add(-0.5, center)
                        .clamp(oy, oy + self.ch - band_h);
                    out.push(Decoration {
                        origin_px: [ox, band_top],
                        size_px: [cw, band_h],
                        color,
                        kind: DecoKind::Curly,
                        thickness_px: thickness,
                        period_px: cw.max(thickness * 4.0),
                        ground_luminance,
                    });
                }
            }
        }
        if let Some(color) = look.strikethrough {
            // 0.6 of the ascent reads as through the middle of most glyphs.
            let strike_top = thickness.mul_add(-0.5, oy + glyph_top + (glyph_ascent * 0.6).round());
            solid(out, strike_top, color);
        }
        if let Some(color) = look.overline {
            solid(out, oy + glyph_top, color);
        }
    }

    /// Shape runs do not end at color boundaries (`next_shape_run`), and
    /// one draw carries one tint and one ground, so a glyph covering cells
    /// of different fg or bg is split into per-cell strips (kitty's
    /// per-cell sprites). A uniform run emits the original quad.
    fn push_sliced_by_cell(&self, out: &mut RowPaint, quad: GlyphDraw, at: &CellRef<'_>) {
        let x0 = quad.origin_px[0];
        let x1 = x0 + quad.size_px[0];
        if quad.size_px[0] <= 0.0 {
            out.glyphs.push(quad);
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
            out.glyphs.push(quad);
            return;
        }
        let ink_at = |col: u16| -> Ink {
            if col == at.col {
                return Ink {
                    color: quad.color,
                    ground_luminance: quad.ground_luminance,
                };
            }
            let look = resolve_cell(at.cells, at.frame, self.theme, col);
            self.ink(look.fg, look.bg)
        };
        // Both sides come from the same palette lookups, so equal
        // colors are equal bit patterns; an epsilon would blur distinct
        // palette entries.
        #[allow(clippy::float_cmp)]
        let uniform = (col_first..=col_last).all(|col| {
            let ink = ink_at(col);
            ink.color == quad.color && ink.ground_luminance == quad.ground_luminance
        });
        if uniform {
            out.glyphs.push(quad);
            return;
        }
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
            let ink = ink_at(col);
            let mut strip = GlyphDraw {
                origin_px: [sx0, quad.origin_px[1]],
                size_px: [sx1 - sx0, quad.size_px[1]],
                color: ink.color,
                ground_luminance: ink.ground_luminance,
                ..quad
            };
            strip.crop_src_x(t0, t1);
            out.glyphs.push(strip);
        }
    }
}

/// The fg pass runs after the bg pass, so a cell's glyph can only be
/// hidden by not emitting it. The walk mirrors
/// `extend_preedit` so the skipped span matches the painted
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
pub fn extend_preedit<A: AtlasView>(
    overlay: &PreeditOverlay,
    metrics: CellMetrics,
    grid_rows: u16,
    grid_cols: u16,
    atlas: &A,
    out: &mut RowPaint,
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
    let first_glyph = out.glyphs.len();
    for cell in overlay_cells(&overlay.text, start, grid_cols) {
        let origin = [f32::from(cell.col) * cw, f32::from(row) * ch];
        let span_px = [cw * f32::from(cell.width), ch];
        let underline_px =
            if active_range.is_some_and(|(s, e)| cell.bytes.start < e && cell.bytes.end > s) {
                PREEDIT_ACTIVE_UNDERLINE_PX
            } else {
                PREEDIT_UNDERLINE_PX
            };
        out.rects.push(Rect {
            origin_px: origin,
            size_px: span_px,
            color: PREEDIT_BG,
        });
        out.rects.push(Rect {
            origin_px: [origin[0], origin[1] + ch - underline_px],
            size_px: [span_px[0], underline_px],
            color: theme_default_fg(),
        });
        push_overlay_glyphs(
            atlas,
            metrics,
            (&overlay.text[cell.bytes], cell.width),
            (origin, overlay_ink(PREEDIT_BG)),
            &mut out.glyphs,
        );
    }
    clip_fg_right_of(&mut out.glyphs, first_glyph, f32::from(grid_cols) * cw);
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
    (cluster, cols): (&str, u8),
    (origin, ink): ([f32; 2], Ink),
    out: &mut Vec<GlyphDraw>,
) {
    let ascent = f32::from(u16::try_from(metrics.ascent).unwrap_or(u16::MAX));
    let span_pad = f32::from(cols.saturating_sub(1)) * metrics.pad[0] as f32;
    let origin = [origin[0] + span_pad, origin[1]];
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
                ink,
            },
            out,
        );
        return;
    }
    if let Some(slot) = atlas.slot(base, metrics.height, SizingKey::default()) {
        out.push(glyph_draw(
            slot,
            [
                origin[0] + slot.offset_px[0] as f32,
                origin[1] + ascent - slot.offset_px[1] as f32,
            ],
            ink,
        ));
    }
}

/// A shaped cluster's glyphs and where its pen starts: `origin` is the
/// left edge on the baseline. A non-zero `fit_px` reads the glyphs'
/// fitted slots.
#[derive(Clone, Copy)]
struct ClusterPen<'g> {
    glyphs: &'g [ClusterGlyph],
    base_sizing: SizingKey,
    fit_px: u16,
    glyph_scale: f32,
    origin: [f32; 2],
    ink: Ink,
}

/// A coverage draw's tint and the luminance of what it is drawn over.
#[derive(Clone, Copy)]
struct Ink {
    color: [f32; 4],
    ground_luminance: f32,
}

/// Overlay text is the fixed [`theme_default_fg`] on the overlay's own
/// background, which is always opaque.
fn overlay_ink(bg: [f32; 4]) -> Ink {
    Ink {
        color: theme_default_fg(),
        ground_luminance: relative_luminance(bg),
    }
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
            let key = pen.base_sizing.with_font_id(usize::from(cg.font_id));
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
                // The shaper reports GPOS y in font space (y-up); screen space
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

const fn glyph_draw(slot: GlyphSlot, origin_px: [f32; 2], ink: Ink) -> GlyphDraw {
    let [x, y] = slot.atlas_px;
    let [w, h] = slot.size_px;
    GlyphDraw {
        origin_px,
        size_px: [w as f32, h as f32],
        atlas: if slot.is_color {
            AtlasKind::Color
        } else {
            AtlasKind::Mono
        },
        src_min_px: [x as f32, y as f32],
        src_max_px: [(x + w) as f32, (y + h) as f32],
        color: ink.color,
        ground_luminance: ink.ground_luminance,
    }
}

fn push_cluster_glyphs<A: AtlasView>(
    atlas: &A,
    cell_height: u32,
    pen: ClusterPen<'_>,
    out: &mut Vec<GlyphDraw>,
) {
    out.extend(
        cluster_glyph_quads(atlas, cell_height, pen)
            .map(|(slot, origin_px)| glyph_draw(slot, origin_px, pen.ink)),
    );
}

/// Hard-coded rather than `theme.fg` so the overlay stays high-contrast
/// against `PREEDIT_BG` whatever the theme.
const fn theme_default_fg() -> [f32; 4] {
    [1.0, 1.0, 1.0, 1.0]
}

/// Appended after the cell pass so highlights and bar paint on top.
/// Atlas misses in the label are skipped; the renderer primes the
/// label before painting. Highlights on the bar row are still
/// emitted but covered by the bar.
pub fn extend_search<A: AtlasView>(
    overlay: &SearchOverlay,
    metrics: CellMetrics,
    grid_rows: u16,
    grid_cols: u16,
    draw_bar: bool,
    atlas: &A,
    out: &mut RowPaint,
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
        out.rects.push(Rect {
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

/// Same emission contract as [`extend_search`]'s bar step.
pub fn extend_confirm<A: AtlasView>(
    overlay: &ConfirmOverlay,
    metrics: CellMetrics,
    grid_rows: u16,
    grid_cols: u16,
    atlas: &A,
    out: &mut RowPaint,
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

/// Same emission contract as [`extend_search`]'s bar step.
/// Reuses [`SEARCH_BAR_BG`] rather than [`CONFIRM_BAR_BG`]: the preview
/// is informational, not a destructive prompt, so it must not read like
/// one.
pub fn extend_link_preview<A: AtlasView>(
    overlay: &LinkPreviewOverlay,
    metrics: CellMetrics,
    grid_rows: u16,
    grid_cols: u16,
    atlas: &A,
    out: &mut RowPaint,
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
pub fn clip_cells_above(out: &mut RowPaint, max_y: f32) {
    out.rects.retain_mut(|quad| {
        clip_extent_at(quad.origin_px[1], &mut quad.size_px[1], max_y).is_some()
    });
    out.glyphs.retain_mut(|quad| {
        let Some(kept) = clip_extent_at(quad.origin_px[1], &mut quad.size_px[1], max_y) else {
            return false;
        };
        quad.crop_src_y(0.0, kept);
        true
    });
    out.decos.retain_mut(|quad| {
        clip_extent_at(quad.origin_px[1], &mut quad.size_px[1], max_y).is_some()
    });
}

/// Cuts the glyph quads from `from` on at `max_x`, the window's right
/// edge, so overlay text never draws past it.
fn clip_fg_right_of(quads: &mut Vec<GlyphDraw>, from: usize, max_x: f32) {
    let mut i = from;
    while i < quads.len() {
        let quad = &mut quads[i];
        let Some(kept) = clip_extent_at(quad.origin_px[0], &mut quad.size_px[0], max_x) else {
            quads.remove(i);
            continue;
        };
        quad.crop_src_x(0.0, kept);
        i += 1;
    }
}

/// Shrinks `extent`, along one axis, so the quad ends at `max`,
/// reporting the fraction of it that survived, or `None` when nothing
/// does.
#[must_use]
pub fn clip_extent_at(origin: f32, extent: &mut f32, max: f32) -> Option<f32> {
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
    out: &mut RowPaint,
) {
    let cw = metrics.width as f32;
    let ch = metrics.height as f32;
    let bar_row = grid_rows - 1;
    let bar_origin_y = f32::from(bar_row) * ch;
    out.rects.push(Rect {
        origin_px: [0.0, bar_origin_y],
        size_px: [f32::from(grid_cols) * cw, ch],
        color: bar_bg,
    });

    // Whitespace advances the column without an fg emission, like the
    // cell pass's atlas-miss handling.
    let first_glyph = out.glyphs.len();
    for cell in overlay_cells(label, 0, grid_cols) {
        let origin = [f32::from(cell.col) * cw, bar_origin_y];
        push_overlay_glyphs(
            atlas,
            metrics,
            (&label[cell.bytes], cell.width),
            (origin, overlay_ink(bar_bg)),
            &mut out.glyphs,
        );
    }
    clip_fg_right_of(&mut out.glyphs, first_glyph, f32::from(grid_cols) * cw);
}

/// One px at a ~16 px cell, two at ~32 px.
fn decoration_thickness(ch: f32) -> f32 {
    (ch / 16.0).round().max(1.0)
}

#[cfg(test)]
mod tests;

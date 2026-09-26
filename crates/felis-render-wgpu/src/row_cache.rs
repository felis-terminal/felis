//! Cell instances kept across frames in fixed-stride row slots, so a
//! frame repaints and uploads only the rows the shadow marked dirty.

use crate::{
    SelectionRange,
    buffer_ring::RowUpdates,
    instances::{
        AtlasView, BgInstance, CellPainter, CursorPaint, DecorationInstance, FgInstance,
        InstanceBuffers, PreeditSpan, clip_cell_instances_above,
    },
};
use bytemuck::Zeroable;

/// The frame-wide inputs every row's instances depend on. The theme is
/// not here: its setters call [`RowCache::invalidate`] instead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameKey {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) metrics: [u32; 3],
    pub(crate) viewport: u32,
    pub(crate) reserved_bottom_rows: u16,
    pub(crate) preedit: Option<PreeditSpan>,
    pub(crate) selection: Option<SelectionRange>,
    pub(crate) reverse_video: bool,
    /// An empty shape frame and a built one need not agree on a cell.
    pub(crate) shaped: bool,
    /// Bumped by every glyph atlas recycle, which moves every slot.
    pub(crate) atlas_generation: u64,
}

const BG: usize = size_of::<BgInstance>();
const FG: usize = size_of::<FgInstance>();
const DECO: usize = size_of::<DecorationInstance>();

/// Padding any grid may carry regardless of how sparse its rows are.
const PADDING_ALLOWANCE: usize = 16 << 20;

#[derive(Default)]
pub(crate) struct RowCache {
    /// Half the device's buffer size limit bounds a padded layer, since
    /// the ring rounds a buffer's capacity up to a power of two.
    max_buffer_bytes: usize,
    key: Option<FrameKey>,
    cursor: CursorPaint,
    /// Instances per row slot, for bg, fg, deco. Unused slots hold
    /// zero-sized quads, which rasterize nothing.
    stride: [usize; 3],
    pub(crate) out: InstanceBuffers,
    row_version: Vec<u64>,
    version: u64,
    epoch: u64,
    row: InstanceBuffers,
    flat: InstanceBuffers,
    ends: Vec<[usize; 3]>,
    /// Set when the rows fit slots far wider than they need.
    relayout: bool,
}

impl RowCache {
    pub(crate) fn new(max_buffer_bytes: u64) -> Self {
        Self {
            max_buffer_bytes: usize::try_from(max_buffer_bytes).unwrap_or(usize::MAX),
            ..Self::default()
        }
    }

    pub(crate) const fn invalidate(&mut self) {
        self.key = None;
    }

    /// Repaints `dirty` plus the rows the cursor left and entered, or every
    /// row when `key` changed or a row outgrew its slot, then appends the
    /// scrollbar lane. The caller appends the overlays after that.
    pub(crate) fn paint<A: AtlasView>(
        &mut self,
        painter: &CellPainter<'_, A>,
        key: FrameKey,
        dirty: impl Iterator<Item = usize>,
        clip_max_y: Option<f32>,
    ) -> &mut InstanceBuffers {
        self.version += 1;
        let cursor = painter.cursor;
        let rows = usize::from(key.rows);
        let reusable = self.key == Some(key) && self.row_version.len() == rows;
        let fits = reusable && {
            self.truncate_to_rows();
            let moved = (self.cursor != cursor).then_some([self.cursor.cell, cursor.cell]);
            let cursor_rows = moved.into_iter().flatten().flatten();
            dirty
                .chain(cursor_rows.map(|(r, _)| usize::from(r)))
                .filter(|&r| r < rows)
                .all(|r| self.repaint_row(painter, r, clip_max_y))
        };
        if !fits {
            self.repaint_all(painter, key, clip_max_y);
        }
        let mut lane = std::mem::take(&mut self.row);
        lane.bg.clear();
        lane.fg.clear();
        lane.deco.clear();
        painter.push_scrollbar_lane(&mut lane);
        if let Some(max_y) = clip_max_y {
            clip_cell_instances_above(&mut lane, max_y);
        }
        self.out.bg.extend_from_slice(&lane.bg);
        self.row = lane;
        self.key = Some(key);
        self.cursor = cursor;
        &mut self.out
    }

    pub(crate) fn updates(&self, instance_bytes: usize, layer: usize) -> RowUpdates<'_> {
        RowUpdates {
            epoch: self.epoch,
            version: self.version,
            row_version: &self.row_version,
            row_bytes: self.stride[layer] * instance_bytes,
        }
    }

    fn truncate_to_rows(&mut self) {
        let rows = self.row_version.len();
        self.out.bg.truncate(rows * self.stride[0]);
        self.out.fg.truncate(rows * self.stride[1]);
        self.out.deco.truncate(rows * self.stride[2]);
    }

    fn paint_row_scratch<A: AtlasView>(
        row: &mut InstanceBuffers,
        painter: &CellPainter<'_, A>,
        r: usize,
        clip_max_y: Option<f32>,
    ) {
        row.bg.clear();
        row.fg.clear();
        row.deco.clear();
        painter.extend_row(row, r as u16);
        if let Some(max_y) = clip_max_y {
            clip_cell_instances_above(row, max_y);
        }
    }

    fn repaint_row<A: AtlasView>(
        &mut self,
        painter: &CellPainter<'_, A>,
        r: usize,
        clip_max_y: Option<f32>,
    ) -> bool {
        Self::paint_row_scratch(&mut self.row, painter, r, clip_max_y);
        let [bg, fg, deco] = self.stride;
        if self.row.bg.len() > bg || self.row.fg.len() > fg || self.row.deco.len() > deco {
            return false;
        }
        place(&mut self.out.bg[r * bg..(r + 1) * bg], &self.row.bg);
        place(&mut self.out.fg[r * fg..(r + 1) * fg], &self.row.fg);
        place(&mut self.out.deco[r * deco..(r + 1) * deco], &self.row.deco);
        self.row_version[r] = self.version;
        true
    }

    /// Keeps the slot layout when every row still fits it, so a frame that
    /// changes every row (a scroll) costs one walk and one copy per row.
    fn repaint_all<A: AtlasView>(
        &mut self,
        painter: &CellPainter<'_, A>,
        key: FrameKey,
        clip_max_y: Option<f32>,
    ) {
        let rows = usize::from(key.rows);
        if !self.relayout && self.row_version.len() == rows {
            self.truncate_to_rows();
            let mut widest = [0usize; 3];
            let fits = (0..rows).all(|r| {
                let fits = self.repaint_row(painter, r, clip_max_y);
                widest = widen(widest, &self.row);
                fits
            });
            if fits {
                self.relayout = (0..3).any(|l| slot_width(widest[l]) * 2 < self.stride[l]);
                return;
            }
        }
        self.relayout = false;
        let mut flat = std::mem::take(&mut self.flat);
        flat.bg.clear();
        flat.fg.clear();
        flat.deco.clear();
        self.ends.clear();
        let mut widest = [0usize; 3];
        for r in 0..rows {
            Self::paint_row_scratch(&mut self.row, painter, r, clip_max_y);
            widest = widen(widest, &self.row);
            flat.bg.extend_from_slice(&self.row.bg);
            flat.fg.extend_from_slice(&self.row.fg);
            flat.deco.extend_from_slice(&self.row.deco);
            self.ends
                .push([flat.bg.len(), flat.fg.len(), flat.deco.len()]);
        }
        for (stride, widest) in self.stride.iter_mut().zip(widest) {
            // Doubling a slot a row outgrew keeps a line being typed from
            // relaying out every few keystrokes.
            *stride = if *stride > 0 && widest > *stride {
                widest * 2
            } else {
                slot_width(widest)
            };
        }
        let [bg, fg, deco] = self.stride;
        let padded = [bg * BG, fg * FG, deco * DECO].map(|b| b * rows);
        let packed = [
            flat.bg.len() * BG,
            flat.fg.len() * FG,
            flat.deco.len() * DECO,
        ];
        if padded.iter().zip(packed).any(|(&padded, packed)| {
            padded > self.max_buffer_bytes / 2 || padded > (packed * 4).max(PADDING_ALLOWANCE)
        }) {
            // One wide row pads every other row to its width; past these
            // bounds the frame keeps the packed rows and repaints them all
            // until the rows even out.
            std::mem::swap(&mut self.out, &mut flat);
            self.flat = flat;
            self.row_version.clear();
            self.epoch += 1;
            return;
        }
        scatter(&mut self.out.bg, &flat.bg, &self.ends, 0, bg);
        scatter(&mut self.out.fg, &flat.fg, &self.ends, 1, fg);
        scatter(&mut self.out.deco, &flat.deco, &self.ends, 2, deco);
        self.flat = flat;
        self.row_version.clear();
        self.row_version.resize(rows, self.version);
        self.epoch += 1;
    }
}

fn widen(widest: [usize; 3], row: &InstanceBuffers) -> [usize; 3] {
    [
        widest[0].max(row.bg.len()),
        widest[1].max(row.fg.len()),
        widest[2].max(row.deco.len()),
    ]
}

/// Headroom past the widest row, so a row that grows by a few glyphs
/// does not send the next frame back through [`RowCache::repaint_all`];
/// kept small because every frame that repaints all rows uploads the
/// padding too.
const fn slot_width(widest: usize) -> usize {
    widest + widest / 8 + 8
}

/// Lays packed rows, ending at `ends[..][layer]`, into `stride`-wide slots.
fn scatter<T: Copy + Zeroable>(
    out: &mut Vec<T>,
    flat: &[T],
    ends: &[[usize; 3]],
    layer: usize,
    stride: usize,
) {
    out.resize(ends.len() * stride, T::zeroed());
    let mut start = 0;
    for (slot, end) in out.chunks_exact_mut(stride).zip(ends) {
        place(slot, &flat[start..end[layer]]);
        start = end[layer];
    }
}

fn place<T: Copy + Zeroable>(slot: &mut [T], row: &[T]) {
    let (used, spare) = slot.split_at_mut(row.len());
    used.copy_from_slice(row);
    spare.fill(T::zeroed());
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use felis_grid::{
        AttrFlags, Attributes, Cell, Color, CursorStyle, Grapheme, HAlign, ScreenBuffer,
        ScrollDirection, Sizing, TableGc, UnderlineStyle, VAlign,
    };
    use felis_shaping::{CellMetrics, GlyphId, SizingKey};
    use proptest::prelude::*;

    use super::*;
    use crate::{
        glyphs::ShapeFrame,
        instances::GlyphSlot,
        palette::{ResolvedTheme, Theme},
    };

    /// Every printable char has a slot whose uv depends on the atlas
    /// generation, so a cached row that outlived a recycle shows up.
    struct Atlas(u64);

    impl AtlasView for Atlas {
        fn slot(&self, glyph: char, _: u32, _: SizingKey) -> Option<GlyphSlot> {
            (glyph.is_ascii_graphic()).then(|| GlyphSlot {
                uv_min: [f32::from(glyph as u8) / 128.0, self.0 as f32],
                uv_max: [1.0, 1.0],
                size_px: [6, 10],
                offset_px: [1, 10],
                is_color: false,
            })
        }

        fn glyph_id_slot(&self, _: GlyphId, _: u32, _: SizingKey) -> Option<GlyphSlot> {
            None
        }
    }

    /// Row and column bounds exceed the grid so out-of-range writes and
    /// cursor positions are exercised too.
    #[derive(Debug, Clone)]
    enum Op {
        /// Row, text, style, link.
        WriteRow(u16, Vec<u8>, u8, bool),
        /// Row, col, byte, style.
        SetCell(u16, u16, u8, u8),
        /// Top, bottom, count, up.
        Scroll(u16, u16, u16, bool),
        Resize(u16, u16),
        /// Row, col, visible, style.
        Cursor(u16, u16, bool, u8),
        Focus(bool),
        Selection(Option<SelectionRange>),
        ReservedRows(u16),
        Preedit(Option<PreeditSpan>),
        Viewport(u32),
        ReverseVideo(bool),
        FontSize(u32),
        Theme(u8),
        AtlasReset,
        Reconnect,
        GcStyles,
        /// Row, col, OSC 66 scale.
        Sized(u16, u16, u8),
    }

    fn op() -> impl Strategy<Value = Op> {
        let text = proptest::collection::vec(0x20u8..0x7f, 0..20);
        let span = (0u16..12, 0u16..20, 0u16..20);
        let sel = ((0u16..12, 0u16..20), (0u16..12, 0u16..20), any::<bool>());
        prop_oneof![
            6 => (0u16..12, text, 0u8..6, any::<bool>()).prop_map(|(r, t, s, l)| Op::WriteRow(r, t, s, l)),
            6 => (0u16..12, 0u16..20, 0x20u8..0x7f, 0u8..6).prop_map(|(r, c, b, s)| Op::SetCell(r, c, b, s)),
            2 => (0u16..12, 0u16..12, 1u16..4, any::<bool>()).prop_map(|(t, b, n, u)| Op::Scroll(t, b, n, u)),
            1 => (1u16..12, 1u16..20).prop_map(|(r, c)| Op::Resize(r, c)),
            6 => (0u16..12, 0u16..20, any::<bool>(), 0u8..3).prop_map(|(r, c, v, s)| Op::Cursor(r, c, v, s)),
            2 => any::<bool>().prop_map(Op::Focus),
            2 => proptest::option::of(sel).prop_map(|o| Op::Selection(o.map(|(a, b, rectangle)| {
                SelectionRange { start: a.min(b), end: a.max(b), rectangle }
            }))),
            1 => (0u16..2).prop_map(Op::ReservedRows),
            1 => proptest::option::of(span).prop_map(|o| Op::Preedit(o.map(|(row, a, b)| {
                PreeditSpan { row, col_start: a.min(b), col_end: a.max(b) }
            }))),
            1 => (0u32..3).prop_map(Op::Viewport),
            1 => any::<bool>().prop_map(Op::ReverseVideo),
            1 => (14u32..18).prop_map(Op::FontSize),
            1 => (0u8..3).prop_map(Op::Theme),
            1 => Just(Op::AtlasReset),
            1 => Just(Op::Reconnect),
            1 => Just(Op::GcStyles),
            2 => (0u16..12, 0u16..20, 1u8..4).prop_map(|(r, c, s)| Op::Sized(r, c, s)),
        ]
    }

    fn attrs(style: u8) -> Attributes {
        let decorated = AttrFlags::UNDERLINE | AttrFlags::STRIKETHROUGH | AttrFlags::OVERLINE;
        let flags = [
            AttrFlags::UNDERLINE,
            decorated,
            AttrFlags::REVERSE,
            AttrFlags::empty(),
        ];
        Attributes {
            fg: Color::Indexed(style),
            bg: if style == 4 {
                Color::Rgb(10, 20, 30)
            } else {
                Color::Default
            },
            flags: flags[usize::from(style % 4)],
            underline_style: UnderlineStyle::Curly,
            ..Attributes::default()
        }
    }

    /// The renderer's paint-time state, mirrored with the same
    /// invalidation rules `Renderer::build_cell_instances` applies.
    struct Harness {
        screen: ScreenBuffer,
        theme: Theme,
        height: u32,
        focused: bool,
        selection: Option<SelectionRange>,
        reserved: u16,
        preedit: Option<PreeditSpan>,
        viewport: u32,
        reverse: bool,
        atlas: u64,
        cache: RowCache,
    }

    impl Harness {
        fn new(max_buffer_bytes: u64) -> Self {
            Self {
                screen: ScreenBuffer::with_scrollback(6, 10, 0),
                theme: Theme::default(),
                height: 16,
                focused: true,
                selection: None,
                reserved: 0,
                preedit: None,
                viewport: 0,
                reverse: false,
                atlas: 0,
                cache: RowCache::new(max_buffer_bytes),
            }
        }

        fn apply(&mut self, op: Op) {
            let (rows, cols) = (self.screen.rows(), self.screen.cols());
            match op {
                Op::WriteRow(row, text, style, link) => {
                    let style = self.screen.style_table_mut().intern(attrs(style));
                    let cells: Vec<Cell> = (0..usize::from(cols))
                        .map(|c| Cell {
                            grapheme: text.get(c).map_or(Grapheme::Empty, |b| Grapheme::Ascii(*b)),
                            style,
                            link: link.then_some(NonZeroU16::MIN),
                            sizing: None,
                        })
                        .collect();
                    self.screen.write_row_cells(row, &cells);
                }
                Op::SetCell(row, col, byte, style) => {
                    let style = self.screen.style_table_mut().intern(attrs(style));
                    let grapheme = Grapheme::Ascii(byte);
                    let cell = Cell {
                        grapheme,
                        style,
                        ..Cell::default()
                    };
                    self.screen.set_cell(row, col, cell);
                }
                Op::Scroll(top, bottom, n, up) => {
                    let dir = [ScrollDirection::Down, ScrollDirection::Up][usize::from(up)];
                    let _ = self.screen.apply_scroll_directive(top, bottom, n, dir);
                }
                Op::Resize(rows, cols) => self.screen.resize(rows, cols),
                Op::Cursor(row, col, visible, style) => {
                    let styles = [CursorStyle::Block, CursorStyle::Underline, CursorStyle::Bar];
                    let style = styles[usize::from(style)];
                    let _ = self
                        .screen
                        .set_cursor_state(row, col, visible, style, false);
                }
                Op::Focus(focused) => self.focused = focused,
                Op::Selection(sel) => self.selection = sel,
                Op::ReservedRows(n) => self.reserved = n,
                Op::Preedit(span) => self.preedit = span,
                Op::Viewport(v) => self.viewport = v,
                Op::ReverseVideo(on) => self.reverse = on,
                Op::FontSize(h) => self.height = h,
                Op::Theme(i) => {
                    self.theme.fg = [f32::from(i) / 3.0, 0.5, 0.5, 1.0];
                    self.cache.invalidate();
                }
                Op::AtlasReset => self.atlas += 1,
                Op::Reconnect => self.screen = ScreenBuffer::with_scrollback(rows, cols, 0),
                Op::GcStyles => {
                    TableGc::new().sweep(&mut self.screen);
                }
                Op::Sized(row, col, scale) => {
                    let sizing = Sizing::new(scale, 0, 0, 0, VAlign::Center, HAlign::Center);
                    let handle = sizing.and_then(|s| self.screen.install_sizing(s));
                    self.screen.set_cell_sizing(row, col, handle);
                }
            }
        }

        /// Paints through the cache and returns what it holds with the
        /// empty slots dropped, next to a from-scratch walk.
        fn frame(&mut self) -> (InstanceBuffers, InstanceBuffers) {
            let mut cache = std::mem::take(&mut self.cache);
            let theme = ResolvedTheme::new(&self.theme);
            let atlas = Atlas(self.atlas);
            let frame = ShapeFrame::empty();
            let (height, rows) = (self.height, self.screen.rows());
            let metrics = CellMetrics {
                width: 8,
                height,
                ascent: height - 4,
            };
            let painter = CellPainter::new(&self.screen, metrics, &theme, &atlas, &frame)
                .with_cursor_visible(self.focused)
                .with_selection(self.selection)
                .with_viewport(self.viewport, u32::from(rows) + 3)
                .with_reserved_bottom_rows(self.reserved)
                .with_preedit(self.preedit)
                .with_reverse_video(self.reverse)
                .resolved();
            let clip = (self.reserved > 0)
                .then(|| f32::from(rows.saturating_sub(self.reserved)) * height as f32);
            let key = FrameKey {
                rows,
                cols: self.screen.cols(),
                metrics: [8, height, height - 4],
                viewport: self.viewport,
                reserved_bottom_rows: self.reserved,
                preedit: self.preedit,
                selection: self.selection,
                reverse_video: self.reverse,
                shaped: false,
                atlas_generation: self.atlas,
            };
            let dirty = self.screen.damage().dirty_rows();
            let cached = cache.paint(&painter, key, dirty, clip);
            let got = InstanceBuffers {
                bg: occupied(&cached.bg),
                fg: occupied(&cached.fg),
                deco: occupied(&cached.deco),
            };
            let mut want = InstanceBuffers::default();
            for r in 0..rows {
                painter.extend_row(&mut want, r);
            }
            painter.push_scrollbar_lane(&mut want);
            if let Some(max_y) = clip {
                clip_cell_instances_above(&mut want, max_y);
            }
            self.cache = cache;
            self.screen.clear_damage();
            (got, want)
        }
    }

    fn occupied<T: Copy + PartialEq + Zeroable>(slots: &[T]) -> Vec<T> {
        slots
            .iter()
            .copied()
            .filter(|q| *q != T::zeroed())
            .collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Every input the key or an invalidation must catch (see `Op`),
        /// under a buffer limit that fits padded rows and one that forces
        /// packed ones. A frame applies up to three ops, so a scroll
        /// directive can land on a band rows were already written to.
        #[test]
        fn incremental_frames_match_a_full_rebuild(
            frames in proptest::collection::vec(proptest::collection::vec(op(), 1..=3), 1..30),
            max_buffer_bytes in prop_oneof![Just(u64::MAX), 1u64..8192],
        ) {
            let mut h = Harness::new(max_buffer_bytes);
            let (got, want) = h.frame();
            prop_assert_eq!(got, want);
            for ops in frames {
                for op in ops {
                    h.apply(op);
                }
                let (got, want) = h.frame();
                prop_assert_eq!(got, want);
            }
        }
    }

    #[test]
    fn a_one_cell_write_repaints_only_its_row() {
        let mut h = Harness::new(u64::MAX);
        h.frame();
        let epoch = h.cache.epoch;
        h.apply(Op::SetCell(3, 2, b'x', 0));
        h.frame();
        assert_eq!(h.cache.epoch, epoch);
        let changed = h.cache.row_version.iter().map(|&v| v == h.cache.version);
        assert!(changed.eq([false, false, false, true, false, false]));
    }

    #[test]
    fn rows_padded_past_half_the_buffer_limit_stay_packed() {
        let mut h = Harness::new(4096);
        h.apply(Op::WriteRow(0, b"wide".to_vec(), 0, false));
        let (got, _) = h.frame();
        assert_eq!(h.cache.row_version.len(), 0);
        assert_eq!(h.cache.out.bg.len(), got.bg.len());
    }
}

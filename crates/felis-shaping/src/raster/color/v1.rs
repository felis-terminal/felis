//! `COLR` version 1 glyphs painted into premultiplied RGBA8 canvases.
//!
//! Every buffer sized by the pixel size is charged against
//! [`coverage::IMAGE_BUDGET`] before allocation. A paint this painter
//! lacks, a refused charge, or a malformed graph drops the glyph whole.

use skrifa::{
    GlyphId as FontGlyphId,
    font::glyph::{
        color::{
            Brush, ColorGlyph, ColorGlyphVersion, ColorStop, CompositeMode, Extend, PaintSink,
        },
        outline::{DrawMemory, DrawOptions},
    },
    raw::types::{BoundingBox, Matrix},
};

use super::{
    Canvas, FOREGROUND, FOREGROUND_INDEX,
    composite::{self, Conical, Ramp},
    div255,
};
use crate::{
    Font, GlyphBitmap, GlyphId, GlyphPixels,
    raster::{PathRecorder, SkrifaOutlines, coverage, coverage::PathEl, coverage::Point},
};

/// `None` when the face has no version 1 paint for `glyph`, or the paint
/// cannot be drawn whole; the next source is then tried.
pub(super) fn render(
    outlines: &mut SkrifaOutlines,
    font: &Font,
    palette: &[skrifa::color::Color],
    glyph: GlyphId,
    px: f32,
) -> Option<GlyphBitmap> {
    let face = &font.face;
    let color_glyph = face
        .color_glyphs()
        .get_with_version(FontGlyphId::new(glyph), ColorGlyphVersion::ColrV1)?;
    let scale = px / f32::from(face.units_per_em().max(1));
    let base = Matrix::from_elements([scale, 0.0, 0.0, scale, 0.0, 0.0]);
    let Some([left, top, right, bottom]) = canvas_box(&color_glyph, base)? else {
        return Some(GlyphBitmap::new(0, 0, 0, 0, GlyphPixels::Rgba(Vec::new())));
    };
    coverage::reserve_segments(&mut outlines.segments);
    let mut budget = Budget(coverage::IMAGE_BUDGET);
    let root = super::canvas([left, top, right, bottom], px, budget.0)?;
    budget.charge(root.data.len())?;
    let (width, height) = (root.width, root.height);
    let mut path = std::mem::take(&mut outlines.path);
    let mut painter = Painter {
        face,
        palette,
        px,
        path: &mut path,
        segments: &mut outlines.segments,
        budget,
        width,
        height,
        left,
        top,
        transforms: vec![base],
        clips: vec![Clip {
            rect: [0.0, 0.0, width as f32, height as f32],
            mask: None,
            mask_rect: [0.0; 4],
            bounds: [0, 0, width, height],
        }],
        masks: Vec::new(),
        layers: vec![Layer {
            data: root.data,
            mode: CompositeMode::SrcOver,
            ink: None,
        }],
        failed: false,
    };
    let painted = color_glyph.paint(&mut painter);
    let Painter {
        failed,
        mut layers,
        clips,
        ..
    } = painter;
    outlines.path = path;
    if painted.is_err() || failed || layers.len() != 1 || clips.len() != 1 {
        return None;
    }
    let data = layers.pop()?.data;
    Some(
        Canvas {
            width,
            height,
            left,
            top,
            data,
        }
        .into_bitmap(),
    )
}

/// The canvas `glyph` paints into under `base`: its clip box as painting
/// clips to it, else the bounds of what it paints. The extents a face
/// reports round a varied clip box's deltas to whole units, which could
/// leave a fraction of the clip outside the canvas.
#[expect(
    clippy::option_option,
    reason = "an outer None refuses the glyph, an inner None draws nothing"
)]
fn canvas_box(glyph: &ColorGlyph<'_>, base: Matrix<f32>) -> Option<Option<[i32; 4]>> {
    let font_box = if let Some(clip) = glyph.clip_box() {
        clip
    } else {
        let e = glyph.extents().ok()?;
        BoundingBox {
            x_min: e.x_bearing,
            y_min: e.y_bearing - e.height,
            x_max: e.x_bearing + e.width,
            y_max: e.y_bearing,
        }
    };
    pixel_box(base, font_box)
}

/// The whole pixels `font_box` covers under `m`, as `[left, top, right,
/// bottom]` with y up; `Some(None)` when it covers none, `None` when it is
/// not finite.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the canvas side limit rejects a box this far out before it is allocated"
)]
#[expect(
    clippy::option_option,
    reason = "an outer None refuses the glyph, an inner None draws nothing"
)]
fn pixel_box(m: Matrix<f32>, font_box: BoundingBox<f32>) -> Option<Option<[i32; 4]>> {
    let corners = [
        [font_box.x_min, font_box.y_min],
        [font_box.x_max, font_box.y_min],
        [font_box.x_min, font_box.y_max],
        [font_box.x_max, font_box.y_max],
    ]
    .map(|[x, y]| m.transform(x, y));
    let fold = |f: fn(f32, f32) -> f32, axis: fn(&(f32, f32)) -> f32| {
        corners.iter().map(axis).reduce(f).unwrap_or(0.0)
    };
    let (x0, x1) = (fold(f32::min, |c| c.0), fold(f32::max, |c| c.0));
    let (y0, y1) = (fold(f32::min, |c| c.1), fold(f32::max, |c| c.1));
    if ![x0, x1, y0, y1].iter().all(|v| v.is_finite()) {
        return None;
    }
    let clamp = |v: f32| v.clamp(i32::MIN as f32 / 2.0, i32::MAX as f32 / 2.0) as i32;
    let (left, right) = (clamp(x0.floor()), clamp(x1.ceil()));
    let (bottom, top) = (clamp(y0.floor()), clamp(y1.ceil()));
    Some((left < right && bottom < top).then_some([left, top, right, bottom]))
}

/// Bytes left of the raster budget.
struct Budget(usize);

impl Budget {
    fn charge(&mut self, bytes: usize) -> Option<()> {
        self.0 = self.0.checked_sub(bytes)?;
        Some(())
    }

    const fn credit(&mut self, bytes: usize) {
        self.0 += bytes;
    }
}

/// The clip in canvas pixels, y down: the axis-aligned `rect` times the
/// A8 coverage `masks[mask]`, which holds the clip as it stood inside
/// `mask_rect`, all within the whole-pixel `bounds` (`[left, top, right,
/// bottom]`, exclusive).
#[derive(Clone, Copy)]
struct Clip {
    rect: [f32; 4],
    mask: Option<usize>,
    mask_rect: [f32; 4],
    bounds: [usize; 4],
}

/// A canvas the size of the root, composited onto the one below with
/// `mode` when popped. `ink` bounds every pixel painted into it.
struct Layer {
    data: Vec<u8>,
    mode: CompositeMode,
    ink: Option<[usize; 4]>,
}

/// What a fill paints at a point of the canvas, in pixels with y up.
enum Shader {
    Solid([u8; 4]),
    Linear {
        inverse: Matrix<f32>,
        p0: [f32; 2],
        d: [f32; 2],
        inv_len2: f32,
        ramp: Box<Ramp>,
    },
    Radial {
        inverse: Matrix<f32>,
        conical: Conical,
        ramp: Box<Ramp>,
    },
}

impl Shader {
    fn at(&self, x: f32, y: f32) -> Option<[u8; 4]> {
        match self {
            Self::Solid(color) => Some(*color),
            Self::Linear {
                inverse,
                p0,
                d,
                inv_len2,
                ramp,
            } => {
                let p = inverse.transform(x, y).into();
                Some(ramp.sample(composite::linear_t(p, *p0, *d, *inv_len2)))
            }
            Self::Radial {
                inverse,
                conical,
                ramp,
            } => conical
                .t(inverse.transform(x, y).into())
                .map(|t| ramp.sample(t)),
        }
    }
}

struct Painter<'a> {
    face: &'a skrifa::Font,
    palette: &'a [skrifa::font::glyph::color::Color],
    px: f32,
    path: &'a mut PathRecorder,
    segments: &'a mut Vec<[f32; 4]>,
    budget: Budget,
    width: usize,
    height: usize,
    left: i32,
    top: i32,
    transforms: Vec<Matrix<f32>>,
    clips: Vec<Clip>,
    masks: Vec<Vec<u8>>,
    layers: Vec<Layer>,
    /// Set by the first paint that cannot be drawn whole; every later
    /// callback is ignored, and the glyph dropped.
    failed: bool,
}

impl Painter<'_> {
    fn ctm(&self) -> Matrix<f32> {
        self.transforms.last().copied().unwrap_or_default()
    }

    fn clip(&self) -> &Clip {
        &self.clips[self.clips.len() - 1]
    }

    /// The straight color of a palette entry times `alpha`, in 0..=1.
    fn color(&self, palette_index: u16, alpha: f32) -> [f32; 4] {
        let [r, g, b, a] = if palette_index == FOREGROUND_INDEX {
            FOREGROUND
        } else {
            self.palette
                .get(usize::from(palette_index))
                .map_or(FOREGROUND, |c| [c.red, c.green, c.blue, c.alpha])
        };
        let unit = |c: u8| f32::from(c) / 255.0;
        [unit(r), unit(g), unit(b), (unit(a) * alpha).clamp(0.0, 1.0)]
    }

    /// The shader for `brush` mapped into the canvas by `m`; `None` for a
    /// brush this painter does not draw, `Some(None)` for one that paints
    /// nothing.
    #[expect(
        clippy::option_option,
        reason = "an outer None refuses the glyph, an inner None draws nothing"
    )]
    fn shader(&self, brush: &Brush<'_>, m: Matrix<f32>) -> Option<Option<Shader>> {
        let ramp = |stops: &[ColorStop], extend: Extend| -> Box<Ramp> {
            let stops: Vec<(f32, [f32; 4])> = stops
                .iter()
                .map(|s| (s.offset, self.color(s.palette_index, s.alpha)))
                .collect();
            Box::new(Ramp::new(&stops, extend))
        };
        Some(match *brush {
            Brush::Solid {
                palette_index,
                alpha,
            } => {
                let [red, green, blue, opacity] = self.color(palette_index, alpha);
                let unit = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
                Some(Shader::Solid([
                    unit(red * opacity),
                    unit(green * opacity),
                    unit(blue * opacity),
                    unit(opacity),
                ]))
            }
            Brush::LinearGradient {
                p0,
                p1,
                color_stops,
                extend,
            } => {
                let ramp = ramp(color_stops, extend);
                let d = [p1.x - p0.x, p1.y - p0.y];
                let len2 = d[0].mul_add(d[0], d[1] * d[1]);
                inverse(m)
                    .filter(|_| len2 > 0.0)
                    .map(|inverse| Shader::Linear {
                        inverse,
                        p0: [p0.x, p0.y],
                        d,
                        inv_len2: 1.0 / len2,
                        ramp,
                    })
            }
            Brush::RadialGradient {
                c0,
                r0,
                c1,
                r1,
                color_stops,
                extend,
            } => {
                let ramp = ramp(color_stops, extend);
                inverse(m).map(|inverse| Shader::Radial {
                    inverse,
                    conical: Conical {
                        c0: [c0.x, c0.y],
                        r0,
                        cd: [c1.x - c0.x, c1.y - c0.y],
                        dr: r1 - r0,
                    },
                    ramp,
                })
            }
            Brush::SweepGradient { .. } => return None,
        })
    }

    /// The outline of `glyph` under the current transform, cut to the
    /// current clip; `None` over a raster limit or the budget.
    fn outline(&mut self, glyph: FontGlyphId) -> Option<coverage::Mask> {
        let m = self.ctm();
        self.path.0.clear();
        let mut sink = Transformed {
            m,
            path: &mut self.path.0,
        };
        let drawn = self
            .face
            .outline_glyphs()
            .get(glyph)
            .is_some_and(|outline| {
                outline
                    .draw(&DrawOptions::new(), DrawMemory::Scratch, &mut sink)
                    .is_ok()
            });
        if !drawn {
            return Some(coverage::Mask::default());
        }
        self.rasterize()
    }

    /// The outline in `self.path` cut to the clip's rectangle, which is
    /// then already applied to it.
    fn rasterize(&mut self) -> Option<coverage::Mask> {
        let Clip { rect, bounds, .. } = *self.clip();
        let (left, top) = (self.left as f32, self.top as f32);
        let window = [
            left + rect[0].max(bounds[0] as f32),
            top - rect[1].max(bounds[1] as f32),
            left + rect[2].min(bounds[2] as f32),
            top - rect[3].min(bounds[3] as f32),
        ];
        coverage::rasterize_clipped(&self.path.0, self.px, self.segments, self.budget.0, window)
    }

    /// Paints `shader` source-over into the top layer where the clip and
    /// `shape` (an A8 mask over the canvas, or the whole canvas) cover.
    fn paint(&mut self, shader: &Shader, shape: Option<&coverage::Mask>) {
        let clip = *self.clip();
        let mut bounds = clip.bounds;
        if let Some(mask) = shape {
            let Some(mask_bounds) = self.mask_bounds(mask) else {
                return;
            };
            bounds = intersect(bounds, mask_bounds);
        }
        let [c0, r0, c1, r1] = bounds;
        if c0 >= c1 || r0 >= r1 {
            return;
        }
        let (width, left, top) = (self.width, self.left, self.top);
        let Some(layer) = self.layers.last_mut() else {
            return;
        };
        for r in r0..r1 {
            let y = top as f32 - r as f32 - 0.5;
            for c in c0..c1 {
                let cover = match shape {
                    Some(mask) => div255(
                        clip_cover(&clip, &self.masks, width, c, r, true)
                            * u32::from(mask_at(mask, left, top, c, r)),
                    ),
                    None => clip_cover(&clip, &self.masks, width, c, r, false),
                };
                if cover == 0 {
                    continue;
                }
                let x = left as f32 + c as f32 + 0.5;
                let Some(color) = shader.at(x, y) else {
                    continue;
                };
                let source = color.map(|v| narrow(div255(u32::from(v) * cover)));
                let pixel = &mut layer.data[(r * width + c) * 4..][..4];
                pixel.copy_from_slice(&composite::src_over(source, to4(pixel)));
            }
        }
        layer.ink = Some(layer.ink.map_or(bounds, |ink| union(ink, bounds)));
    }

    /// The canvas pixels `mask` covers, clipped to the canvas.
    fn mask_bounds(&self, mask: &coverage::Mask) -> Option<[usize; 4]> {
        if mask.width == 0 || mask.height == 0 {
            return None;
        }
        let c0 = i64::from(mask.left) - i64::from(self.left);
        let r0 = i64::from(self.top) - i64::from(mask.top);
        let clamp = |v: i64, max: usize| usize::try_from(v.max(0)).unwrap_or(0).min(max);
        Some([
            clamp(c0, self.width),
            clamp(r0, self.height),
            clamp(c0 + i64::from(mask.width), self.width),
            clamp(r0 + i64::from(mask.height), self.height),
        ])
    }

    const fn fail(&mut self) {
        self.failed = true;
    }
}

/// `mask`'s coverage of canvas pixel `(c, r)`, for a canvas whose first
/// row's top left corner is `(left, top)`.
fn mask_at(mask: &coverage::Mask, left: i32, top: i32, c: usize, r: usize) -> u8 {
    let (Ok(c), Ok(r)) = (i64::try_from(c), i64::try_from(r)) else {
        return 0;
    };
    let col = i64::from(left) + c - i64::from(mask.left);
    let row = i64::from(mask.top) - i64::from(top) + r;
    if col < 0 || row < 0 || col >= i64::from(mask.width) || row >= i64::from(mask.height) {
        return 0;
    }
    mask.data[row as usize * mask.width as usize + col as usize]
}

/// The clip's coverage of canvas pixel `(c, r)`, in 0..=255, less its
/// rectangle when `cut` says the shape it covers was already cut to it.
#[expect(clippy::float_cmp, reason = "a rect equals the one it was copied from")]
fn clip_cover(clip: &Clip, masks: &[Vec<u8>], width: usize, c: usize, r: usize, cut: bool) -> u32 {
    let rect = || {
        let [x0, y0, x1, y1] = clip.rect;
        let (x, y) = (overlap(c, x0, x1), overlap(r, y0, y1));
        if x == 255 && y == 255 {
            255
        } else {
            div255(x * y)
        }
    };
    match clip.mask {
        Some(i) => {
            let mask = u32::from(masks[i][r * width + c]);
            if cut || clip.rect == clip.mask_rect {
                mask
            } else {
                div255(rect() * mask)
            }
        }
        None if cut => 255,
        None => rect(),
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "every caller's value is within 0..=255"
)]
const fn narrow(v: u32) -> u8 {
    if v > 255 { 255 } else { v as u8 }
}

/// How much of pixel `i`'s span `[i, i + 1]` lies inside `[lo, hi]`, in
/// 0..=255.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped into 0..=255"
)]
fn overlap(i: usize, lo: f32, hi: f32) -> u32 {
    let i = i as f32;
    let covered = (hi.min(i + 1.0) - lo.max(i)).clamp(0.0, 1.0);
    (covered * 255.0).round() as u32
}

fn intersect(a: [usize; 4], b: [usize; 4]) -> [usize; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

fn union(a: [usize; 4], b: [usize; 4]) -> [usize; 4] {
    [
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[2].max(b[2]),
        a[3].max(b[3]),
    ]
}

fn inverse(m: Matrix<f32>) -> Option<Matrix<f32>> {
    let det = m.xx.mul_add(m.yy, -m.xy * m.yx);
    if det == 0.0 || !det.is_finite() {
        return None;
    }
    let inv = 1.0 / det;
    let (xx, xy, yx, yy) = (m.yy * inv, -m.xy * inv, -m.yx * inv, m.xx * inv);
    Some(Matrix::from_elements([
        xx,
        yx,
        xy,
        yy,
        -xx.mul_add(m.dx, xy * m.dy),
        -yx.mul_add(m.dx, yy * m.dy),
    ]))
}

/// Whether `m` keeps an axis-aligned box axis-aligned.
fn axis_aligned(m: Matrix<f32>) -> bool {
    (m.xy == 0.0 && m.yx == 0.0) || (m.xx == 0.0 && m.yy == 0.0)
}

/// Records an outline mapped by `m`.
struct Transformed<'a> {
    m: Matrix<f32>,
    path: &'a mut Vec<PathEl>,
}

impl Transformed<'_> {
    fn point(&self, x: f32, y: f32) -> Point {
        let (x, y) = self.m.transform(x, y);
        Point { x, y }
    }
}

impl skrifa::PathSink for Transformed<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.point(x, y);
        self.path.push(PathEl::MoveTo(p));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.point(x, y);
        self.path.push(PathEl::LineTo(p));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let el = PathEl::QuadTo([self.point(cx0, cy0), self.point(x, y)]);
        self.path.push(el);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let el = PathEl::CurveTo([self.point(cx0, cy0), self.point(cx1, cy1), self.point(x, y)]);
        self.path.push(el);
    }

    fn close(&mut self) {
        self.path.push(PathEl::Close);
    }
}

impl PaintSink for Painter<'_> {
    fn push_transform(&mut self, transform: Matrix<f32>) {
        let m = self.ctm() * transform;
        self.transforms.push(m);
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn push_clip_glyph(&mut self, glyph: FontGlyphId) {
        if self.failed {
            return;
        }
        let m = self.ctm();
        self.path.0.clear();
        let mut sink = Transformed {
            m,
            path: &mut self.path.0,
        };
        let drawn = self
            .face
            .outline_glyphs()
            .get(glyph)
            .is_some_and(|outline| {
                outline
                    .draw(&DrawOptions::new(), DrawMemory::Scratch, &mut sink)
                    .is_ok()
            });
        if drawn {
            self.push_clip_path();
        } else {
            self.push_empty_clip();
        }
    }

    fn push_clip_box(&mut self, clip_box: BoundingBox<f32>) {
        if self.failed {
            return;
        }
        let m = self.ctm();
        if axis_aligned(m) {
            let (ax, ay) = m.transform(clip_box.x_min, clip_box.y_min);
            let (bx, by) = m.transform(clip_box.x_max, clip_box.y_max);
            let (left, top) = (self.left as f32, self.top as f32);
            let rect = [
                ax.min(bx) - left,
                top - ay.max(by),
                ax.max(bx) - left,
                top - ay.min(by),
            ];
            let parent = self.clip();
            let rect = [
                rect[0].max(parent.rect[0]),
                rect[1].max(parent.rect[1]),
                rect[2].min(parent.rect[2]),
                rect[3].min(parent.rect[3]),
            ];
            let outward = |v: f32, max: usize| {
                if v.is_finite() {
                    (v.max(0.0) as usize).min(max)
                } else {
                    0
                }
            };
            let bounds = intersect(
                parent.bounds,
                [
                    outward(rect[0].floor(), self.width),
                    outward(rect[1].floor(), self.height),
                    outward(rect[2].ceil(), self.width),
                    outward(rect[3].ceil(), self.height),
                ],
            );
            let (mask, mask_rect) = (parent.mask, parent.mask_rect);
            self.clips.push(Clip {
                rect,
                mask,
                mask_rect,
                bounds,
            });
            return;
        }
        self.path.0.clear();
        let corners = [
            (clip_box.x_min, clip_box.y_min),
            (clip_box.x_max, clip_box.y_min),
            (clip_box.x_max, clip_box.y_max),
            (clip_box.x_min, clip_box.y_max),
        ];
        for (i, (x, y)) in corners.into_iter().enumerate() {
            let (x, y) = m.transform(x, y);
            let p = Point { x, y };
            self.path.0.push(if i == 0 {
                PathEl::MoveTo(p)
            } else {
                PathEl::LineTo(p)
            });
        }
        self.path.0.push(PathEl::Close);
        self.push_clip_path();
    }

    fn pop_clip(&mut self) {
        if self.failed || self.clips.len() < 2 {
            return;
        }
        let Some(popped) = self.clips.pop() else {
            return;
        };
        if popped.mask.is_some()
            && popped.mask != self.clip().mask
            && let Some(mask) = self.masks.pop()
        {
            self.budget.credit(mask.capacity());
        }
    }

    fn fill(&mut self, brush: Brush<'_>) {
        if self.failed {
            return;
        }
        match self.shader(&brush, self.ctm()) {
            Some(Some(shader)) => self.paint(&shader, None),
            Some(None) => {}
            None => self.fail(),
        }
    }

    fn fill_glyph(
        &mut self,
        glyph: FontGlyphId,
        brush_transform: Option<Matrix<f32>>,
        brush: Brush<'_>,
    ) {
        if self.failed {
            return;
        }
        let m = brush_transform.map_or_else(|| self.ctm(), |t| self.ctm() * t);
        let shader = match self.shader(&brush, m) {
            Some(Some(shader)) => shader,
            Some(None) => return,
            None => return self.fail(),
        };
        match self.outline(glyph) {
            Some(mask) => self.paint(&shader, Some(&mask)),
            None => self.fail(),
        }
    }

    fn push_layer(&mut self, mode: CompositeMode) {
        if self.failed {
            return;
        }
        let bytes = self.width * self.height * 4;
        if self.budget.charge(bytes).is_none() {
            return self.fail();
        }
        self.layers.push(Layer {
            data: vec![0; bytes],
            mode,
            ink: None,
        });
    }

    fn pop_layer_with_mode(&mut self, _: CompositeMode) {
        if self.failed || self.layers.len() < 2 {
            return;
        }
        let Some(source) = self.layers.pop() else {
            return;
        };
        self.budget.credit(source.data.capacity());
        self.composite(&source);
    }
}

impl Painter<'_> {
    /// Draws a clip from the outline in `self.path`, inside the current
    /// clip.
    fn push_clip_path(&mut self) {
        let bytes = self.width * self.height;
        if self.budget.charge(bytes).is_none() {
            return self.fail();
        }
        let Some(shape) = self.rasterize() else {
            return self.fail();
        };
        let parent = *self.clip();
        let mut data = vec![0; bytes];
        let bounds = self
            .mask_bounds(&shape)
            .map_or([0; 4], |b| intersect(b, parent.bounds));
        let [c0, r0, c1, r1] = bounds;
        for r in r0..r1 {
            for c in c0..c1 {
                let inside = u32::from(mask_at(&shape, self.left, self.top, c, r));
                let under = clip_cover(&parent, &self.masks, self.width, c, r, true);
                data[r * self.width + c] = narrow(div255(inside * under));
            }
        }
        let rect = parent.rect;
        self.masks.push(data);
        let mask = Some(self.masks.len() - 1);
        self.clips.push(Clip {
            rect,
            mask,
            mask_rect: rect,
            bounds,
        });
    }

    /// A clip that covers nothing, for a clip glyph without an outline.
    fn push_empty_clip(&mut self) {
        let clip = Clip {
            bounds: [0; 4],
            ..*self.clip()
        };
        self.clips.push(clip);
    }

    /// Composites `source` onto the layer below with its mode. Both were
    /// painted under the clip that stood when the composite began, and
    /// nothing outside it, so the clip is not applied again: at a pixel
    /// it covers in part, source-in through it would keep the share of a
    /// backdrop that lies inside the clip as if it lay outside.
    fn composite(&mut self, source: &Layer) {
        let width = self.width;
        let op = composite::op(source.mode);
        let Some(below) = self.layers.last_mut() else {
            return;
        };
        let region = match (below.ink, source.ink) {
            (Some(b), Some(s)) if composite::clears_backdrop(source.mode) => Some(union(b, s)),
            (Some(b), None) if composite::clears_backdrop(source.mode) => Some(b),
            (_, s) => s,
        };
        let Some([c0, r0, c1, r1]) = region else {
            return;
        };
        for r in r0..r1 {
            let row = r * width * 4;
            let (from, to) = (row + c0 * 4, row + c1 * 4);
            let pixels = below.data[from..to]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(source.data[from..to].as_chunks::<4>().0);
            for (d, &s) in pixels {
                *d = op(s, *d);
            }
        }
        let region = [c0, r0, c1, r1];
        below.ink = Some(below.ink.map_or(region, |ink| union(ink, region)));
    }
}

/// The four channels of a pixel slice.
fn to4(pixel: &[u8]) -> [u8; 4] {
    std::array::from_fn(|i| pixel[i])
}

#[cfg(test)]
#[cfg(not(windows))]
mod tests {
    use skrifa::{Tag, font::glyph::color::ColorGlyphVersion, raw::types::Matrix};

    use crate::{
        FontStack, FontStyle, GlyphBitmap, GlyphPixels, ShapeCache, Shaper, StyleFaces,
        raster::Rasterizer,
        test_support::{colrv1, glyph_named},
    };

    const NOTO: &str = "FelisColrV1Probe.ttf";
    const TEST_GLYPHS: &str = "test_glyphs-glyf_colr_1.ttf";

    fn alpha(bitmap: &GlyphBitmap) -> Vec<u8> {
        let GlyphPixels::Rgba(rgba) = bitmap.pixels() else {
            panic!("a color bitmap");
        };
        rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect()
    }

    /// Every emoji the subset keeps, as a cluster: a flag, skin tones, a
    /// ZWJ family, a keycap, a source-in glyph and the deepest graph.
    const EMOJI: [&str; 9] = [
        "\u{1F600}",
        "\u{2764}\u{FE0F}",
        "\u{1F1EF}\u{1F1F5}",
        "\u{1F1EA}\u{1F1E8}",
        "\u{1F44D}\u{1F3FD}",
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        "#\u{FE0F}\u{20E3}",
        "\u{1F9D1}\u{1F3FB}\u{200D}\u{1F384}",
        "\u{1F9D1}\u{1F3FF}\u{200D}\u{1F430}\u{200D}\u{1F9D1}\u{1F3FE}",
    ];

    #[test]
    fn noto_color_emoji_built_with_only_version_1_draws_every_emoji_in_color() {
        let stack = FontStack::discover_in_files(
            &[crate::test_font_dir().join("../colrv1").join(NOTO)],
            Some("Noto Color Emoji"),
            &[],
            &[],
            &StyleFaces::default(),
        )
        .expect("the subset loads");
        let mut shaper = Shaper::new();
        let mut cache = ShapeCache::default();
        for text in EMOJI {
            let cluster = shaper.shape_cluster(&stack, 24.0, FontStyle::REGULAR, text);
            let [glyph] = cluster.glyphs.as_slice() else {
                panic!("{text:?} shapes to {} glyphs", cluster.glyphs.len());
            };
            let font = stack.font_at(cluster.font_id, FontStyle::REGULAR);
            for px in [13.0, 24.0, 112.0] {
                let bitmap = cache.rasterize_glyph_id(font, glyph.glyph_id, px);
                assert!(alpha(&bitmap).contains(&255), "{text:?} at {px} px");
            }
        }
    }

    /// `circle_r350` is four quarter circles of alternating winding that
    /// abut through the middle of column 5 and row 5 at 13 px. The rows are
    /// `FreeType` 2.14's gray raster, unhinted, which cancels the two halves of
    /// such a pixel as felis's signed-area accumulation does.
    #[test]
    fn abutting_opposite_windings_cover_as_freetype_gray_raster_does() {
        const FREETYPE: [[u8; 11]; 10] = [
            [0, 0, 0, 1, 52, 0, 52, 1, 0, 0, 0],
            [0, 0, 57, 215, 255, 0, 255, 215, 57, 0, 0],
            [0, 44, 244, 255, 255, 0, 255, 255, 244, 43, 0],
            [0, 179, 255, 255, 255, 0, 255, 255, 255, 175, 0],
            [1, 247, 255, 255, 255, 0, 255, 255, 255, 246, 0],
            [0, 150, 151, 151, 151, 0, 152, 152, 152, 150, 1],
            [0, 215, 255, 255, 255, 0, 255, 255, 255, 213, 0],
            [0, 102, 255, 255, 255, 0, 255, 255, 255, 100, 0],
            [0, 0, 150, 255, 255, 0, 255, 255, 150, 0, 0],
            [0, 0, 0, 69, 158, 0, 158, 69, 0, 0, 0],
        ];
        let mut font = colrv1(TEST_GLYPHS);
        font.raster = crate::raster::RasterOptions::NEVER_HINTED;
        let bitmap =
            Rasterizer::default().rasterize(&font, glyph_named(&font, "circle_r350"), 13.0);
        assert_eq!(
            (bitmap.width(), bitmap.height(), bitmap.left, bitmap.top),
            (11, 10, 1, 13)
        );
        let GlyphPixels::Coverage(coverage) = bitmap.pixels() else {
            panic!("an outline draws coverage");
        };
        for (r, row) in FREETYPE.iter().enumerate() {
            for (c, &want) in row.iter().enumerate() {
                let got = coverage[r * 11 + c];
                assert!(got.abs_diff(want) <= 1, "({c}, {r}): {got} against {want}");
            }
        }
    }

    fn draw(name: &str, px: f32) -> GlyphBitmap {
        let font = colrv1(TEST_GLYPHS);
        Rasterizer::default().rasterize(&font, glyph_named(&font, name), px)
    }

    #[test]
    fn a_paint_the_painter_lacks_draws_the_glyph_whole_from_the_next_source() {
        assert!(!draw("sweep_0_360_pad_narrow", 40.0).pixels().is_color());
    }

    #[test]
    fn a_paint_graph_that_cycles_draws_the_next_source() {
        let font = colrv1(TEST_GLYPHS);
        for name in ["paintcolrglyph_cycle_first", "paintcolrglyph_cycle_second"] {
            let glyph = glyph_named(&font, name);
            let mut outlines = crate::raster::SkrifaOutlines::default();
            let palette = font.face.palettes().get(0).expect("a palette").colors();
            assert!(
                super::render(&mut outlines, &font, palette, glyph, 40.0).is_none(),
                "{name}"
            );
            assert!(
                !Rasterizer::default()
                    .rasterize(&font, glyph, 40.0)
                    .pixels()
                    .is_color()
            );
        }
    }

    /// The sweeps whose stops coincide, which cairo never finishes: one
    /// that pads still reaches a sweep, which the painter lacks, so it
    /// draws its outline; one that reflects or repeats reaches the painter
    /// as the solid color of its last stop, as the `COLR` specification
    /// resolves such a sweep, and draws in color.
    #[test]
    fn sweeps_with_coincident_stops_finish_on_one_source() {
        let font = colrv1(TEST_GLYPHS);
        for direction in ["forward", "reverse"] {
            for colors in ["blue_red", "linen_gray"] {
                for (extend, color) in [("pad", false), ("reflect", true), ("repeat", true)] {
                    let name = format!("sweep_coincident_stops_{direction}_{colors}_{extend}");
                    let bitmap =
                        Rasterizer::default().rasterize(&font, glyph_named(&font, &name), 40.0);
                    assert_eq!(bitmap.pixels().is_color(), color, "{name}");
                }
            }
        }
    }

    /// `clip_box_center`'s clip box right edge moves with `CLXA`: at
    /// +10.3 units it ends 0.015 px into column 38 at 50 px, which its
    /// extents, whose deltas are rounded to whole units, leave out.
    #[test]
    fn a_varied_clip_box_sizes_the_canvas_to_its_fractional_edge() {
        let face = colrv1("test_glyphs-glyf_colr_1_variable.ttf");
        let setting = [(Tag::new(b"CLXA"), 10.3)];
        let font = face.with_coords(face.normalized_coords(&setting), setting.to_vec());
        let glyph = font
            .face
            .color_glyphs()
            .get_with_version(
                skrifa::GlyphId::new(glyph_named(&font, "clip_box_center")),
                ColorGlyphVersion::ColrV1,
            )
            .expect("a version 1 glyph");
        let base = Matrix::from_elements([0.05, 0.0, 0.0, 0.05, 0.0, 0.0]);
        assert_eq!(
            super::canvas_box(&glyph, base),
            Some(Some([12, 38, 39, 12]))
        );
        let rounded = glyph.extents().expect("a well formed graph");
        assert_eq!((rounded.x_bearing + rounded.width) * 0.05, 38.0);
    }
}

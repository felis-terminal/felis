//! `COLR` version 0 glyphs: each layer's outline through the face's own
//! scaler and [`coverage`], composited source-over in its palette color.

use skrifa::{
    GlyphId as FontGlyphId,
    font::glyph::color::{Brush, ColorGlyphVersion, CompositeMode, PaintSink},
    raw::types::{BoundingBox, Matrix},
};

use super::{SkrifaOutlines, coverage};
use crate::{Font, GlyphBitmap, GlyphId, GlyphPixels};

/// The color a layer gets when it names the text foreground: a canvas has
/// no foreground of its own.
const FOREGROUND: [u8; 4] = [128, 128, 128, 255];
const FOREGROUND_INDEX: u16 = 0xFFFF;

/// One `COLR` glyph's layers, in paint order.
#[derive(Default)]
struct Layers {
    fills: Vec<(FontGlyphId, u16, f32)>,
    /// A paint operation version 0 has no use for.
    unsupported: bool,
}

impl PaintSink for Layers {
    fn push_transform(&mut self, _: Matrix<f32>) {
        self.unsupported = true;
    }
    fn pop_transform(&mut self) {}
    fn push_clip_glyph(&mut self, _: FontGlyphId) {
        self.unsupported = true;
    }
    fn push_clip_box(&mut self, _: BoundingBox<f32>) {
        self.unsupported = true;
    }
    fn pop_clip(&mut self) {}
    fn fill(&mut self, _: Brush<'_>) {
        self.unsupported = true;
    }
    fn fill_glyph(&mut self, glyph: FontGlyphId, transform: Option<Matrix<f32>>, brush: Brush<'_>) {
        match brush {
            Brush::Solid {
                palette_index,
                alpha,
            } if transform.is_none() => self.fills.push((glyph, palette_index, alpha)),
            _ => self.unsupported = true,
        }
    }
    fn push_layer(&mut self, _: CompositeMode) {
        self.unsupported = true;
    }
}

/// Premultiplied RGBA8, `top` rows above the baseline at its first row.
struct Canvas {
    width: usize,
    height: usize,
    left: i32,
    top: i32,
    data: Vec<u8>,
}

/// `None` when the face draws `glyph` with no version 0 layers, a layer
/// asks for a paint version 0 lacks, or a buffer would exceed the raster
/// budget; the next source is then tried.
pub(super) fn render(
    outlines: &mut SkrifaOutlines,
    font: &Font,
    glyph: GlyphId,
    px: f32,
) -> Option<GlyphBitmap> {
    let palette = font.face.palettes().get(0)?;
    let colors = font.face.color_glyphs();
    let color_glyph =
        colors.get_with_version(FontGlyphId::new(glyph), ColorGlyphVersion::ColrV0)?;
    let mut layers = Layers::default();
    color_glyph.paint(&mut layers).ok()?;
    if layers.unsupported {
        return None;
    }
    let reserved = layers.fills.capacity() * size_of::<(FontGlyphId, u16, f32)>();
    let canvas_budget = coverage::IMAGE_BUDGET.checked_sub(reserved)?;
    // Sized from the layers as hinted, not from the face's extents:
    // hinting moves an edge as far as the face's program says.
    let mut ink: Option<[i32; 4]> = None;
    for &(layer, ..) in &layers.fills {
        let [left, top, right, bottom] = outlines.layer_box(font, layer.to_u32(), px)?;
        if left < right && bottom < top {
            ink = Some(ink.map_or([left, top, right, bottom], |[l, t, r, b]| {
                [l.min(left), t.max(top), r.max(right), b.min(bottom)]
            }));
        }
    }
    let Some(ink) = ink else {
        return Some(GlyphBitmap::new(0, 0, 0, 0, GlyphPixels::Rgba(Vec::new())));
    };
    coverage::reserve_segments(&mut outlines.segments);
    let mut canvas = canvas(ink, px, canvas_budget)?;
    let budget = canvas_budget - canvas.data.len();
    for (layer, index, alpha) in layers.fills {
        let mask = outlines.mask_within(font, layer.to_u32(), px, budget)?;
        let color = if index == FOREGROUND_INDEX {
            FOREGROUND
        } else {
            palette
                .colors()
                .get(usize::from(index))
                .map_or(FOREGROUND, |c| [c.red, c.green, c.blue, c.alpha])
        };
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "alpha is clamped into 0..=255"
        )]
        let alpha = (f32::from(color[3]) * alpha).round().clamp(0.0, 255.0) as u8;
        let dropped = canvas.composite(&mask, [color[0], color[1], color[2], alpha]);
        if cfg!(test) {
            assert_eq!(
                dropped, 0,
                "glyph {glyph} at {px} px inks outside its canvas"
            );
        }
    }
    Some(canvas.into_bitmap())
}

/// The canvas over `[left, top, right, bottom]`, y up.
#[expect(
    clippy::cast_precision_loss,
    reason = "a side past f32's integers is past the side limit"
)]
fn canvas([left, top, right, bottom]: [i32; 4], px: f32, budget: usize) -> Option<Canvas> {
    let width = usize::try_from(i64::from(right) - i64::from(left)).ok()?;
    let height = usize::try_from(i64::from(top) - i64::from(bottom)).ok()?;
    let side = coverage::side_limit(px)?;
    if width as f32 > side || height as f32 > side {
        return None;
    }
    let bytes = width.checked_mul(height)?.checked_mul(4)?;
    if bytes > budget {
        return None;
    }
    Some(Canvas {
        width,
        height,
        left,
        top,
        data: vec![0; bytes],
    })
}

/// `round(x / 255)` for `x` up to `255 * 255`.
const fn div255(x: u32) -> u32 {
    (x + 128 + ((x + 128) >> 8)) >> 8
}

impl Canvas {
    /// Source-over `color` (straight) through `mask`; ink outside the
    /// canvas is dropped, and the dropped pixels counted.
    fn composite(&mut self, mask: &coverage::Mask, color: [u8; 4]) -> usize {
        let mut dropped = 0;
        let [red, green, blue, opacity] = color.map(u32::from);
        let width = mask.width as usize;
        let canvas_index = |at: i64, size: usize| usize::try_from(at).ok().filter(|&i| i < size);
        for (row, coverage_row) in (0_u32..).zip(mask.data.chunks_exact(width.max(1))) {
            let Some(y) = canvas_index(
                i64::from(self.top) - i64::from(mask.top) + i64::from(row),
                self.height,
            ) else {
                dropped += coverage_row
                    .iter()
                    .filter(|&&coverage| coverage != 0)
                    .count();
                continue;
            };
            for (col, &coverage) in (0_u32..).zip(coverage_row) {
                if coverage == 0 {
                    continue;
                }
                let Some(x) = canvas_index(
                    i64::from(mask.left) - i64::from(self.left) + i64::from(col),
                    self.width,
                ) else {
                    dropped += 1;
                    continue;
                };
                let alpha = div255(u32::from(coverage) * opacity);
                let source = [
                    div255(alpha * red),
                    div255(alpha * green),
                    div255(alpha * blue),
                    alpha,
                ];
                let pixel = &mut self.data[(y * self.width + x) * 4..][..4];
                for (under, over) in pixel.iter_mut().zip(source) {
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "premultiplied source-over stays within 0..=255"
                    )]
                    let blended = (over + div255(u32::from(*under) * (255 - alpha))) as u8;
                    *under = blended;
                }
            }
        }
        dropped
    }

    /// Trimmed to the ink box and un-premultiplied in the canvas's own
    /// buffer; a canvas with no ink is an empty color bitmap, which draws
    /// nothing.
    fn into_bitmap(self) -> GlyphBitmap {
        let Self {
            width,
            height,
            left,
            top,
            mut data,
        } = self;
        let inked = |data: &[u8], x: usize, y: usize| data[(y * width + x) * 4 + 3] != 0;
        let row_inked = |y: &usize| (0..width).any(|x| inked(&data, x, *y));
        let Some(y0) = (0..height).find(row_inked) else {
            return GlyphBitmap::new(0, 0, 0, 0, GlyphPixels::Rgba(Vec::new()));
        };
        let y1 = (0..height).rfind(row_inked).unwrap_or(y0);
        let column_inked = |x: &usize| (y0..=y1).any(|y| inked(&data, *x, y));
        let x0 = (0..width).find(column_inked).unwrap_or(0);
        let x1 = (0..width).rfind(column_inked).unwrap_or(x0);
        let (trimmed_width, trimmed_height) = (x1 - x0 + 1, y1 - y0 + 1);
        // Each pixel is written at or before the offset it is read from,
        // so no pixel is overwritten before it is read.
        for (row, y) in (y0..=y1).enumerate() {
            for col in 0..trimmed_width {
                let from = (y * width + x0 + col) * 4;
                let pixel = std::array::from_fn(|channel| data[from + channel]);
                let to = (row * trimmed_width + col) * 4;
                data[to..to + 4].copy_from_slice(&super::strike::unpremultiply(pixel));
            }
        }
        data.truncate(trimmed_width * trimmed_height * 4);
        coverage::shrink_within(&mut data);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            reason = "the side limit keeps the canvas far inside u32 and i32"
        )]
        GlyphBitmap::new(
            trimmed_width as u32,
            trimmed_height as u32,
            left + x0 as i32,
            top - y0 as i32,
            GlyphPixels::Rgba(data),
        )
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{Canvas, coverage::Mask};
    use crate::raster::strike::unpremultiply;

    fn canvas(pixel: [u8; 4]) -> Canvas {
        Canvas {
            width: 1,
            height: 1,
            left: 0,
            top: 1,
            data: pixel.to_vec(),
        }
    }

    fn mask(coverage: u8) -> Mask {
        Mask {
            width: 1,
            height: 1,
            left: 0,
            top: 1,
            data: vec![coverage],
        }
    }

    fn premultiplied() -> impl Strategy<Value = [u8; 4]> {
        any::<[u8; 4]>().prop_map(|[r, g, b, a]| [r.min(a), g.min(a), b.min(a), a])
    }

    proptest! {
        #[test]
        fn an_opaque_layer_over_full_coverage_replaces_the_pixel(
            under in premultiplied(),
            [r, g, b] in any::<[u8; 3]>(),
        ) {
            let mut canvas = canvas(under);
            canvas.composite(&mask(255), [r, g, b, 255]);
            prop_assert_eq!(canvas.data, vec![r, g, b, 255]);
        }

        #[test]
        fn compositing_a_layer_never_lowers_alpha(
            under in premultiplied(),
            color in any::<[u8; 4]>(),
            coverage in any::<u8>(),
        ) {
            let mut canvas = canvas(under);
            canvas.composite(&mask(coverage), color);
            prop_assert!(canvas.data[3] >= under[3]);
            prop_assert!(canvas.data[..3].iter().all(|&c| c <= canvas.data[3]));
        }

        #[test]
        fn unpremultiplying_then_premultiplying_returns_within_one(pixel in premultiplied()) {
            let straight = unpremultiply(pixel);
            let a = u32::from(pixel[3]);
            for (s, p) in straight[..3].iter().zip(&pixel[..3]) {
                let back = (u32::from(*s) * a + 127) / 255;
                prop_assert!(back.abs_diff(u32::from(*p)) <= 1, "{pixel:?} -> {straight:?}");
            }
        }
    }

    #[cfg(not(windows))]
    mod probes {
        use crate::{GlyphBitmap, GlyphPixels, raster::Rasterizer, test_support::color_probe};

        const PX: f32 = 100.0;

        fn draw(face: &str, c: char) -> GlyphBitmap {
            let font = color_probe(face);
            let glyph = font.glyph_for(c).expect("the probe maps it");
            Rasterizer::default().rasterize(&font, glyph, PX)
        }

        /// The straight RGBA pixel covering `(x, y)`, y up from the baseline.
        fn at(bitmap: &GlyphBitmap, x: i32, y: i32) -> [u8; 4] {
            let GlyphPixels::Rgba(rgba) = bitmap.pixels() else {
                panic!("a color bitmap");
            };
            let (col, row) = (x - bitmap.left, bitmap.top - 1 - y);
            let width = bitmap.width().cast_signed();
            assert!((0..width).contains(&col) && (0..bitmap.height().cast_signed()).contains(&row));
            let i = ((row * width + col) * 4) as usize;
            rgba[i..i + 4].try_into().expect("four channels")
        }

        #[expect(
            clippy::suboptimal_flops,
            reason = "the expected values are worked by hand in this order"
        )]
        fn over(pixel: [u8; 4], background: f32) -> [u8; 3] {
            let a = f32::from(pixel[3]) / 255.0;
            [0, 1, 2].map(|c| (f32::from(pixel[c]) * a + background * (1.0 - a)).round() as u8)
        }

        /// `A`'s layers: opaque red below `y` = 40, translucent blue (CPAL
        /// alpha 128) above `y` = 30, at 0.1 px per unit.
        #[test]
        fn a_glyph_with_both_versions_paints_its_version_0_layers() {
            let a = draw("FelisColorProbe.ttf", 'A');
            assert_eq!(
                at(&a, 10, 20),
                [255, 0, 0, 255],
                "red layer, no green v1 paint"
            );
        }

        #[test]
        fn a_translucent_layer_composites_as_its_palette_alpha_over_black_and_white() {
            let a = draw("FelisColorProbe.ttf", 'A');
            let blue_only = at(&a, 30, 55);
            assert_eq!(blue_only, [0, 0, 255, 128]);
            assert_eq!(over(blue_only, 0.0), [0, 0, 128]);
            assert_eq!(over(blue_only, 255.0), [127, 127, 255]);
            let blue_over_red = at(&a, 30, 35);
            assert_eq!(over(blue_over_red, 0.0), [127, 0, 128]);
            assert_eq!(over(blue_over_red, 255.0), [127, 0, 128]);
        }

        #[test]
        fn a_fully_covered_pixel_of_an_opaque_layer_is_opaque() {
            let a = draw("FelisColorProbe.ttf", 'A');
            assert_eq!(at(&a, 10, 20)[3], 255);
        }

        #[test]
        fn a_glyph_only_version_1_defines_draws_its_outline() {
            assert!(!draw("FelisColorProbe.ttf", 'B').pixels().is_color());
        }

        #[test]
        fn a_face_with_colr_but_no_cpal_draws_outlines() {
            let font = color_probe("FelisColorProbeNoCpal.ttf");
            assert!(!font.has_color_glyphs());
            assert!(!draw("FelisColorProbeNoCpal.ttf", 'A').pixels().is_color());
        }

        #[test]
        fn layers_too_far_apart_for_the_budget_fall_through_to_the_outline() {
            let c = draw("FelisColorProbe.ttf", 'C');
            assert!(
                !c.pixels().is_color(),
                "{} {} {} {}",
                c.left,
                c.top,
                c.width(),
                c.height()
            );
            assert_eq!((c.width(), c.height()), (50, 70));
        }

        /// `A`'s one layer is `B`, whose glyph program lifts its top edge
        /// 3 px above its outline: the canvas holds the layer as each mode
        /// hints it, so the color glyph covers what `B` draws alone.
        #[test]
        fn a_layer_hinted_past_the_outline_extents_keeps_its_ink() {
            let face = color_probe("FelisHintedProbe.ttf");
            let (layered, layer) = (
                face.glyph_for('A').expect("the probe maps it"),
                face.glyph_for('B').expect("the probe maps it"),
            );
            let mut tops = Vec::new();
            for (mode, raster) in crate::goldens::raster_modes() {
                let font = crate::goldens::with_raster(&face, raster);
                let mut rasterizer = Rasterizer::default();
                let color = rasterizer.rasterize(&font, layered, PX);
                let outline = rasterizer.rasterize(&font, layer, PX);
                assert!(color.pixels().is_color(), "{mode}");
                let placement = |b: &GlyphBitmap| (b.left, b.top, b.width(), b.height());
                assert_eq!(placement(&color), placement(&outline), "{mode}");
                tops.push((mode, color.top));
            }
            assert_eq!(tops[0], ("unhinted", 50));
            for &(mode, top) in &tops[1..] {
                assert_eq!(top, 53, "{mode}");
            }
        }

        /// `E`'s layers sit at opposite corners of an em square, so its
        /// canvas is a square `px` on a side: at 2032 px it fits the
        /// budget, at 2033 px it does not.
        #[test]
        fn a_canvas_past_the_budget_falls_through_to_the_outline() {
            let font = color_probe("FelisColorProbe.ttf");
            let glyph = font.glyph_for('E').expect("the probe maps it");
            let mut rasterizer = Rasterizer::default();
            assert!(
                rasterizer
                    .rasterize(&font, glyph, 2032.0)
                    .pixels()
                    .is_color()
            );
            assert!(
                !rasterizer
                    .rasterize(&font, glyph, 2033.0)
                    .pixels()
                    .is_color()
            );
        }

        #[test]
        fn layers_that_paint_no_ink_draw_an_empty_color_glyph() {
            let d = draw("FelisColorProbe.ttf", 'D');
            assert!(d.pixels().is_color());
            assert_eq!((d.width(), d.height()), (0, 0));
        }
    }
}

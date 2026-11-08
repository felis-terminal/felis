//! The rasterization gateway behind [`crate::ShapeCache`]: color glyphs
//! through swash, monochrome outlines through skrifa and [`coverage`].

use skrifa::{DrawMemory, HintOptions, PathSink, RasterTarget, ScalerFont};
use swash::{
    scale::{Render, ScaleContext, Source, StrikeWith, image::Content},
    zeno::Format,
};

use crate::{Font, GlyphBitmap, GlyphId, GlyphPixels};

pub(crate) mod coverage;
#[cfg(test)]
mod tests;

use coverage::{PathEl, Point};

/// How a face's outlines are drawn, fixed for the face's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RasterOptions {
    /// `None` draws unhinted.
    pub(crate) hinting: Option<HintOptions>,
}

impl RasterOptions {
    pub(crate) const UNHINTED: Self = Self { hinting: None };

    pub(crate) fn hinted() -> Self {
        Self {
            hinting: Some(
                HintOptions::auto_fallback()
                    .with_target(RasterTarget::Lcd)
                    .with_preserve_linear_metrics(true),
            ),
        }
    }

    /// Unhinted on macOS: CoreText never grid-fits, so a hinted raster
    /// sits beside every native window with stems snapped a pixel off.
    pub(crate) fn platform() -> Self {
        if cfg!(target_os = "macos") {
            Self::UNHINTED
        } else {
            Self::hinted()
        }
    }
}

pub(crate) trait OutlineBackend {
    /// Monochrome outline to A8 coverage; `None` when the face has no
    /// outline for `glyph` or it is over a raster limit.
    fn coverage(&mut self, font: &Font, glyph: GlyphId, px: f32) -> Option<GlyphBitmap>;
}

#[derive(Default)]
pub(crate) struct Rasterizer {
    outlines: SkrifaOutlines,
    color: ScaleContext,
}

impl Rasterizer {
    pub(crate) fn rasterize(&mut self, font: &Font, glyph: GlyphId, px: f32) -> GlyphBitmap {
        if font.color
            && let Some(bitmap) = self.color_glyph(font, glyph, px)
        {
            return bitmap;
        }
        self.outlines
            .coverage(font, glyph, px)
            .unwrap_or_else(GlyphBitmap::blank)
    }

    fn color_glyph(&mut self, font: &Font, glyph: GlyphId, px: f32) -> Option<GlyphBitmap> {
        let mut scaler = self
            .color
            .builder(font.font_ref())
            .size(px)
            .hint(font.raster.hinting.is_some())
            .normalized_coords(&font.coords)
            .build();
        let image = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
        ])
        .format(Format::Alpha)
        .render(&mut scaler, glyph)?;
        let pixels = match image.content {
            Content::Mask => GlyphPixels::Coverage(image.data),
            // swash emits straight RGBA in R,G,B,A order (sbix/CBDT PNG
            // decoder and COLR blitter alike); a BGRA reorder here renders a
            // red 🦀 blue.
            Content::Color => GlyphPixels::Rgba(image.data),
            // LCD masks are RGBA-packed coverage, not color.
            Content::SubpixelMask => {
                GlyphPixels::Coverage(image.data.as_chunks::<4>().0.iter().map(|p| p[3]).collect())
            }
        };
        Some(GlyphBitmap::new(
            image.placement.width,
            image.placement.height,
            image.placement.left,
            image.placement.top,
            pixels,
        ))
    }
}

/// Hinted setup reruns the font's programs, so scalers are kept for the
/// last few (font, size) pairs.
const SCALER_SLOTS: usize = 8;

struct ScalerSlot {
    font: u64,
    px: u32,
    used: u64,
    scaler: ScalerFont,
}

#[derive(Default)]
pub(crate) struct SkrifaOutlines {
    slots: Vec<ScalerSlot>,
    clock: u64,
    path: PathRecorder,
    segments: Vec<[f32; 4]>,
}

impl SkrifaOutlines {
    fn scaler(&mut self, font: &Font, px: f32) -> &ScalerFont {
        self.clock += 1;
        let key = (font.raster_id, px.to_bits());
        let found = self.slots.iter().position(|s| (s.font, s.px) == key);
        let index = found.unwrap_or_else(|| {
            let slot = ScalerSlot {
                font: key.0,
                px: key.1,
                used: 0,
                scaler: scaler_font(font, px),
            };
            if self.slots.len() < SCALER_SLOTS {
                self.slots.push(slot);
                self.slots.len() - 1
            } else {
                let oldest = (0..self.slots.len())
                    .min_by_key(|&i| self.slots[i].used)
                    .unwrap_or(0);
                self.slots[oldest] = slot;
                oldest
            }
        });
        self.slots[index].used = self.clock;
        &self.slots[index].scaler
    }
}

/// `force_unhinted`, not `unhinted`: skrifa's `unhinted` still hints a
/// hint-reliant face, which would change those faces' pixels on macOS.
/// A face whose hinting setup fails draws unhinted: a failed hinted
/// scaler fails every draw, and the face would draw no glyphs at all.
fn scaler_font(font: &Font, px: f32) -> ScalerFont {
    let outlines = font.face.clone();
    match font.raster.hinting {
        Some(options) => {
            let scaler = ScalerFont::hinted(outlines.clone(), px, options);
            if scaler.hinting_error().is_some() {
                ScalerFont::force_unhinted(outlines, Some(px))
            } else {
                scaler
            }
        }
        None => ScalerFont::force_unhinted(outlines, Some(px)),
    }
}

impl OutlineBackend for SkrifaOutlines {
    fn coverage(&mut self, font: &Font, glyph: GlyphId, px: f32) -> Option<GlyphBitmap> {
        if !(px.is_finite() && px > 0.0) {
            return None;
        }
        let mut path = std::mem::take(&mut self.path);
        path.0.clear();
        let outline = self
            .scaler(font, px)
            .outline(skrifa::GlyphId::new(u32::from(glyph)));
        let drawn =
            outline.is_some_and(|outline| outline.draw(DrawMemory::Scratch, &mut path).is_ok());
        let mask = drawn
            .then(|| coverage::rasterize(&path.0, px, &mut self.segments))
            .flatten();
        self.path = path;
        mask.map(GlyphBitmap::from)
    }
}

impl From<coverage::Mask> for GlyphBitmap {
    fn from(mask: coverage::Mask) -> Self {
        Self::new(
            mask.width,
            mask.height,
            mask.left,
            mask.top,
            GlyphPixels::Coverage(mask.data),
        )
    }
}

#[derive(Default)]
struct PathRecorder(Vec<PathEl>);

impl PathSink for PathRecorder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.push(PathEl::MoveTo(Point { x, y }));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.push(PathEl::LineTo(Point { x, y }));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0
            .push(PathEl::QuadTo([Point { x: cx0, y: cy0 }, Point { x, y }]));
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.push(PathEl::CurveTo([
            Point { x: cx0, y: cy0 },
            Point { x: cx1, y: cy1 },
            Point { x, y },
        ]));
    }

    fn close(&mut self) {
        self.0.push(PathEl::Close);
    }
}

/// Rasterizes the path `draw` emits, for the fuzz target.
#[cfg(feature = "testing")]
pub(crate) fn rasterize_path(px: f32, draw: impl FnOnce(&mut dyn PathSink)) -> GlyphBitmap {
    let mut path = PathRecorder::default();
    draw(&mut path);
    coverage::rasterize(&path.0, px, &mut Vec::new())
        .map_or_else(GlyphBitmap::blank, GlyphBitmap::from)
}

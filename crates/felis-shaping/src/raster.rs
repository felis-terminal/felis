//! The rasterization gateway behind [`crate::ShapeCache`]: `COLR` layers,
//! then `sbix`/`CBDT` strikes for a color face, then the monochrome
//! outline, all outlines through skrifa and [`coverage`].

use skrifa::{DrawMemory, HintOptions, PathSink, RasterTarget, ScalerFont};

use crate::{Font, GlyphBitmap, GlyphId, GlyphPixels, provider::FaceHinting};

mod color;
pub(crate) mod coverage;
mod resample;
mod strike;
#[cfg(test)]
mod tests;

use coverage::{PathEl, Point};

/// How a face's outlines are drawn, fixed for the face's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RasterOptions {
    pub(crate) hinting: Hinting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hinting {
    Hinted(HintOptions),
    /// Unhinted, except a hint-reliant face, whose contours are wrong
    /// without its instructions, as `FreeType` hints it under `hintnone`.
    Unhinted,
    /// Unhinted even for a hint-reliant face, as CoreText draws it.
    Never,
}

impl RasterOptions {
    pub(crate) const NEVER_HINTED: Self = Self {
        hinting: Hinting::Never,
    };

    pub(crate) fn hinted() -> Self {
        Self {
            hinting: Hinting::Hinted(
                HintOptions::auto_fallback()
                    .with_target(RasterTarget::Lcd)
                    .with_preserve_linear_metrics(true),
            ),
        }
    }

    /// The options for a face the host reports `hinting` for, the
    /// platform's own where it reports none.
    pub(crate) fn for_face(hinting: Option<FaceHinting>) -> Self {
        hinting.map_or_else(Self::platform, Self::fontconfig)
    }

    /// Unhinted on macOS: CoreText never grid-fits, so a hinted raster
    /// sits beside every native window with stems snapped a pixel off. A
    /// Linux face outside fontconfig takes fontconfig's stock `hintslight`.
    fn platform() -> Self {
        if cfg!(target_os = "macos") {
            Self::NEVER_HINTED
        } else if cfg!(windows) {
            Self::hinted()
        } else {
            Self::fontconfig(FaceHinting::Slight { autohint: false })
        }
    }

    /// Linear metrics are kept where fontconfig asks for no horizontal
    /// fitting; `hintfull` asks for it.
    fn fontconfig(hinting: FaceHinting) -> Self {
        let engine = |autohint| {
            if autohint {
                HintOptions::auto()
            } else {
                HintOptions::auto_fallback()
            }
        };
        let hinting = match hinting {
            FaceHinting::None => Hinting::Unhinted,
            FaceHinting::Slight { autohint } => Hinting::Hinted(
                engine(autohint)
                    .with_target(RasterTarget::Light)
                    .with_preserve_linear_metrics(true),
            ),
            FaceHinting::Full { autohint } => {
                Hinting::Hinted(engine(autohint).with_target(RasterTarget::Normal))
            }
        };
        Self { hinting }
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
        if !(px.is_finite() && px > 0.0) {
            return None;
        }
        color::render(&mut self.outlines, font, glyph, px)
            .or_else(|| strike::render(font, glyph, px))
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
        let key = (font.id, px.to_bits());
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

/// A face whose hinting setup fails draws unhinted: a failed hinted
/// scaler fails every draw, and the face would draw no glyphs at all.
fn scaler_font(font: &Font, px: f32) -> ScalerFont {
    let outlines = font.face.clone();
    let scaler = match font.raster.hinting {
        Hinting::Hinted(options) => ScalerFont::hinted(outlines.clone(), px, options),
        Hinting::Unhinted => ScalerFont::unhinted(outlines.clone(), Some(px)),
        Hinting::Never => return ScalerFont::force_unhinted(outlines, Some(px)),
    };
    if scaler.hinting_error().is_some() {
        ScalerFont::force_unhinted(outlines, Some(px))
    } else {
        scaler
    }
}

impl SkrifaOutlines {
    /// A `COLR` layer's coverage within `budget` bytes; a layer with no
    /// outline is empty, and `None` means over a raster limit.
    fn mask_within(
        &mut self,
        font: &Font,
        glyph: GlyphId,
        px: f32,
        budget: usize,
    ) -> Option<coverage::Mask> {
        self.with_layer_path(font, glyph, px, |path, segments| match path {
            Some(path) => coverage::rasterize_within(path, px, segments, budget),
            None => Some(coverage::Mask::default()),
        })
    }

    /// The pixel box a `COLR` layer's coverage is drawn into, as the face
    /// hints it; empty for a layer that draws nothing.
    fn layer_box(&mut self, font: &Font, glyph: GlyphId, px: f32) -> Option<[i32; 4]> {
        self.with_layer_path(font, glyph, px, |path, segments| match path {
            Some(path) => coverage::pixel_box(path, px, segments),
            None => Some([0; 4]),
        })
    }

    /// `f` with the layer's outline as the face's scaler draws it at `px`,
    /// or `None` when it has none.
    fn with_layer_path<T>(
        &mut self,
        font: &Font,
        glyph: GlyphId,
        px: f32,
        f: impl FnOnce(Option<&[PathEl]>, &mut Vec<[f32; 4]>) -> T,
    ) -> T {
        let mut path = std::mem::take(&mut self.path);
        path.0.clear();
        let outline = self.scaler(font, px).outline(skrifa::GlyphId::new(glyph));
        let drawn =
            outline.is_some_and(|outline| outline.draw(DrawMemory::Scratch, &mut path).is_ok());
        let result = f(drawn.then_some(path.0.as_slice()), &mut self.segments);
        self.path = path;
        result
    }
}

impl OutlineBackend for SkrifaOutlines {
    fn coverage(&mut self, font: &Font, glyph: GlyphId, px: f32) -> Option<GlyphBitmap> {
        if !(px.is_finite() && px > 0.0) {
            return None;
        }
        let mut path = std::mem::take(&mut self.path);
        path.0.clear();
        let outline = self.scaler(font, px).outline(skrifa::GlyphId::new(glyph));
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

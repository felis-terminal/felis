//! What the cell painter emits: rectangles, glyph draws and line
//! decorations in physical pixels, colors in linear RGBA. A backend
//! draws `rects`, then `glyphs`, then `decos`, each in list order.

/// Which glyph sheet a [`GlyphDraw`] samples.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AtlasKind {
    /// Coverage masks, tinted by the draw's color.
    Mono,
    /// RGBA color bitmaps, drawn untinted.
    Color,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Rect {
    pub origin_px: [f32; 2],
    pub size_px: [f32; 2],
    pub color: [f32; 4],
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GlyphDraw {
    pub origin_px: [f32; 2],
    pub size_px: [f32; 2],
    pub atlas: AtlasKind,
    /// Top-left of the sampled region, in pixels of the `atlas` sheet.
    pub src_min_px: [f32; 2],
    /// Bottom-right of the sampled region, in pixels of the `atlas` sheet.
    pub src_max_px: [f32; 2],
    pub color: [f32; 4],
}

impl GlyphDraw {
    /// Narrows the source to the fractions `t0..t1` of its width, the
    /// part a destination cut at the same fractions still shows.
    pub fn crop_src_x(&mut self, t0: f32, t1: f32) {
        let (min, max) = crop(self.src_min_px[0], self.src_max_px[0], t0, t1);
        self.src_min_px[0] = min;
        self.src_max_px[0] = max;
    }

    /// [`Self::crop_src_x`] along the height.
    pub fn crop_src_y(&mut self, t0: f32, t1: f32) {
        let (min, max) = crop(self.src_min_px[1], self.src_max_px[1], t0, t1);
        self.src_min_px[1] = min;
        self.src_max_px[1] = max;
    }
}

fn crop(min: f32, max: f32, t0: f32, t1: f32) -> (f32, f32) {
    let span = max - min;
    (span.mul_add(t0, min), span.mul_add(t1, min))
}

/// The line style of a [`Decoration`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecoKind {
    Solid,
    /// `SGR 4:4`.
    Dotted,
    /// `SGR 4:5`.
    Dashed,
    /// `SGR 4:3`.
    Curly,
}

/// The quad spans the decoration band: the line rectangle for the
/// straight kinds, a taller band the sine swings inside for
/// [`DecoKind::Curly`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Decoration {
    pub origin_px: [f32; 2],
    pub size_px: [f32; 2],
    pub color: [f32; 4],
    pub kind: DecoKind,
    pub thickness_px: f32,
    /// Pattern period (dotted / dashed) or sine wavelength (curly).
    /// Unused by [`DecoKind::Solid`].
    pub period_px: f32,
}

#[derive(Debug, Default, PartialEq)]
pub struct RowPaint {
    pub rects: Vec<Rect>,
    pub glyphs: Vec<GlyphDraw>,
    /// Drawn after the glyphs so strikethrough sits over the ink.
    pub decos: Vec<Decoration>,
}

impl RowPaint {
    pub fn clear(&mut self) {
        self.rects.clear();
        self.glyphs.clear();
        self.decos.clear();
    }
}

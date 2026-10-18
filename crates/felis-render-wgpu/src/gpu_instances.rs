//! The instance records the bg, fg, decoration and image passes of
//! `docs/explanation/rendering/pipeline.md` read, and their encoding
//! from the core display list. Coordinates are `[f32; 2]` physical
//! pixels; the vertex shader divides by viewport size. The layout is
//! committed here so shader and Rust cannot drift.

use bytemuck::{Pod, Zeroable};
use felis_render_core::{
    display_list::{AtlasKind, DecoKind, RowPaint},
    glyphs::AtlasView,
    painter::clip_extent_at,
};

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

#[derive(Debug, Default, PartialEq)]
pub struct InstanceBuffers {
    pub bg: Vec<BgInstance>,
    pub fg: Vec<FgInstance>,
    /// Drawn after the glyph pass so strikethrough sits over the ink.
    pub deco: Vec<DecorationInstance>,
}

impl InstanceBuffers {
    pub(crate) fn clear(&mut self) {
        self.bg.clear();
        self.fg.clear();
        self.deco.clear();
    }
}

/// The pixel sides of the glyph sheets a display list's glyph sources
/// are measured in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AtlasSides {
    mono: f32,
    color: f32,
}

impl AtlasSides {
    pub(crate) fn of(atlas: &impl AtlasView) -> Self {
        Self {
            mono: atlas.sheet_side(AtlasKind::Mono) as f32,
            color: atlas.sheet_side(AtlasKind::Color) as f32,
        }
    }
}

/// Appends `paint` to `out`. The uv divides each source pixel by the
/// side rather than multiplying by its reciprocal, so an unclipped
/// glyph's uv rounds exactly as `alloc / side` does at any side.
pub(crate) fn encode(paint: &RowPaint, sides: AtlasSides, out: &mut InstanceBuffers) {
    out.bg.extend(paint.rects.iter().map(|r| BgInstance {
        origin_px: r.origin_px,
        size_px: r.size_px,
        color: r.color,
    }));
    out.fg.extend(paint.glyphs.iter().map(|g| {
        let (side, is_color) = match g.atlas {
            AtlasKind::Mono => (sides.mono, 0.0),
            AtlasKind::Color => (sides.color, 1.0),
        };
        FgInstance {
            origin_px: g.origin_px,
            size_px: g.size_px,
            uv_min: [g.src_min_px[0] / side, g.src_min_px[1] / side],
            uv_max: [g.src_max_px[0] / side, g.src_max_px[1] / side],
            color: g.color,
            is_color,
            _pad: [0.0; 3],
        }
    }));
    out.deco
        .extend(paint.decos.iter().map(|d| DecorationInstance {
            origin_px: d.origin_px,
            size_px: d.size_px,
            color: d.color,
            kind: match d.kind {
                DecoKind::Solid => DECO_SOLID,
                DecoKind::Dotted => DECO_DOTTED,
                DecoKind::Dashed => DECO_DASHED,
                DecoKind::Curly => DECO_CURLY,
            },
            thickness_px: d.thickness_px,
            period_px: d.period_px,
            _pad: 0.0,
        }));
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

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use felis_render_core::display_list::{Decoration, GlyphDraw, Rect};

    use super::*;

    #[test]
    fn encode_divides_glyph_sources_by_their_own_sheet_side() {
        let sides = AtlasSides {
            mono: 256.0,
            color: 128.0,
        };
        let glyph = |atlas| GlyphDraw {
            origin_px: [3.0, 4.0],
            size_px: [8.0, 16.0],
            atlas,
            src_min_px: [32.0, 64.0],
            src_max_px: [40.0, 80.0],
            color: [0.5; 4],
        };
        let paint = RowPaint {
            rects: vec![Rect {
                origin_px: [1.0, 2.0],
                size_px: [8.0, 16.0],
                color: [0.25; 4],
            }],
            glyphs: vec![glyph(AtlasKind::Mono), glyph(AtlasKind::Color)],
            decos: vec![Decoration {
                origin_px: [0.0, 14.0],
                size_px: [8.0, 1.0],
                color: [1.0; 4],
                kind: DecoKind::Curly,
                thickness_px: 1.0,
                period_px: 8.0,
            }],
        };
        let mut out = InstanceBuffers::default();
        encode(&paint, sides, &mut out);
        assert_eq!(
            out.bg,
            [BgInstance {
                origin_px: [1.0, 2.0],
                size_px: [8.0, 16.0],
                color: [0.25; 4],
            }]
        );
        let [mono, color] = out.fg.as_slice() else {
            panic!("two glyphs: {:?}", out.fg);
        };
        assert_eq!(
            (mono.uv_min, mono.uv_max),
            ([0.125, 0.25], [0.15625, 0.3125])
        );
        assert_eq!(mono.is_color, 0.0);
        assert_eq!((color.uv_min, color.uv_max), ([0.25, 0.5], [0.3125, 0.625]));
        assert_eq!(color.is_color, 1.0);
        assert_eq!(out.deco[0].kind, DECO_CURLY);
        assert_eq!(out.deco[0].period_px, 8.0);
    }

    /// The uv the painter wrote before glyph sources moved to atlas
    /// pixels: normalize the allocation first, then interpolate.
    struct UvOracle {
        min: [f32; 2],
        max: [f32; 2],
    }

    impl UvOracle {
        fn new(side: u32, [x, y, w, h]: [u32; 4]) -> Self {
            let side = side as f32;
            Self {
                min: [x as f32 / side, y as f32 / side],
                max: [(x + w) as f32 / side, (y + h) as f32 / side],
            }
        }

        fn slice_x(&mut self, t0: f32, t1: f32) {
            let span = self.max[0] - self.min[0];
            self.max[0] = span.mul_add(t1, self.min[0]);
            self.min[0] = span.mul_add(t0, self.min[0]);
        }

        fn clip_x(&mut self, kept: f32) {
            self.max[0] = (self.max[0] - self.min[0]).mul_add(kept, self.min[0]);
        }

        fn clip_y(&mut self, kept: f32) {
            self.max[1] = (self.max[1] - self.min[1]).mul_add(kept, self.min[1]);
        }
    }

    /// The painter's slice, then the chrome clip, then the overlay's
    /// right-edge clip, on atlas pixels; encoded against `side`.
    fn cropped_uv(side: u32, [x, y, w, h]: [u32; 4], cuts: [f32; 4]) -> ([f32; 2], [f32; 2]) {
        let [t0, t1, kept_y, kept_x] = cuts;
        let mut glyph = GlyphDraw {
            origin_px: [0.0; 2],
            size_px: [w as f32, h as f32],
            atlas: AtlasKind::Mono,
            src_min_px: [x as f32, y as f32],
            src_max_px: [(x + w) as f32, (y + h) as f32],
            color: [1.0; 4],
        };
        glyph.crop_src_x(t0, t1);
        glyph.crop_src_y(0.0, kept_y);
        glyph.crop_src_x(0.0, kept_x);
        let sides = AtlasSides {
            mono: side as f32,
            color: side as f32,
        };
        let mut out = InstanceBuffers::default();
        encode(
            &RowPaint {
                glyphs: vec![glyph],
                ..RowPaint::default()
            },
            sides,
            &mut out,
        );
        (out.fg[0].uv_min, out.fg[0].uv_max)
    }

    fn oracle_uv(side: u32, alloc: [u32; 4], cuts: [f32; 4]) -> ([f32; 2], [f32; 2]) {
        let [t0, t1, kept_y, kept_x] = cuts;
        let mut uv = UvOracle::new(side, alloc);
        uv.slice_x(t0, t1);
        uv.clip_y(kept_y);
        uv.clip_x(kept_x);
        (uv.min, uv.max)
    }

    fn alloc_in(side: u32, [x, y, w, h]: [u32; 4]) -> [u32; 4] {
        let (w, h) = (w.min(side), h.min(side));
        [x % (side - w + 1), y % (side - h + 1), w, h]
    }

    fn cuts() -> impl proptest::strategy::Strategy<Value = [f32; 4]> {
        use proptest::strategy::Strategy as _;
        (0.0f32..=1.0, 0.0f32..=1.0, 0.0f32..=1.0, 0.0f32..=1.0)
            .prop_map(|(t0, dt, y, x)| [t0, dt.mul_add(1.0 - t0, t0), y, x])
    }

    proptest::proptest! {
        /// Cropping atlas pixels and dividing by the side at encode lands
        /// within 1e-6 of interpolating normalized uv, at any side a
        /// device may report.
        #[test]
        fn cropping_pixels_matches_cropping_uv_at_any_atlas_side(
            side in 256u32..=2048,
            alloc in proptest::array::uniform4(1u32..2048),
            cuts in cuts(),
        ) {
            let alloc = alloc_in(side, alloc);
            let (got_min, got_max) = cropped_uv(side, alloc, cuts);
            let (want_min, want_max) = oracle_uv(side, alloc, cuts);
            for (got, want) in got_min.into_iter().chain(got_max).zip(want_min.into_iter().chain(want_max)) {
                proptest::prop_assert!((got - want).abs() < 1e-6, "{got} vs {want}");
            }
        }

        /// Dividing by a power of two is exact, so at those sides the two
        /// orders round identically.
        #[test]
        fn cropping_pixels_matches_cropping_uv_bit_for_bit_at_power_of_two_sides(
            log2_side in 8u32..=11,
            alloc in proptest::array::uniform4(1u32..2048),
            cuts in cuts(),
        ) {
            let side = 1 << log2_side;
            let alloc = alloc_in(side, alloc);
            proptest::prop_assert_eq!(cropped_uv(side, alloc, cuts), oracle_uv(side, alloc, cuts));
        }
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
    fn an_image_entirely_inside_the_chrome_row_is_dropped() {
        assert_eq!(clip_img_quad_above(img_quad(32.0, 16.0), 32.0), None);
        assert_eq!(clip_img_quad_above(img_quad(48.0, 16.0), 32.0), None);
    }

    #[test]
    fn an_image_clear_of_the_chrome_row_is_untouched() {
        let quad = img_quad(0.0, 32.0);
        assert_eq!(clip_img_quad_above(quad, 32.0), Some(quad));
    }

    proptest::proptest! {
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
}

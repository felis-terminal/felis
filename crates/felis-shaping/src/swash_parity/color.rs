//! Color glyphs against swash: bitmap strikes byte for byte, `COLR`
//! layers against swash's layer outlines composited without the two swash
//! compositing defects felis does not reproduce (a canvas that drops the
//! bottom row or right column, and edges blended toward transparent black).

use swash::{
    scale::ScaleContext,
    zeno::{Mask, Origin},
};
#[cfg(not(windows))]
use swash::{
    scale::{Render, Source, StrikeWith, image::Content},
    zeno::Format,
};

use crate::{Font, GlyphBitmap, GlyphId, GlyphPixels, raster::Rasterizer};

/// Straight RGBA with its top-left corner at `(left, top)`, y up.
struct Image {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    rgba: Vec<u8>,
}

impl Image {
    fn of(bitmap: &GlyphBitmap) -> Self {
        let GlyphPixels::Rgba(rgba) = bitmap.pixels() else {
            panic!("a color bitmap");
        };
        Self {
            left: bitmap.left,
            top: bitmap.top,
            width: bitmap.width().cast_signed(),
            height: bitmap.height().cast_signed(),
            rgba: rgba.clone(),
        }
    }

    fn at(&self, x: i32, y: i32) -> [u8; 4] {
        let (col, row) = (x - self.left, self.top - 1 - y);
        if !(0..self.width).contains(&col) || !(0..self.height).contains(&row) {
            return [0; 4];
        }
        let i = ((row * self.width + col) * 4) as usize;
        self.rgba[i..i + 4].try_into().expect("four channels")
    }

    /// `(left, top, width, height)` of the pixels with alpha.
    fn ink_box(&self) -> Option<(i32, i32, i32, i32)> {
        let inked =
            |col: i32, row: i32| self.rgba[((row * self.width + col) * 4 + 3) as usize] != 0;
        let rows: Vec<i32> = (0..self.height)
            .filter(|&r| (0..self.width).any(|c| inked(c, r)))
            .collect();
        let cols: Vec<i32> = (0..self.width)
            .filter(|&c| (0..self.height).any(|r| inked(c, r)))
            .collect();
        let (r0, r1, c0, c1) = (*rows.first()?, *rows.last()?, *cols.first()?, *cols.last()?);
        Some((self.left + c0, self.top - r0, c1 - c0 + 1, r1 - r0 + 1))
    }
}

#[derive(Default)]
struct Parity {
    renders: usize,
    samples: usize,
    within: usize,
    worst: u8,
    worst_at: String,
    worst_box: i32,
    worst_box_at: String,
}

impl Parity {
    /// Each channel of each pixel either inks, composited over black and
    /// over white.
    #[expect(
        clippy::suboptimal_flops,
        reason = "a reference, in the textbook order"
    )]
    fn compare(&mut self, label: &str, ours: &Image, reference: &Image) {
        self.renders += 1;
        let boxes = match (ours.ink_box(), reference.ink_box()) {
            (Some(a), Some(b)) => [a.0 - b.0, a.1 - b.1, a.2 - b.2, a.3 - b.3]
                .into_iter()
                .map(i32::abs)
                .max()
                .unwrap_or(0),
            (None, None) => 0,
            _ => i32::MAX,
        };
        if boxes > self.worst_box {
            self.worst_box = boxes;
            self.worst_box_at = label.to_owned();
        }
        let (x0, x1) = (
            ours.left.min(reference.left),
            (ours.left + ours.width).max(reference.left + reference.width),
        );
        let (y0, y1) = (
            (ours.top - ours.height).min(reference.top - reference.height),
            ours.top.max(reference.top),
        );
        for y in y0..y1 {
            for x in x0..x1 {
                let (a, b) = (ours.at(x, y), reference.at(x, y));
                if a[3] == 0 && b[3] == 0 {
                    continue;
                }
                for background in [0.0_f32, 255.0] {
                    for c in 0..3 {
                        let show = |p: [u8; 4]| {
                            let alpha = f32::from(p[3]) / 255.0;
                            f32::from(p[c]) * alpha + background * (1.0 - alpha)
                        };
                        #[expect(
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            reason = "a difference of two values in 0..=255"
                        )]
                        let delta = (show(a) - show(b)).abs().round() as u8;
                        self.samples += 1;
                        self.within += usize::from(delta <= 8);
                        if delta > self.worst {
                            self.worst = delta;
                            self.worst_at = format!("{label} at ({x}, {y})");
                        }
                    }
                }
            }
        }
    }

    fn assert_gate(&self, mode: &str) {
        let report = format!(
            "{mode}: {} renders, {} samples, {:.4}% within 8/255, worst {}/255 ({}), worst ink box Δ {} ({})",
            self.renders,
            self.samples,
            self.within as f64 * 100.0 / self.samples.max(1) as f64,
            self.worst,
            self.worst_at,
            self.worst_box,
            self.worst_box_at,
        );
        eprintln!("{report}");
        assert!(self.renders > 0, "{report}");
        assert!(self.within * 1000 >= self.samples * 999, "{report}");
        assert!(self.worst <= 32, "{report}");
        assert!(self.worst_box <= 1, "{report}");
    }
}

/// swash's own layer outlines, rasterized by zeno and composited
/// source-over, premultiplied, into the union of the layer boxes.
#[expect(
    clippy::suboptimal_flops,
    reason = "a reference, in the textbook order"
)]
fn reference(
    ctx: &mut ScaleContext,
    font: &Font,
    glyph: GlyphId,
    px: f32,
    hint: bool,
) -> Option<Image> {
    let palette: Vec<[u8; 4]> = font
        .face
        .palettes()
        .get(0)?
        .colors()
        .iter()
        .map(|c| [c.red, c.green, c.blue, c.alpha])
        .collect();
    let coords = font.swash_coords();
    let mut scaler = ctx
        .builder(font.swash_ref())
        .size(px)
        .hint(hint)
        .normalized_coords(&coords)
        .build();
    let outline = scaler.scale_color_outline(u16::try_from(glyph).ok()?)?;
    let mut layers = Vec::new();
    for i in 0..outline.len() {
        let layer = outline.get(i)?;
        let (data, placement) = Mask::new(layer.path()).origin(Origin::BottomLeft).render();
        let color = layer
            .color_index()
            .and_then(|i| palette.get(usize::from(i)).copied())
            .unwrap_or([128, 128, 128, 255]);
        if placement.width > 0 && placement.height > 0 {
            // zeno reports the bottom edge as `top` unless the mask was
            // inspected first.
            let (width, height) = (
                placement.width.cast_signed(),
                placement.height.cast_signed(),
            );
            layers.push((
                placement.left,
                placement.top + height,
                width,
                height,
                data,
                color,
            ));
        }
    }
    let left = layers.iter().map(|l| l.0).min()?;
    let top = layers.iter().map(|l| l.1).max()?;
    let right = layers.iter().map(|l| l.0 + l.2).max()?;
    let bottom = layers.iter().map(|l| l.1 - l.3).min()?;
    let (width, height) = (right - left, top - bottom);
    let mut canvas = vec![[0.0_f32; 4]; (width * height) as usize];
    for (l_left, l_top, l_width, l_height, data, color) in &layers {
        for row in 0..*l_height {
            for col in 0..*l_width {
                let k = data[(row * l_width + col) as usize];
                if k == 0 {
                    continue;
                }
                let at = ((top - l_top + row) * width + l_left - left + col) as usize;
                let alpha = f32::from(k) / 255.0 * f32::from(color[3]) / 255.0;
                let pixel = &mut canvas[at];
                for c in 0..3 {
                    pixel[c] = f32::from(color[c]) / 255.0 * alpha + pixel[c] * (1.0 - alpha);
                }
                pixel[3] = alpha + pixel[3] * (1.0 - alpha);
            }
        }
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "unit values scaled to 0..=255"
    )]
    let rgba = canvas
        .iter()
        .flat_map(|p| {
            if p[3] == 0.0 {
                return [0; 4];
            }
            let byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
            [
                byte(p[0] / p[3]),
                byte(p[1] / p[3]),
                byte(p[2] / p[3]),
                byte(p[3]),
            ]
        })
        .collect();
    Some(Image {
        left,
        top,
        width,
        height,
        rgba,
    })
}

/// swash's color render as felis drew it before: `COLR`, then strikes.
#[cfg(not(windows))]
fn swash_color(
    ctx: &mut ScaleContext,
    font: &Font,
    glyph: GlyphId,
    px: f32,
    hint: bool,
) -> Option<swash::scale::image::Image> {
    let coords = font.swash_coords();
    let mut scaler = ctx
        .builder(font.swash_ref())
        .size(px)
        .hint(hint)
        .normalized_coords(&coords)
        .build();
    Render::new(&[
        Source::ColorOutline(0),
        Source::ColorBitmap(StrikeWith::BestFit),
    ])
    .format(Format::Alpha)
    .render(&mut scaler, u16::try_from(glyph).ok()?)
}

/// Strike glyphs whose placement, size and bytes all equal swash's.
#[cfg(not(windows))]
fn assert_strikes_match(font: &Font, glyphs: &[GlyphId], sizes: &[f32]) {
    let mut ctx = ScaleContext::new();
    let mut rasterizer = Rasterizer::default();
    let mut renders = 0;
    for &px in sizes {
        for &glyph in glyphs {
            let theirs =
                swash_color(&mut ctx, font, glyph, px, false).expect("swash draws the strike");
            assert!(matches!(theirs.content, Content::Color));
            let ours = rasterizer.rasterize(font, glyph, px);
            let p = theirs.placement;
            assert_eq!(
                (ours.left, ours.top, ours.width(), ours.height()),
                (p.left, p.top, p.width, p.height),
                "glyph {glyph} at {px} px"
            );
            assert!(
                ours.pixels().as_bytes() == theirs.data,
                "glyph {glyph} at {px} px"
            );
            renders += 1;
        }
    }
    assert!(renders > 0);
}

#[cfg(not(windows))]
#[test]
fn cbdt_strikes_match_swash_byte_for_byte() {
    use crate::goldens::{CBDT_SIZES, strike_glyphs};
    let font = crate::test_support::pinned("NotoColorEmoji.ttf");
    let glyphs: Vec<GlyphId> = strike_glyphs(&font).into_iter().step_by(9).collect();
    assert!(glyphs.len() > 300, "{}", glyphs.len());
    assert_strikes_match(&font, &glyphs, &CBDT_SIZES);
}

/// `F` to `K`: RGB, gray, gray with alpha, a palette without and with
/// `tRNS`, and 2-bit gray. The probe's 16-bit `L` is left out: swash's
/// decoder indexes past the end of a 16-bit row and panics.
#[cfg(not(windows))]
#[test]
fn png_strikes_of_every_color_type_match_swash_byte_for_byte() {
    let font = crate::test_support::color_probe("FelisBitmapProbe.ttf");
    let glyphs: Vec<GlyphId> = "FGHIJK"
        .chars()
        .map(|c| font.glyph_for(c).expect("the probe maps it"))
        .collect();
    assert_strikes_match(&font, &glyphs, &[10.0, 13.0, 20.0, 26.5, 32.0]);
}

/// Each layered glyph against the reference, at `hint` for swash.
fn colr_parity(
    font: &Font,
    glyphs: &[GlyphId],
    sizes: impl Iterator<Item = f32> + Clone,
    hint: bool,
) -> Parity {
    let mut ctx = ScaleContext::new();
    let mut rasterizer = Rasterizer::default();
    let mut parity = Parity::default();
    for px in sizes {
        for &glyph in glyphs {
            let ours = rasterizer.rasterize(font, glyph, px);
            if ours.width() == 0 {
                continue;
            }
            let reference =
                reference(&mut ctx, font, glyph, px, hint).expect("swash has the layers");
            parity.compare(
                &format!("glyph {glyph} at {px} px"),
                &Image::of(&ours),
                &reference,
            );
        }
    }
    parity
}

/// Unhinted, and hinted as Windows hints every face; swash has no
/// counterpart of fontconfig's slight and full targets, which the color
/// golden pins instead.
#[cfg(not(windows))]
#[test]
fn colr_layers_match_swash_layers_composited_source_over() {
    use crate::{goldens::with_raster, raster::RasterOptions};
    let probe = crate::test_support::color_probe("FelisColrProbe.ttf");
    let glyphs: Vec<GlyphId> = probe
        .face
        .charmap()
        .unicodes()
        .map(|(_, g)| g.to_u32())
        .collect();
    assert!(glyphs.len() >= 12);
    for (mode, raster, hint) in [
        ("unhinted", RasterOptions::NEVER_HINTED, false),
        ("lcd", RasterOptions::hinted(), true),
    ] {
        let font = with_raster(&probe, raster);
        colr_parity(&font, &glyphs, (10..=40).map(|px| px as f32), hint).assert_gate(mode);
    }
}

/// swash's canvas drops ink below its last row or right of its last
/// column, and its alpha never reaches 255; felis keeps both.
#[cfg(not(windows))]
#[test]
fn colr_layers_keep_the_ink_and_opacity_swash_dropped() {
    let font = crate::goldens::with_raster(
        &crate::test_support::color_probe("FelisColrProbe.ttf"),
        crate::raster::RasterOptions::NEVER_HINTED,
    );
    let mut ctx = ScaleContext::new();
    let mut rasterizer = Rasterizer::default();
    let (mut renders, mut swash_clipped, mut swash_opaque) = (0, 0, 0);
    for (_, glyph) in font.face.charmap().unicodes() {
        let glyph = glyph.to_u32();
        for px in 16..=40 {
            let px = px as f32;
            let ours = Image::of(&rasterizer.rasterize(&font, glyph, px));
            let theirs = swash_color(&mut ctx, &font, glyph, px, false).expect("swash draws it");
            let p = theirs.placement;
            let theirs = Image {
                left: p.left,
                top: p.top,
                width: p.width.cast_signed(),
                height: p.height.cast_signed(),
                rgba: theirs.data,
            };
            renders += 1;
            assert!(
                ours.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 255),
                "glyph {glyph} at {px} px"
            );
            swash_opaque += usize::from(theirs.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 255));
            let dropped = (ours.top - ours.height..ours.top).any(|y| {
                (ours.left..ours.left + ours.width)
                    .any(|x| ours.at(x, y)[3] > 8 && theirs.at(x, y)[3] == 0)
            });
            swash_clipped += usize::from(dropped);
        }
    }
    assert_eq!(swash_opaque, 0);
    assert!(swash_clipped * 2 > renders, "{swash_clipped} of {renders}");
}

/// Apple Color Emoji's `sbix` strikes, placement included.
#[cfg(target_os = "macos")]
#[test]
fn apple_color_emoji_strikes_match_swash_byte_for_byte() {
    let font = crate::test_support::load_path(std::path::Path::new(
        "/System/Library/Fonts/Apple Color Emoji.ttc",
    ));
    let mut glyphs: Vec<GlyphId> = Vec::new();
    for text in ["😀", "❤\u{FE0F}", "🇯🇵", "👍🏽", "#\u{FE0F}\u{20E3}"] {
        let shaped = crate::Shaper::new().shape_run(&font, 32.0, &[], text);
        glyphs.extend(shaped.iter().map(|g| g.glyph_id));
    }
    let strikes = font
        .face
        .bitmap_strikes_with(skrifa::GlyphSourceOrder::COLOR_BITMAPS);
    glyphs.extend(
        (0..font.face.num_glyphs())
            .filter(|&g| {
                strikes
                    .select_bitmap(skrifa::GlyphId::new(g), None)
                    .is_some()
            })
            .step_by(50),
    );
    assert_strikes_match(&font, &glyphs, &[13.0, 16.0, 20.5, 26.5, 40.0]);
}

/// Segoe UI Emoji's `COLR` layers, hinted as Windows draws the face.
#[cfg(windows)]
#[test]
fn segoe_ui_emoji_layers_match_swash_layers_composited_source_over() {
    use skrifa::font::glyph::color::ColorGlyphVersion;
    let fonts = std::env::var_os("WINDIR").map_or_else(
        || std::path::PathBuf::from(r"C:\Windows\Fonts"),
        |dir| std::path::PathBuf::from(dir).join("Fonts"),
    );
    let font = crate::test_support::load_path(&fonts.join("seguiemj.ttf"));
    let colors = font.face.color_glyphs();
    let glyphs: Vec<GlyphId> = (0..font.face.num_glyphs())
        .filter(|&g| {
            colors
                .get_with_version(skrifa::GlyphId::new(g), ColorGlyphVersion::ColrV0)
                .is_some()
        })
        .take(200)
        .collect();
    assert_eq!(glyphs.len(), 200);
    colr_parity(
        &font,
        &glyphs,
        [13.0, 16.0, 20.0, 26.5, 40.0].into_iter(),
        true,
    )
    .assert_gate("lcd");
}

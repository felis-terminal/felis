//! Embedded bitmap glyphs from `sbix` and `CBDT` strikes, decoded and
//! scaled from the strike's size to the size asked for.

use skrifa::{
    GlyphSourceOrder,
    font::glyph::bitmap::{BitmapGlyph, BitmapSource, Pixels},
};

use super::{coverage, resample};
use crate::{Font, GlyphBitmap, GlyphId, GlyphPixels, metadata};

/// Apple Color Emoji leaves `originOffsetY` at zero and expects the image
/// lowered by a tenth of an em, as CoreText draws it.
const APPLE_EMOJI_FAMILY: &str = "Apple Color Emoji";

/// The strike image before scaling: straight RGBA, or coverage widened to
/// RGBA so one resampler serves both.
struct Decoded {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    coverage: bool,
}

/// `None` when the face has no bitmap for `glyph`, the image does not
/// decode, or a buffer would exceed the raster budget.
pub(super) fn render(font: &Font, glyph: GlyphId, px: f32) -> Option<GlyphBitmap> {
    let strikes = font
        .face
        .bitmap_strikes_with(GlyphSourceOrder::COLOR_BITMAPS);
    // Truncated, so a fractional size keeps the strike its whole part
    // names rather than moving up to the next.
    let bitmap = strikes.select_bitmap(skrifa::GlyphId::new(glyph), Some(px.trunc().max(1.0)))?;
    let ppem = f32::from(bitmap.ppem());
    let scale = px / ppem;
    let side = coverage::side_limit(px)?;
    let target = |width: usize, height: usize| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "the per-side limit is checked on the result"
        )]
        let size = (
            (width as f32 * scale) as usize,
            (height as f32 * scale) as usize,
        );
        (size.0 as f32 <= side && size.1 as f32 <= side).then_some(size)
    };
    let unscaled = px.total_cmp(&ppem).is_eq();
    let decoded = decode(&bitmap, |width, height| {
        let (to_width, to_height) = target(width, height)?;
        let source = width.checked_mul(height)?.checked_mul(4)?;
        if unscaled {
            return Some(source);
        }
        let scratch = resample::scratch_len(height, to_width)?;
        let out = to_width.checked_mul(to_height)?.checked_mul(4)?;
        source.checked_add(scratch)?.checked_add(out)
    })?;
    let Decoded {
        width,
        height,
        rgba: source,
        coverage: is_coverage,
    } = decoded;
    let (to_width, to_height) = target(width, height)?;
    let mut rgba = if unscaled {
        source
    } else {
        let (mut scratch, mut out) = (Vec::new(), Vec::new());
        resample::resample(
            &source,
            (width, height),
            (to_width, to_height),
            &mut scratch,
            &mut out,
        );
        drop(source);
        out
    };
    let (x, y) = placement(font, &bitmap, height);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "bearings are 16-bit and the scale is bounded by the side limit"
    )]
    let (left, top) = ((x as f32 * scale) as i32, (y as f32 * scale) as i32);
    let pixels = if is_coverage {
        let len = to_width * to_height;
        for i in 0..len {
            rgba[i] = rgba[i * 4 + 3];
        }
        rgba.truncate(len);
        coverage::shrink_within(&mut rgba);
        GlyphPixels::Coverage(rgba)
    } else {
        GlyphPixels::Rgba(rgba)
    };
    Some(GlyphBitmap::new(
        u32::try_from(to_width).ok()?,
        u32::try_from(to_height).ok()?,
        left,
        top,
        pixels,
    ))
}

/// The strike-pixel offset of the image's top left corner from the glyph
/// origin, y up.
fn placement(font: &Font, bitmap: &BitmapGlyph<'_>, height: usize) -> (i32, i32) {
    let x = bitmap.inner_x_bearing();
    if bitmap.source() != BitmapSource::Sbix {
        return (x, bitmap.inner_y_bearing());
    }
    let height = i32::try_from(height).unwrap_or(i32::MAX);
    (
        x,
        sbix_top(font, bitmap.inner_y_bearing(), bitmap.ppem()).saturating_add(height),
    )
}

/// An `sbix` image's bottom edge, in strike pixels above the baseline.
fn sbix_top(font: &Font, origin_offset_y: i32, ppem: u16) -> i32 {
    if origin_offset_y != 0
        || metadata::english_name(&font.face, skrifa::raw::types::NameId::FAMILY_NAME).as_deref()
            != Some(APPLE_EMOJI_FAMILY)
    {
        return origin_offset_y;
    }
    let units_per_em = f32::from(font.face.units_per_em());
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a tenth of an em in pixels of a 16-bit strike"
    )]
    let lowered = (-100.0 * f32::from(ppem) / units_per_em).round() as i32;
    lowered
}

/// `charge(width, height)` is every byte the image at that size will cost,
/// asked before the image is allocated; `None` refuses it.
fn decode(
    bitmap: &BitmapGlyph<'_>,
    charge: impl Fn(usize, usize) -> Option<usize>,
) -> Option<Decoded> {
    match bitmap.pixels() {
        Pixels::Png(data) => decode_png(data, charge),
        Pixels::Bgra(bgra) => {
            let (width, height) = (bgra.width() as usize, bgra.height() as usize);
            within(charge(width, height)?)?;
            let mut rgba = Vec::with_capacity(width * height * 4);
            rgba.extend(
                bgra.data()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .take(width * height)
                    .flat_map(|&[b, g, r, a]| unpremultiply([r, g, b, a])),
            );
            if rgba.len() != width * height * 4 {
                return None;
            }
            Some(Decoded {
                width,
                height,
                rgba,
                coverage: false,
            })
        }
        Pixels::Mask(mask) => {
            let (width, height) = (mask.width() as usize, mask.height() as usize);
            within(charge(width, height)?)?;
            let mut rgba = Vec::with_capacity(width * height * 4);
            rgba.extend(mask.pixels().flat_map(|a| [255, 255, 255, a]));
            Some(Decoded {
                width,
                height,
                rgba,
                coverage: true,
            })
        }
    }
}

fn within(bytes: usize) -> Option<()> {
    (bytes <= coverage::IMAGE_BUDGET).then_some(())
}

/// The IHDR size is charged before anything image-sized is allocated, and
/// the decoder's own buffers are held to what the budget leaves.
fn decode_png(data: &[u8], charge: impl Fn(usize, usize) -> Option<usize>) -> Option<Decoded> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    let info = decoder.read_header_info().ok()?;
    let (width, height) = (info.width as usize, info.height as usize);
    let charged = charge(width, height)?;
    within(charged)?;
    decoder.set_limits(png::Limits {
        bytes: coverage::IMAGE_BUDGET - charged,
    });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let len = width * height * 4;
    if reader.output_buffer_size()? > len {
        return None;
    }
    let mut rgba = vec![0; len];
    let frame = reader.next_frame(&mut rgba).ok()?;
    if (frame.width as usize, frame.height as usize) != (width, height)
        || frame.bit_depth != png::BitDepth::Eight
    {
        return None;
    }
    let channels = match frame.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None,
    };
    // Widened back to front, so each pixel is read before the wider
    // pixels after it overwrite its bytes.
    if channels < 4 {
        for i in (0..width * height).rev() {
            let pixel = match rgba[channels * i..][..channels] {
                [gray] => [gray, gray, gray, 255],
                [gray, alpha] => [gray, gray, gray, alpha],
                [red, green, blue] => [red, green, blue, 255],
                _ => return None,
            };
            rgba[4 * i..4 * i + 4].copy_from_slice(&pixel);
        }
    }
    Some(Decoded {
        width,
        height,
        rgba,
        coverage: false,
    })
}

/// Premultiplied to straight, rounded; zero alpha is all zeros.
pub(super) fn unpremultiply([red, green, blue, alpha]: [u8; 4]) -> [u8; 4] {
    if alpha == 0 {
        return [0; 4];
    }
    let straight = |channel: u8| {
        let value = (u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha);
        u8::try_from(value.min(255)).unwrap_or(u8::MAX)
    };
    [straight(red), straight(green), straight(blue), alpha]
}

#[cfg(test)]
#[cfg(not(windows))]
mod tests {
    use crate::{
        GlyphBitmap, GlyphPixels,
        raster::Rasterizer,
        test_support::{color_probe, pinned},
    };

    fn draw(face: &str, c: char, px: f32) -> GlyphBitmap {
        let font = color_probe(face);
        let glyph = font.glyph_for(c).expect("the probe maps it");
        Rasterizer::default().rasterize(&font, glyph, px)
    }

    fn rgba(bitmap: &GlyphBitmap) -> &[u8] {
        let GlyphPixels::Rgba(rgba) = bitmap.pixels() else {
            panic!("a color bitmap");
        };
        rgba
    }

    #[test]
    fn a_png_strike_with_translucent_edges_decodes_straight() {
        let a = draw("FelisBitmapProbe.ttf", 'A', 20.0);
        let edges: Vec<&[u8; 4]> = rgba(&a)
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] != 0 && p[3] != 255)
            .collect();
        assert!(!edges.is_empty(), "the disc has a soft edge");
        for pixel in edges {
            assert_eq!(pixel[..3], [230, 40, 30], "{pixel:?}");
        }
    }

    #[test]
    fn a_premultiplied_bgra_strike_becomes_straight_rgba() {
        let b = draw("FelisBitmapProbe.ttf", 'B', 20.0);
        assert_eq!(
            rgba(&b),
            [0x20, 0x40, 0x80, 0x80, 0, 0, 0, 0].repeat(2),
            "swizzled and unpremultiplied; zero alpha is all zeros",
        );
    }

    #[test]
    fn a_png_header_over_the_budget_falls_through_to_the_outline() {
        let c = draw("FelisBitmapProbe.ttf", 'C', 20.0);
        assert!(!c.pixels().is_color());
        assert_eq!((c.width(), c.height()), (10, 10));
    }

    /// `L`'s samples are 16-bit: red `x` · 3600, green `y` · 3600, blue
    /// 0x8080 + `x`, and the disc's alpha · 257.
    #[test]
    fn a_16_bit_png_strike_keeps_the_high_byte_of_each_sample() {
        let l = draw("FelisBitmapProbe.ttf", 'L', 20.0);
        assert_eq!((l.width(), l.height()), (18, 18));
        let center = (9 * 18 + 9) * 4;
        assert_eq!(rgba(&l)[center..center + 4], [126, 126, 128, 255]);
    }

    /// `E` is a 3 × 2 one-bit mask of rows 101 and 011.
    #[test]
    fn a_one_bit_mask_strike_draws_as_coverage() {
        let e = draw("FelisBitmapProbe.ttf", 'E', 32.0);
        let GlyphPixels::Coverage(coverage) = e.pixels() else {
            panic!("a mask is coverage");
        };
        assert_eq!((e.width(), e.height()), (3, 2));
        assert_eq!(coverage, &[255, 0, 255, 0, 255, 255]);
    }

    #[test]
    fn an_sbix_dupe_draws_the_glyph_it_names() {
        let a = draw("FelisSbixProbe.ttf", 'A', 20.0);
        let b = draw("FelisSbixProbe.ttf", 'B', 20.0);
        assert_eq!(rgba(&a), rgba(&b));
        assert_eq!((a.left, a.top), (b.left, b.top));
    }

    /// The probe's family is Apple Color Emoji: an image at origin offset
    /// zero sits a tenth of an em lower, an image with an offset of its own
    /// sits at that offset.
    #[test]
    fn apple_color_emoji_lowers_an_image_at_origin_offset_zero() {
        let a = draw("FelisSbixProbe.ttf", 'A', 20.0);
        assert_eq!((a.left, a.top, a.width(), a.height()), (0, 16, 18, 18));
        let c = draw("FelisSbixProbe.ttf", 'C', 20.0);
        assert_eq!((c.left, c.top), (2, 15));
    }

    /// Strikes of 20 and 40 ppem: 20.5 px keeps the 20, 21 px takes the 40.
    #[test]
    fn a_fractional_size_keeps_the_strike_of_its_whole_part() {
        let kept = draw("FelisSbixProbe.ttf", 'A', 20.5);
        assert_eq!((kept.width(), kept.top), (18, 16));
        let next = draw("FelisSbixProbe.ttf", 'A', 21.0);
        assert_eq!((next.width(), next.top), (19, 17));
    }

    #[test]
    fn a_strike_whose_target_is_over_the_budget_draws_blank() {
        let font = pinned("NotoColorEmoji.ttf");
        let glyph = font.glyph_for('😀').expect("the face maps it");
        let mut rasterizer = Rasterizer::default();
        assert!(rasterizer.rasterize(&font, glyph, 64.0).pixels().is_color());
        assert!(rasterizer.rasterize(&font, glyph, 2000.0).is_blank());
    }
}

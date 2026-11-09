//! `color_reference <font> <px> <out> <glyph>...` writes each color glyph
//! as `<out>/<glyph>.img` (a `width height left top` line, `top` up from
//! the baseline, then straight RGBA rows) and its `COLR` clip box in
//! pixels as `<glyph>.clip`, for `dev/color-reference/compare.py`. A glyph
//! drawn as an outline is named on stderr and gets no file.

use std::{io::Write as _, path::PathBuf};

use felis_shaping::{GlyphPixels, rasterize_face_glyphs_unhinted};
use skrifa::font::glyph::color::ColorGlyphVersion;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "color_reference <font> <px> <out> <glyph>...";
    let font = PathBuf::from(args.next().ok_or(usage)?);
    let px: f32 = args.next().ok_or(usage)?.parse()?;
    let out = PathBuf::from(args.next().ok_or(usage)?);
    let glyphs = args.map(|g| g.parse()).collect::<Result<Vec<u32>, _>>()?;
    let bitmaps = rasterize_face_glyphs_unhinted(std::fs::read(&font)?, &glyphs, px)
        .ok_or("the file holds no face")?;
    std::fs::create_dir_all(&out)?;
    let face = skrifa::Font::new(std::fs::read(&font)?, 0).ok_or("the file holds no face")?;
    let scale = px / f32::from(face.units_per_em());
    for &glyph in &glyphs {
        let clip = face
            .color_glyphs()
            .get_with_version(skrifa::GlyphId::new(glyph), ColorGlyphVersion::ColrV1)
            .and_then(|g| g.clip_box());
        if let Some(b) = clip {
            let [x0, y0, x1, y1] = [b.x_min, b.y_min, b.x_max, b.y_max].map(|v| v * scale);
            std::fs::write(
                out.join(format!("{glyph}.clip")),
                format!("{x0} {y0} {x1} {y1}\n"),
            )?;
        }
    }
    for (glyph, bitmap) in glyphs.iter().zip(bitmaps) {
        let GlyphPixels::Rgba(rgba) = bitmap.pixels() else {
            writeln!(std::io::stderr(), "glyph {glyph}: no color glyph")?;
            continue;
        };
        let mut file = std::fs::File::create(out.join(format!("{glyph}.img")))?;
        writeln!(
            file,
            "{} {} {} {}",
            bitmap.width(),
            bitmap.height(),
            bitmap.left,
            bitmap.top
        )?;
        file.write_all(rgba)?;
    }
    Ok(())
}

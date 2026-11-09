//! The outline path against swash's, which it replaces at parity.
#![cfg(not(windows))]

use std::fmt::Write as _;

use skrifa::{HintOptions, RasterTarget, ScalerFont};
use swash::{
    scale::{Render, ScaleContext, Source},
    zeno::Format,
};

use super::{Hinting, OutlineBackend, RasterOptions, SkrifaOutlines, scaler_font};
use crate::{
    FileSet, Font, GlyphBitmap,
    goldens::{Digest, assert_golden},
    provider::{FaceHinting, FaceSource, ResolvedFace},
    test_font_dir,
};

const SYMBOLS: &str = "←↑→↓↔⇒∀∃∅∈∑√∞≈≠≤≥⊕⌘⌥⏎■□▲△●○◆★☆☐☑♠♣♥♦♪✓✔✗✘❯";

fn load(name: &str) -> Font {
    let mut db = FileSet::default();
    db.add_file(test_font_dir().join(name))
        .expect("the pinned face reads");
    Font::load_face_id(&db, 0).expect("the pinned face loads")
}

fn with_raster(font: &Font, raster: RasterOptions) -> Font {
    let mut font = font.with_coords(font.coords.clone(), font.position.clone());
    font.raster = raster;
    font
}

/// Four styles of each outline format in the pinned set, plus the symbol
/// faces at their one style.
fn corpus() -> Vec<(String, Font)> {
    let mut faces = Vec::new();
    for (dir, stem, ext) in [
        ("opentype", "MonaspaceNeon", "otf"),
        ("truetype", "MonaspaceNeonFrozen", "ttf"),
    ] {
        for style in ["Regular", "Bold", "Italic", "BoldItalic"] {
            let name = format!("{dir}/{stem}-{style}.{ext}");
            faces.push((name.clone(), load(&name)));
        }
    }
    let var = load("truetype/Monaspace Neon Var.ttf");
    for (weight, italic) in [(400, false), (700, false), (400, true), (700, true)] {
        let view = var
            .at_style(weight, italic)
            .expect("the variable face moves");
        faces.push((format!("Var wght={weight} italic={italic}"), view));
    }
    for name in ["NotoSansSymbols.ttf", "NotoSansSymbols2-Regular.otf"] {
        faces.push((name.to_owned(), load(name)));
    }
    faces
}

/// `(left, top, width, height)` of the nonzero pixels.
fn ink_box(
    data: &[u8],
    width: i32,
    height: i32,
    left: i32,
    top: i32,
) -> Option<(i32, i32, i32, i32)> {
    let inked = |r: i32, c: i32| data[(r * width + c) as usize] != 0;
    let rows: Vec<i32> = (0..height)
        .filter(|&r| (0..width).any(|c| inked(r, c)))
        .collect();
    let cols: Vec<i32> = (0..width)
        .filter(|&c| (0..height).any(|r| inked(r, c)))
        .collect();
    let (r0, r1, c0, c1) = (*rows.first()?, *rows.last()?, *cols.first()?, *cols.last()?);
    Some((left + c0, top - r0, c1 - c0 + 1, r1 - r0 + 1))
}

#[derive(Default)]
struct Parity {
    glyphs: usize,
    inked_pixels: usize,
    within: usize,
    worst_delta: u8,
    worst_delta_at: String,
    worst_placement: i32,
    worst: String,
}

impl Parity {
    fn compare(&mut self, label: &str, ours: &GlyphBitmap, theirs: &swash::scale::image::Image) {
        let p = theirs.placement;
        let (ow, oh) = (ours.width().cast_signed(), ours.height().cast_signed());
        let (tw, th) = (p.width.cast_signed(), p.height.cast_signed());
        let their_ink = theirs.data.iter().any(|&v| v != 0);
        if ours.is_blank() && !their_ink {
            return;
        }
        self.glyphs += 1;
        let ours_box = (!ours.is_blank()).then_some((ours.left, ours.top, ow, oh));
        let placement = match (ours_box, ink_box(&theirs.data, tw, th, p.left, p.top)) {
            (Some(a), Some(b)) => [a.0 - b.0, a.1 - b.1, a.2 - b.2, a.3 - b.3]
                .into_iter()
                .map(i32::abs)
                .max()
                .unwrap_or(0),
            _ => i32::MAX,
        };
        if placement > self.worst_placement {
            self.worst_placement = placement;
            self.worst = format!(
                "{label}: ours {ours_box:?}, swash ink {:?}",
                ink_box(&theirs.data, tw, th, p.left, p.top)
            );
        }
        let ours_px = ours.pixels().as_bytes();
        let at = |data: &[u8], w: i32, h: i32, left: i32, top: i32, x: i32, y: i32| {
            let (c, r) = (x - left, top - y);
            if (0..w).contains(&c) && (0..h).contains(&r) {
                data[(r * w + c) as usize]
            } else {
                0
            }
        };
        let (x0, x1) = (ours.left.min(p.left), (ours.left + ow).max(p.left + tw));
        let (y0, y1) = ((ours.top - oh).min(p.top - th), ours.top.max(p.top));
        for y in (y0 + 1)..=y1 {
            for x in x0..x1 {
                let a = at(ours_px, ow, oh, ours.left, ours.top, x, y);
                let b = at(&theirs.data, tw, th, p.left, p.top, x, y);
                if a == 0 && b == 0 {
                    continue;
                }
                self.inked_pixels += 1;
                let delta = a.abs_diff(b);
                self.within += usize::from(delta <= 8);
                if delta > self.worst_delta {
                    self.worst_delta = delta;
                    self.worst_delta_at = format!("{label} at ({x},{y})");
                }
            }
        }
    }

    fn report(&self, mode: &str) -> String {
        let mut line = String::new();
        let share = self.within as f64 / self.inked_pixels.max(1) as f64;
        let _ = write!(
            line,
            "{mode}: {} glyphs, {} inked pixels, {:.4}% within 8/255, worst |Δ| {}/255 ({}), worst placement Δ {} px ({})",
            self.glyphs,
            self.inked_pixels,
            share * 100.0,
            self.worst_delta,
            self.worst_delta_at,
            self.worst_placement,
            self.worst,
        );
        line
    }
}

fn differential(hinted: bool) -> Parity {
    let raster = if hinted {
        RasterOptions::hinted()
    } else {
        RasterOptions::NEVER_HINTED
    };
    let mut parity = Parity::default();
    let mut swash_ctx = ScaleContext::new();
    let mut outlines = SkrifaOutlines::default();
    for (name, face) in corpus() {
        let font = with_raster(&face, raster);
        let chars = ('!'..='~')
            .chain(SYMBOLS.chars())
            .filter(|&c| font.has_glyph(c));
        for c in chars {
            let glyph = font.swash_ref().charmap().map(c);
            for px in 10..=40 {
                let px = px as f32;
                let mut scaler = swash_ctx
                    .builder(font.swash_ref())
                    .size(px)
                    .hint(hinted)
                    .normalized_coords(font.swash_coords())
                    .build();
                let theirs = Render::new(&[Source::Outline])
                    .format(Format::Alpha)
                    .render(&mut scaler, glyph)
                    .expect("swash draws every corpus outline");
                let ours = outlines
                    .coverage(&font, u32::from(glyph), px)
                    .expect("an outline");
                parity.compare(&format!("{name} {c:?} {px}px"), &ours, &theirs);
            }
        }
    }
    parity
}

fn assert_parity(parity: &Parity, mode: &str) {
    let report = parity.report(mode);
    eprintln!("{report}");
    assert!(parity.glyphs > 1000, "{report}");
    assert!(parity.worst_placement <= 1, "{report}");
    assert!(parity.within * 100 >= parity.inked_pixels * 99, "{report}");
}

/// Every corpus glyph at 10–40 px, hinted and unhinted, one digest per
/// face, mode and size.
#[test]
fn outlines_match_their_golden() {
    let mut out = String::new();
    let mut outlines = SkrifaOutlines::default();
    for (mode, raster) in [
        ("hinted", RasterOptions::hinted()),
        ("unhinted", RasterOptions::NEVER_HINTED),
    ] {
        for (name, face) in corpus() {
            let font = with_raster(&face, raster);
            let glyphs: Vec<u32> = ('!'..='~')
                .chain(SYMBOLS.chars())
                .filter_map(|c| font.glyph_for(c))
                .collect();
            for px in 10..=40 {
                let mut digest = Digest::default();
                for &glyph in &glyphs {
                    let bitmap = outlines
                        .coverage(&font, glyph, px as f32)
                        .unwrap_or_else(GlyphBitmap::blank);
                    digest.bitmap(&bitmap);
                }
                let _ = writeln!(out, "{name} {mode} {px} {}", digest.hex());
            }
        }
    }
    assert_golden("outlines.txt", &out);
}

#[test]
fn hinted_outlines_match_swash_within_tolerance() {
    assert_parity(&differential(true), "hinted (Lcd + preserve)");
}

#[test]
fn unhinted_outlines_match_swash_without_hinting_within_tolerance() {
    assert_parity(&differential(false), "unhinted");
}

/// A face renamed onto the hint-reliant name list, so skrifa treats it
/// as one.
fn hint_reliant_face() -> Font {
    let mut bytes = std::fs::read(test_font_dir().join("truetype/MonaspaceNeonFrozen-Regular.ttf"))
        .expect("the pinned face reads");
    let utf16 = |s: &str| {
        s.encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<u8>>()
    };
    for (from, to) in [
        (utf16("Monaspace"), utf16("MingLiUxx")),
        (b"Monaspace".to_vec(), b"MingLiUxx".to_vec()),
    ] {
        let mut i = 0;
        while let Some(at) = bytes[i..].windows(from.len()).position(|w| w == from) {
            bytes[i + at..i + at + to.len()].copy_from_slice(&to);
            i += at + to.len();
        }
    }
    Font::load_resolved(ResolvedFace {
        source: FaceSource::Shared(std::sync::Arc::new(bytes)),
        index: 0,
        instance: None,
        slanted: false,
        hinting: None,
    })
    .expect("the renamed face loads")
}

#[test]
fn unhinted_raster_leaves_a_hint_reliant_face_unhinted() {
    let face = hint_reliant_face();
    assert!(
        face.face.is_hint_reliant(),
        "the fixture must be hint-reliant"
    );
    assert!(
        ScalerFont::unhinted(face.face.clone(), Some(16.0))
            .hint_options()
            .is_some(),
        "skrifa's unhinted scaler hints a hint-reliant face",
    );
    let font = with_raster(&face, RasterOptions::NEVER_HINTED);
    assert_eq!(scaler_font(&font, 16.0).hint_options(), None);
}

#[test]
fn hintnone_still_hints_a_hint_reliant_face() {
    let none = RasterOptions::for_face(Some(FaceHinting::None));
    let tricky = with_raster(&hint_reliant_face(), none);
    assert!(scaler_font(&tricky, 16.0).hint_options().is_some());
    let plain = with_raster(&load("truetype/MonaspaceNeonFrozen-Regular.ttf"), none);
    assert_eq!(scaler_font(&plain, 16.0).hint_options(), None);
}

#[test]
fn fontconfig_hint_styles_map_to_skrifa_targets() {
    let hinted = |hinting| match RasterOptions::for_face(Some(hinting)).hinting {
        Hinting::Hinted(options) => options,
        other => panic!("{hinting:?} draws {other:?}"),
    };
    assert_eq!(
        RasterOptions::for_face(Some(FaceHinting::None)).hinting,
        Hinting::Unhinted
    );
    for (autohint, engine) in [
        (false, HintOptions::auto_fallback()),
        (true, HintOptions::auto()),
    ] {
        assert_eq!(
            hinted(FaceHinting::Slight { autohint }),
            engine
                .with_target(RasterTarget::Light)
                .with_preserve_linear_metrics(true)
        );
        assert_eq!(
            hinted(FaceHinting::Full { autohint }),
            engine.with_target(RasterTarget::Normal)
        );
    }
}

#[test]
fn a_face_outside_fontconfig_draws_in_the_platform_mode() {
    let expected = if cfg!(target_os = "macos") {
        RasterOptions::NEVER_HINTED
    } else {
        RasterOptions::for_face(Some(FaceHinting::Slight { autohint: false }))
    };
    assert_eq!(RasterOptions::for_face(None), expected);
}

/// The variable file carries no instructions, so both styles run the
/// automatic hinter, which `hintfull` lets snap `m`'s stems horizontally.
#[test]
fn hintslight_and_hintfull_draw_different_bitmaps() {
    let face = load("truetype/Monaspace Neon Var.ttf")
        .at_style(400, false)
        .expect("the variable face moves");
    let draw = |hinting| {
        let font = with_raster(&face, RasterOptions::for_face(Some(hinting)));
        let glyph = font.swash_ref().charmap().map('m');
        SkrifaOutlines::default()
            .coverage(&font, u32::from(glyph), 15.0)
            .expect("an outline")
    };
    let slight = draw(FaceHinting::Slight { autohint: false });
    let full = draw(FaceHinting::Full { autohint: false });
    assert_ne!(
        (slight.left, slight.width(), slight.pixels().as_bytes()),
        (full.left, full.width(), full.pixels().as_bytes())
    );
}

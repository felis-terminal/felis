//! The outline path pinned by its golden, and the hinting each mode picks.
#![cfg(not(windows))]

use std::fmt::Write as _;

use skrifa::{HintOptions, RasterTarget, ScalerFont};

use super::{Hinting, OutlineBackend, RasterOptions, SkrifaOutlines, scaler_font};
use crate::{
    FileSet, Font, GlyphBitmap,
    goldens::{Digest, assert_golden},
    provider::{FaceHinting, FaceSource, ResolvedFace},
    test_font_dir,
};

const SYMBOLS: &str = "←↑→↓↔⇒∀∃∅∈∑√∞≈≠≤≥⊕⌘⌥⏎■□▲△●○◆★☆☐☑♠♣♥♦♪✓✔✗✘❯";

pub(super) fn load(name: &str) -> Font {
    let mut db = FileSet::default();
    db.add_file(test_font_dir().join(name))
        .expect("the pinned face reads");
    Font::load_face_id(&db, 0).expect("the pinned face loads")
}

pub(super) fn with_raster(font: &Font, raster: RasterOptions) -> Font {
    let mut font = font.with_coords(font.coords.clone(), font.position.clone());
    font.raster = raster;
    font
}

/// Four styles of each outline format in the pinned set, plus the symbol
/// faces at their one style.
pub(super) fn corpus() -> Vec<(String, Font)> {
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
    #[cfg(target_os = "macos")]
    let expected = RasterOptions {
        hinting: Hinting::Never,
        outlines: super::Outlines::CoreText,
    };
    #[cfg(not(target_os = "macos"))]
    let expected = RasterOptions::for_face(Some(FaceHinting::Slight { autohint: false }));
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
        let glyph = font.glyph_for('m').expect("the face maps m");
        SkrifaOutlines::default()
            .coverage(&font, glyph, 15.0)
            .expect("an outline")
    };
    let slight = draw(FaceHinting::Slight { autohint: false });
    let full = draw(FaceHinting::Full { autohint: false });
    assert_ne!(
        (slight.left, slight.width(), slight.pixels().as_bytes()),
        (full.left, full.width(), full.pixels().as_bytes())
    );
}

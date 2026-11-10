//! CoreText's outlines checked against skrifa's, on properties that hold
//! on every macOS release: CoreText's pixels move with each one.

use std::{path::Path, sync::Arc};

use objc2_core_foundation::{CFArray, CFData, CFRetained, CGPoint};
use objc2_core_graphics::CGGlyph;
use objc2_core_text::{CTFont, CTFontDescriptor};

use super::{
    Canvas, CoreTextOutlines, Drawn, Fault, Format, SmoothingPreference, descriptors_from_data,
    draw, font_from_descriptor, font_smoothing, inject, pad,
};
use crate::{
    Font, FontBytes, GlyphBitmap, GlyphId, WGHT, goldens, map_face_file, metadata,
    raster::{
        OutlineBackend, Rasterizer, SkrifaOutlines,
        tests::{corpus, load},
    },
};

const MENLO: &str = "/System/Library/Fonts/Menlo.ttc";

#[test]
fn an_integer_zero_alone_turns_smoothing_off() {
    assert!(!SmoothingPreference::Integer(0).smooths());
    for level in 1..=3 {
        assert!(SmoothingPreference::Integer(level).smooths(), "{level}");
    }
    assert!(SmoothingPreference::Unset.smooths());
    assert!(SmoothingPreference::Boolean(false).smooths());
    assert!(SmoothingPreference::Boolean(true).smooths());
    assert!(SmoothingPreference::Other.smooths());
}

#[test]
fn a_string_counts_as_its_leading_integer() {
    assert!(!SmoothingPreference::Text("0".into()).smooths());
    assert!(!SmoothingPreference::Text(" 0 ".into()).smooths());
    assert!(SmoothingPreference::Text("2".into()).smooths());
    assert!(SmoothingPreference::Text("-1".into()).smooths());
    assert!(!SmoothingPreference::Text("off".into()).smooths());
}

/// The pinned probe face's first descriptor as CoreText reads it at `px`.
fn probe_font(font: &Font, px: f64) -> CFRetained<CTFont> {
    let data = CFData::from_bytes(font.bytes.as_ref().as_ref());
    let descriptors = descriptors_from_data(&data).expect("CoreText reads the probe face");
    // SAFETY: CoreText documents the array as holding font descriptors.
    let descriptors =
        unsafe { CFRetained::cast_unchecked::<CFArray<CTFontDescriptor>>(descriptors) };
    let descriptor = descriptors.get(0).expect("the probe face has one face");
    font_from_descriptor(&descriptor, px).expect("CoreText creates the probe font")
}

fn render(font: &CTFont, glyph: CGGlyph, format: Format, smooth: bool) -> Vec<u8> {
    const SIDE: usize = 64;
    let mut canvas = vec![0; SIDE * SIDE * format.bytes_per_pixel()];
    let origin = CGPoint::new(16.0, 16.0);
    assert!(draw(font, glyph, origin, SIDE, format, smooth, &mut canvas));
    canvas
}

fn ink(bytes: &[u8]) -> u64 {
    bytes.iter().map(|&b| u64::from(b)).sum()
}

/// Whether this host's session can smooth at all: a `LaunchDaemon` runner
/// has no Aqua session, and Apple documents nothing either way.
#[test]
fn smoothing_changes_the_drawing_where_the_setting_enables_it() {
    let font = Font::load_test_font();
    let glyph = CGGlyph::try_from(font.glyph_for('H').expect("the probe face draws H"))
        .expect("a 16-bit glyph id");
    let ct = probe_font(&font, 24.0);
    let plain = render(&ct, glyph, Format::Gray, false);
    assert!(ink(&plain) > 0, "CoreText drew no ink");
    let preference = font_smoothing();
    if preference.smooths() {
        assert_ne!(
            render(&ct, glyph, Format::Gray, true),
            plain,
            "smoothing changed nothing with AppleFontSmoothing={preference:?}: this session \
             cannot smooth, so the runner needs an Aqua session"
        );
    }
    let rgbx = render(&ct, glyph, Format::Rgbx, true);
    assert!(
        rgbx.as_chunks::<4>()
            .0
            .iter()
            .all(|[r, g, b, _]| r == g && g == b),
        "smoothing drew color fringes"
    );
}

/// The bitmap felis ships is CoreText's drawing with the smoothing
/// `AppleFontSmoothing` selects, which differs from the other setting's.
#[test]
fn the_shipped_bitmap_is_drawn_with_the_selected_smoothing() {
    let font = load("opentype/MonaspaceNeon-Regular.otf");
    let (glyph, px) = (font.glyph_for('g').expect("Monaspace maps g"), 24.0);
    let ink_box = SkrifaOutlines::default()
        .layer_box(&font, glyph, px)
        .expect("g has an outline");
    let ctf = CoreTextOutlines::default()
        .font(&font, px)
        .expect("CoreText opens the face");
    let id = CGGlyph::try_from(glyph).expect("a 16-bit glyph id");
    let drawn = |smooth| -> GlyphBitmap {
        let Ok(canvas) = Canvas::draw(&ctf, id, ink_box, px, smooth) else {
            panic!("CoreText draws g");
        };
        canvas.trim().into()
    };
    let placed = |bitmap: &GlyphBitmap| {
        (
            bitmap.left,
            bitmap.top,
            bitmap.width(),
            bitmap.pixels().as_bytes().to_vec(),
        )
    };
    let selected = font_smoothing().smooths();
    let shipped = Rasterizer::default().rasterize(&font, glyph, px);
    assert_eq!(placed(&shipped), placed(&drawn(selected)));
    if selected {
        assert_ne!(placed(&drawn(true)), placed(&drawn(false)));
    }
}

/// Every face of the system's Menlo collection.
fn menlo() -> Vec<(String, Font)> {
    let bytes: FontBytes =
        Arc::new(map_face_file(Path::new(MENLO)).expect("Menlo ships with macOS"));
    (0..metadata::face_count(bytes.as_ref().as_ref()))
        .map(|index| (format!("Menlo.ttc#{index}"), goldens::load(&bytes, index)))
        .collect()
}

fn ascii(font: &Font) -> Vec<(char, GlyphId)> {
    ('!'..='~')
        .filter_map(|c| Some((c, font.glyph_for(c)?)))
        .collect()
}

/// A bitmap's ink box, `[left, top, right, bottom]` with y up.
fn ink_box(bitmap: &GlyphBitmap) -> [i32; 4] {
    let (w, h) = (bitmap.width().cast_signed(), bitmap.height().cast_signed());
    [bitmap.left, bitmap.top, bitmap.left + w, bitmap.top - h]
}

/// Ink per pixel row over `rows` (y of each row's bottom edge, from the
/// top down).
fn profile(bitmap: &GlyphBitmap, rows: std::ops::Range<i32>) -> Vec<f64> {
    let width = bitmap.width() as usize;
    let bytes = bitmap.pixels().as_bytes();
    rows.rev()
        .map(|y| {
            let r = bitmap.top - 1 - y;
            usize::try_from(r)
                .ok()
                .filter(|&r| r < bitmap.height() as usize)
                .map_or(0.0, |r| {
                    bytes[r * width..(r + 1) * width]
                        .iter()
                        .map(|&b| f64::from(b))
                        .sum()
                })
        })
        .collect()
}

fn correlation(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let cov: f64 = a.iter().zip(b).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f64 = a.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f64 = b.iter().map(|y| (y - mb).powi(2)).sum();
    cov / (va * vb).sqrt()
}

/// `bitmap` turned upside down in place.
fn flipped(bitmap: &GlyphBitmap) -> GlyphBitmap {
    let width = bitmap.width() as usize;
    let rows: Vec<u8> = bitmap
        .pixels()
        .as_bytes()
        .chunks(width.max(1))
        .rev()
        .flatten()
        .copied()
        .collect();
    GlyphBitmap::new(
        bitmap.width(),
        bitmap.height(),
        bitmap.left,
        bitmap.top,
        crate::GlyphPixels::Coverage(rows),
    )
}

/// Draws `glyph` through both rasterizers, CoreText's canvas untrimmed.
struct Pair {
    canvas: Canvas,
    core_text: GlyphBitmap,
    skrifa: GlyphBitmap,
}

/// Both drawings of `glyph`, `None` where skrifa's box is empty or past
/// the budget; inside it, a drawing either rasterizer fails is an error.
fn pair(
    ct: &mut CoreTextOutlines,
    sk: &mut SkrifaOutlines,
    font: &Font,
    glyph: GlyphId,
    px: f32,
) -> Result<Option<Pair>, String> {
    let Some(ink_box) = sk.layer_box(font, glyph, px) else {
        return Ok(None);
    };
    if ink_box[2] <= ink_box[0] || ink_box[1] <= ink_box[3] {
        return Ok(None);
    }
    let ctf = ct
        .font(font, px)
        .ok_or_else(|| "CoreText did not open the face".to_owned())?;
    let id = CGGlyph::try_from(glyph).expect("a 16-bit glyph id");
    let canvas = Canvas::draw(&ctf, id, ink_box, px, font_smoothing().smooths())
        .map_err(|_| "CoreText drew no canvas".to_owned())?;
    let core_text = GlyphBitmap::from(coverage_of(&canvas));
    let skrifa = sk
        .coverage(font, glyph, px)
        .ok_or_else(|| "skrifa drew nothing inside its box".to_owned())?;
    Ok(Some(Pair {
        canvas,
        core_text,
        skrifa,
    }))
}

fn coverage_of(canvas: &Canvas) -> super::coverage::Mask {
    super::coverage::trim_in_place(
        canvas.data.clone(),
        canvas.width,
        canvas.height,
        canvas.left,
        canvas.top,
    )
}

fn ring_is_empty(canvas: &Canvas) -> bool {
    let (w, h) = (canvas.width, canvas.height);
    let at = |r: usize, c: usize| canvas.data[r * w + c];
    (0..w).all(|c| at(0, c) == 0 && at(h - 1, c) == 0)
        && (0..h).all(|r| at(r, 0) == 0 && at(r, w - 1) == 0)
}

/// The placement and weight properties of one CoreText glyph against
/// skrifa's unhinted one.
fn check(name: &str, c: char, px: f32, pair: &Pair, failures: &mut Vec<String>) {
    let mut fail = |what: String| failures.push(format!("{name} {c:?} {px}px: {what}"));
    if !ring_is_empty(&pair.canvas) {
        fail("ink reached the canvas's outer ring".into());
    }
    let (ct, sk) = (&pair.core_text, &pair.skrifa);
    let (ct_ink, sk_ink) = (ink(ct.pixels().as_bytes()), ink(sk.pixels().as_bytes()));
    if ct.width() == 0 || sk.width() == 0 {
        fail(format!("ink CoreText {ct_ink}, skrifa {sk_ink}"));
        return;
    }
    let [cl, ct_top, cr, cb] = ink_box(ct);
    let [sl, st, sr, sb] = ink_box(sk);
    if !(cl <= sl + 1 && cr >= sr - 1 && ct_top >= st - 1 && cb <= sb + 1) {
        fail(format!(
            "CoreText box {:?} misses skrifa's {:?} shrunk by 1",
            [cl, ct_top, cr, cb],
            [sl, st, sr, sb]
        ));
    }
    let grow = i32::try_from(pad(px)).expect("a small pad");
    if !(cl >= sl - grow && cr <= sr + grow && ct_top <= st + grow && cb >= sb - grow) {
        fail(format!(
            "CoreText box {:?} passes skrifa's {:?} grown by {grow}",
            [cl, ct_top, cr, cb],
            [sl, st, sr, sb]
        ));
    }
    let rim = 255 * u64::from(sk.width() + sk.height());
    if ct_ink > 2 * sk_ink + rim {
        fail(format!("ink CoreText {ct_ink}, skrifa {sk_ink}"));
    }
    if c == 'H' && (ct_top - st).abs() > 1 {
        fail(format!("top {ct_top}, skrifa {st}"));
    }
    if c == 'g' && (cb - sb).abs() > 1 {
        fail(format!("bottom {cb}, skrifa {sb}"));
    }
    // Smoothing grows a glyph upward more than downward, and CoreText
    // aligns a small CFF glyph vertically, so profiles are compared at
    // the best of three one-row shifts. `g`'s profile is near symmetric
    // at those shifts, so it tells no flip.
    if "L7Pj".contains(c) {
        let rows = cb.min(sb) - 1..ct_top.max(st) + 1;
        let own = profile(ct, rows.clone());
        let best = |reference: &GlyphBitmap| {
            let reference = profile(reference, rows.clone());
            (0..3)
                .map(|shift| {
                    let mut moved = vec![0.0; reference.len()];
                    for (i, v) in reference.iter().enumerate() {
                        if let Some(to) = (i + shift).checked_sub(1).filter(|&to| to < moved.len())
                        {
                            moved[to] = *v;
                        }
                    }
                    correlation(&own, &moved)
                })
                .fold(f64::MIN, f64::max)
        };
        let (upright, upside_down) = (best(sk), best(&flipped(sk)));
        if !(upright > 0.7 && upright > upside_down) {
            fail(format!(
                "row profile correlates {upright:.3} upright, {upside_down:.3} flipped"
            ));
        }
    }
}

fn sizes() -> Vec<f32> {
    let mut sizes: Vec<f32> = (6..=72).map(|px| px as f32).collect();
    sizes.extend([16.0 * 7.0, 24.0 * 7.0]);
    sizes
}

fn assert_no_failures(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{} failures, the first: {:#?}",
        failures.len(),
        &failures[..failures.len().min(20)]
    );
}

fn check_faces(faces: &[(String, Font)]) {
    let mut failures = Vec::new();
    let (mut ct, mut sk) = (CoreTextOutlines::default(), SkrifaOutlines::default());
    for (name, font) in faces {
        for px in sizes() {
            for (c, glyph) in ascii(font) {
                match pair(&mut ct, &mut sk, font, glyph, px) {
                    Ok(Some(pair)) => check(name, c, px, &pair, &mut failures),
                    Ok(None) => {}
                    Err(what) => failures.push(format!("{name} {c:?} {px}px: {what}")),
                }
            }
        }
    }
    assert_no_failures(&failures);
}

#[test]
fn coretext_glyphs_sit_where_skrifas_do() {
    check_faces(&corpus());
}

/// `glyph` as CoreText draws it without smoothing, which leaves its
/// outline undilated.
fn plain(
    ct: &mut CoreTextOutlines,
    sk: &mut SkrifaOutlines,
    font: &Font,
    glyph: GlyphId,
    px: f32,
) -> Option<GlyphBitmap> {
    let ink_box = sk.layer_box(font, glyph, px)?;
    let ctf = ct.font(font, px)?;
    let id = CGGlyph::try_from(glyph).ok()?;
    let canvas = Canvas::draw(&ctf, id, ink_box, px, false).ok()?;
    let mask = canvas.trim();
    (mask.width > 0).then(|| mask.into())
}

/// Dilation makes a regular face's glyphs nearer its bold sibling's, so
/// the member is identified by CoreText's undilated drawing.
#[test]
fn every_menlo_member_draws_its_own_face() {
    let members = menlo();
    assert!(members.len() > 1, "Menlo is a collection");
    check_faces(&members);
    let px = 24.0;
    let mut sk = SkrifaOutlines::default();
    let mut ct = CoreTextOutlines::default();
    let distance = |a: &GlyphBitmap, b: &GlyphBitmap| -> u64 {
        let [al, at, ar, ab] = ink_box(a);
        let [bl, bt, br, bb] = ink_box(b);
        let value = |m: &GlyphBitmap, x: i32, y: i32| {
            let (c, r) = (x - m.left, m.top - 1 - y);
            if c < 0 || r < 0 || c >= m.width().cast_signed() || r >= m.height().cast_signed() {
                0
            } else {
                i64::from(m.pixels().as_bytes()[(r * m.width().cast_signed() + c) as usize])
            }
        };
        let mut sum = 0;
        for y in ab.min(bb)..at.max(bt) {
            for x in al.min(bl)..ar.max(br) {
                sum += (value(a, x, y) - value(b, x, y)).unsigned_abs();
            }
        }
        sum
    };
    for (name, own) in &members {
        let totals: Vec<u64> = members
            .iter()
            .map(|(_, other)| {
                ascii(own)
                    .into_iter()
                    .filter_map(|(c, glyph)| {
                        let drawn = plain(&mut ct, &mut sk, own, glyph, px)?;
                        let theirs = sk.coverage(other, other.glyph_for(c)?, px)?;
                        Some(distance(&drawn, &theirs))
                    })
                    .sum()
            })
            .collect();
        let at = members
            .iter()
            .position(|(other, _)| other == name)
            .expect("listed");
        let nearest = (0..totals.len())
            .min_by_key(|&i| totals[i])
            .expect("members");
        assert_eq!(
            nearest, at,
            "{name}'s CoreText glyphs are nearest {}'s skrifa glyphs: {totals:?}",
            members[nearest].0
        );
    }
}

fn variable() -> Font {
    load("truetype/Monaspace Neon Var.ttf")
}

#[test]
fn a_variable_face_draws_at_the_instance_felis_shapes() {
    let var = variable();
    let (mut ct, mut sk) = (CoreTextOutlines::default(), SkrifaOutlines::default());
    let regular = var.at_style(400, false).expect("the face has wght");
    let bold = var.at_style(700, false).expect("the face has wght");
    let draw = |ct: &mut CoreTextOutlines, sk: &mut SkrifaOutlines, font: &Font| {
        let glyph = font.glyph_for('m').expect("the face maps m");
        match ct.coverage(sk, font, glyph, 24.0) {
            Drawn::Glyph(bitmap) => bitmap.into_pixels().as_bytes().to_vec(),
            _ => panic!("CoreText draws the instance"),
        }
    };
    assert_ne!(
        draw(&mut ct, &mut sk, &regular),
        draw(&mut ct, &mut sk, &bold)
    );
    check_faces(&[("wght 400".into(), regular), ("wght 700".into(), bold)]);
}

/// CoreText reports no axis at its default, so the read-back must be
/// completed from `fvar` before it is compared.
#[test]
fn an_instance_at_the_default_reads_back_as_requested() {
    let var = variable();
    let wght = var
        .face
        .axes()
        .get_by_tag(WGHT)
        .expect("the face has wght")
        .default_value();
    let explicit = var.with_coords(var.normalized_coords(&[(WGHT, wght)]), vec![(WGHT, wght)]);
    let mut ct = CoreTextOutlines::default();
    for font in [&var, &explicit] {
        assert!(ct.font(font, 16.0).is_some(), "{}", font.display_name());
    }
}

/// Faces and glyphs CoreText cannot draw match skrifa's bytes.
fn assert_skrifa_bytes(font: &Font, px: f32) {
    let mut rasterizer = Rasterizer::default();
    let mut sk = SkrifaOutlines::default();
    for (c, glyph) in ascii(font) {
        let expected = sk
            .coverage(font, glyph, px)
            .unwrap_or_else(GlyphBitmap::blank);
        let drawn = rasterizer.rasterize(font, glyph, px);
        assert!(
            (
                drawn.left,
                drawn.top,
                drawn.width(),
                drawn.pixels().as_bytes()
            ) == (
                expected.left,
                expected.top,
                expected.width(),
                expected.pixels().as_bytes()
            ),
            "{c:?} differs from skrifa's"
        );
    }
}

#[test]
fn a_face_coretext_cannot_open_draws_through_skrifa() {
    let bold = variable().at_style(700, false).expect("the face has wght");
    for fault in [
        Fault::Descriptors,
        Fault::Name,
        Fault::CopyWithAttributes,
        Fault::CreateFont,
        Fault::CopyVariation,
        Fault::GlyphCount,
        Fault::BlankCanvas,
    ] {
        inject(Some(fault));
        assert_skrifa_bytes(&bold, 16.0);
        inject(None);
    }
}

#[test]
fn a_color_faces_outline_draws_through_skrifa() {
    let path = crate::test_font_dir().join("../color-probe/FelisColorProbe.ttf");
    let bytes: FontBytes = Arc::new(std::fs::read(path).expect("the probe reads"));
    let font = goldens::load(&bytes, 0);
    assert!(font.has_color_glyphs());
    let text = font.outlines_only();
    let glyph = text.glyph_for('C').expect("the probe maps C");
    let expected = SkrifaOutlines::default()
        .coverage(&text, glyph, 32.0)
        .expect("C has an outline");
    let drawn = Rasterizer::default().rasterize(&text, glyph, 32.0);
    assert_eq!(drawn.pixels().as_bytes(), expected.pixels().as_bytes());
    assert_eq!((drawn.left, drawn.top), (expected.left, expected.top));
}

#[test]
fn an_empty_or_unknown_glyph_draws_nothing() {
    let font = Font::load_test_font();
    let (mut ct, mut sk) = (CoreTextOutlines::default(), SkrifaOutlines::default());
    let space = font.glyph_for(' ').expect("the face maps space");
    let past = u32::from(
        skrifa::FontRef::from_index(font.bytes.as_ref().as_ref(), font.index)
            .and_then(|face| skrifa::raw::TableProvider::maxp(&face))
            .expect("maxp")
            .num_glyphs(),
    );
    for glyph in [space, past] {
        assert!(matches!(
            ct.coverage(&mut sk, &font, glyph, 16.0),
            Drawn::Nothing
        ));
    }
    assert!(matches!(
        ct.coverage(&mut sk, &font, u32::from(u16::MAX) + 1, 16.0),
        Drawn::Skrifa
    ));
}

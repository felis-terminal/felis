use super::*;
use crate::test_support::pinned_arc;

#[test]
fn shape_run_with_empty_text_produces_no_glyphs() {
    let font = font();
    let mut shaper = Shaper::new();
    let out = shaper.shape_run(&font, 16.0, &[], "");
    assert_eq!(out, []);
}

/// Features-off shaping is one glyph per byte, or the renderer could
/// not opt cells into shaping without shifting the grid.
#[test]
fn shape_run_ascii_no_features_emits_one_glyph_per_byte() {
    let font = font();
    let mut shaper = Shaper::new();
    let out = shaper.shape_run(&font, 16.0, &[], "Hello");
    assert_eq!(out.len(), 5, "one glyph per ASCII byte");
    for (i, g) in out.iter().enumerate() {
        assert_eq!(g.source_byte_start, i as u32);
        assert_eq!(g.source_byte_len, 1);
        assert!(g.advance_px > 0.0, "ASCII glyph must have non-zero advance");
        assert_ne!(g.glyph_id, 0, "ASCII char must map to a real glyph");
    }
}

#[test]
fn shape_run_of_spaces_produces_one_glyph_per_cell() {
    let font = font();
    let mut shaper = Shaper::new();
    let out = shaper.shape_run(&font, 16.0, &[], "    ");
    assert_eq!(out.len(), 4);
    for g in &out {
        assert!(g.advance_px > 0.0, "space must advance the pen");
    }
}

/// Malformed feature strings are dropped silently and the output
/// equals the empty-feature-list output.
#[test]
fn shape_run_rejects_malformed_features_without_failing() {
    let font = font();
    let mut shaper = Shaper::new();
    let baseline = shaper.shape_run(&font, 16.0, &[], "ABC");
    let with_garbage = shaper.shape_run(
        &font,
        16.0,
        &["nope".to_owned(), "xx".to_owned(), String::new()],
        "ABC",
    );
    assert_eq!(baseline.len(), with_garbage.len());
    for (a, b) in baseline.iter().zip(with_garbage.iter()) {
        assert_eq!(a.glyph_id, b.glyph_id);
    }
}

/// The char path and the glyph-id path produce identical bitmaps, so
/// mixed-mode rendering never draws one letter two ways.
#[test]
fn rasterize_by_char_matches_rasterize_by_glyph_id() {
    let font = font();
    let glyph_id = font
        .glyph_for('A')
        .expect("system monospace must cover 'A'");
    let mut cache = ShapeCache::default();
    let from_char = rasterize(&mut cache.rasterizer, &font, 'A', 16.0);
    let from_id = cache.rasterize_glyph_id(&font, glyph_id, 16.0);
    assert_eq!(from_char.pixels.as_bytes(), from_id.pixels.as_bytes());
    assert_eq!(from_char.width(), from_id.width());
    assert_eq!(from_char.height(), from_id.height());
    assert_eq!(from_char.left, from_id.left);
    assert_eq!(from_char.top, from_id.top);
}

/// `rasterize_glyph_id(0)` returns a blank bitmap, matching the char
/// path's charmap miss.
#[test]
fn rasterize_glyph_id_zero_returns_blank() {
    let font = font();
    let bitmap = ShapeCache::default().rasterize_glyph_id(&font, 0, 16.0);
    assert!(bitmap.is_blank());
    assert_eq!(bitmap.width(), 0);
    assert_eq!(bitmap.height(), 0);
}

#[test]
fn shape_run_is_repeatable_across_calls_on_the_same_shaper() {
    let font = font();
    let mut shaper = Shaper::new();
    let features = vec!["calt".to_owned()];
    let first = shaper.shape_run(&font, 16.0, &features, "Hello");
    let second = shaper.shape_run(&font, 16.0, &features, "Hello");
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.glyph_id, b.glyph_id);
        assert_eq!(a.source_byte_start, b.source_byte_start);
        assert_eq!(a.source_byte_len, b.source_byte_len);
        assert!((a.advance_px - b.advance_px).abs() < f32::EPSILON);
    }
}

/// Monaspace's `calt` texture healing is on by default (harfrust, like
/// `HarfBuzz` and CSS, applies `calt` unrequested) and `-calt` turns
/// it off. Needs the pinned devshell font: `FiraCode`'s calt never
/// yields a single ligature glyph.
#[test]
fn shape_run_calt_texture_healing_defaults_on_and_disables() {
    let font = Font::load_test_font();
    let base_ids: Vec<GlyphId> = ['m', 'i', 'm']
        .into_iter()
        .map(|c| font.glyph_for(c).unwrap_or_default())
        .collect();
    let mut shaper = Shaper::new();
    let healed = shaper.shape_run(&font, 16.0, &[], "mim");
    let disabled = shaper.shape_run(&font, 16.0, &["-calt".to_owned()], "mim");
    for out in [&healed, &disabled] {
        assert_eq!(out.len(), 3, "healing must keep one glyph per cell");
        assert!(out.iter().all(|g| g.source_byte_len == 1));
    }
    let healed_ids: Vec<GlyphId> = healed.iter().map(|g| g.glyph_id).collect();
    let disabled_ids: Vec<GlyphId> = disabled.iter().map(|g| g.glyph_id).collect();
    assert_eq!(
        disabled_ids, base_ids,
        "`-calt` must fall back to the unhealed charmap glyphs",
    );
    assert_ne!(
        healed_ids, base_ids,
        "default shaping must apply calt texture healing",
    );
}

/// The probe face tracks every glyph +100 units at 12 pt and -50 at
/// 24 pt; felis shapes it at its `hmtx` advances at any size.
#[test]
fn a_face_with_a_trak_table_shapes_untracked() {
    use skrifa::raw::{TableProvider, model::Blob};
    let bytes = std::fs::read(pinned_files(&[TRAK_PROBE])[0].clone()).expect("probe reads");
    let raw = skrifa::FontRef::new(&bytes).expect("the probe parses");
    for tag in [b"trak", b"STAT"] {
        assert!(
            raw.table_data(Tag::new(tag)).is_some(),
            "the probe has {tag:?}"
        );
    }
    let tracked = skrifa::Font::new(Blob::from(bytes.clone()), 0).expect("a blob font");
    let mut buffer = Buffer::new();
    buffer.push_str("m");
    buffer.guess_segment_properties();
    harfrust::shape(&ShaperFont::new(&tracked), &mut buffer, ShapeOptions::new()).expect("shapes");
    let hmtx = raw.glyph_metrics(Size::unscaled(), LocationRef::default());
    let m = buffer.glyph_infos()[0].glyph_id;
    let m_advance = hmtx.advance_width(skrifa::GlyphId::new(m)).expect("hmtx");
    assert!(
        (buffer.glyph_positions()[0].x_advance as f32 - m_advance - 100.0).abs() < 0.5,
        "harfrust tracks the probe when it sees the table"
    );
    let font = pinned_arc(TRAK_PROBE);
    let upem = f32::from(raw.head().expect("head").units_per_em());
    let mut shaper = Shaper::new();
    for px in [12.0, 16.0, 24.0, 32.0, 48.0] {
        let out = shaper.shape_run(&font, px, &[], "mim->");
        assert_eq!(out.len(), 5, "{px}");
        for g in out {
            let advance = hmtx
                .advance_width(skrifa::GlyphId::new(g.glyph_id))
                .expect("hmtx");
            assert!(
                (g.advance_px - advance * px / upem).abs() <= 0.01,
                "{px}px glyph {}: {} != {}",
                g.glyph_id,
                g.advance_px,
                advance * px / upem
            );
        }
    }
}

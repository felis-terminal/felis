use super::*;
use crate::test_support::pinned_dir_set;

#[test]
fn the_default_monospace_face_loads_with_nonzero_metrics() {
    let provider = pinned_system();
    let (id, family) = provider
        .default_monospace(REGULAR_WEIGHT)
        .expect("a monospace face");
    assert_eq!(family, TEST_FONT_FAMILY);
    let font = Font::load_face_id(&provider, id).expect("load font");
    let metrics = font.cell_metrics(16.0);
    assert!(metrics.width > 0);
    assert!(metrics.height > 0);
}

#[test]
fn rasterizes_capital_a() {
    let font = font();
    let mut cache = ShapeCache::default();
    let bitmap = cache.get_or_insert(&font, 'A', 16);
    assert!(
        bitmap.width() > 0 && bitmap.height() > 0,
        "A should rasterize"
    );
    assert!(!bitmap.is_blank(), "A should have non-zero pixels");
}

/// A mask glyph comes back as `Coverage`; the renderer routes it to
/// the `R8` atlas on this tag.
#[test]
fn mask_glyph_rasterizes_as_coverage() {
    let font = font();
    let bitmap = rasterize(&mut Rasterizer::default(), &font, 'A', 16.0);
    assert!(
        matches!(bitmap.pixels(), GlyphPixels::Coverage(_)),
        "ASCII 'A' is a coverage mask, not color",
    );
}

/// A color-face glyph (⏸ U+23F8) rasterizes as `Rgba`, not collapsed
/// to a mask (the ⏸/⏺ solid-square regression).
#[test]
fn color_emoji_rasterizes_as_rgba_not_a_mask() {
    let db = pinned_dir_set();
    let font = Font::try_load_with(&db, "Noto Color Emoji").expect("pinned color face");
    assert!(font.has_color_glyphs() && font.has_glyph('⏸'));
    let bitmap = rasterize(&mut Rasterizer::default(), &font, '⏸', 32.0);
    assert!(
        bitmap.is_color(),
        "a color-face U+23F8 must rasterize as RGBA, not collapse to a mask",
    );
}

#[test]
fn cache_returns_stable_results() {
    let font = font();
    let mut cache = ShapeCache::default();
    let first = cache.get_or_insert(&font, 'X', 16).clone();
    let second = cache.get_or_insert(&font, 'X', 16).clone();
    assert_eq!(first.pixels.as_bytes(), second.pixels.as_bytes());
    assert_eq!(cache.len(), 1);
}

/// Once `bytes_used` crosses the cap, inserts evict other entries
/// but keep the just-inserted one.
#[test]
fn byte_cap_evicts_old_entries_but_keeps_the_newest() {
    let font = font();
    let mut cache = ShapeCache::new(1);

    cache.get_or_insert(&font, 'A', 16);
    assert_eq!(cache.len(), 1, "first insert is never evicted");

    for c in ['B', 'C', 'D', 'E'] {
        cache.get_or_insert(&font, c, 16);
        assert_eq!(
            cache.len(),
            1,
            "tiny cap holds at most the just-inserted entry",
        );
    }

    let last = ('E', 16u32, SizingKey::default());
    assert!(
        cache.map.contains_key(&last),
        "newest key survives eviction",
    );
    assert!(
        cache.bytes_used <= entry_cost(&cache.map[&last]),
        "accounting reflects only the surviving entry",
    );
}

/// A cap wide enough for several entries is trimmed to the cap, not
/// flushed. Five sizings of one glyph cost the same bytes, so the
/// surviving count is exact.
#[test]
fn byte_cap_evicts_only_down_to_the_cap_when_it_holds_several_entries() {
    let font = font();
    let m = ShapeCache::default().get_or_insert(&font, 'M', 32).clone();
    assert!(!m.is_blank(), "'M' must rasterize to a non-empty bitmap");
    let entry_bytes = entry_cost(&m);
    let mut cache = ShapeCache::new(entry_bytes * 3);

    for scale in 1..=5u8 {
        cache.get_or_insert_sized(&font, 'M', 32, SizingKey::new(scale, 0, 0, 0, 0, 0));
    }
    assert_eq!(
        cache.len(),
        3,
        "a three-entry cap evicts down to three, not to the newest alone",
    );
    assert_eq!(
        cache.bytes_used,
        entry_bytes * 3,
        "accounting must track exactly the surviving entries",
    );
    assert!(
        cache
            .map
            .contains_key(&('M', 32, SizingKey::new(5, 0, 0, 0, 0, 0))),
        "the just-inserted entry is excluded from eviction",
    );
}

#[test]
fn blank_bitmaps_count_against_the_byte_cap() {
    let font = font();
    let cap = 4096;
    let mut cache = ShapeCache::new(cap);
    for w in 0..=7u8 {
        for n in 0..=15u8 {
            for d in 0..=15u8 {
                let bitmap =
                    cache.get_or_insert_sized(&font, ' ', 16, SizingKey::new(1, w, n, d, 0, 0));
                assert!(bitmap.is_blank());
            }
        }
    }
    assert!(
        cache.len() <= cap / ENTRY_BYTES,
        "{} blank entries under a {cap}-byte cap",
        cache.len(),
    );
}

/// Distinct `SizingKey` values produce distinct cache entries.
#[test]
fn sizing_key_prevents_collisions() {
    let font = font();
    let mut cache = ShapeCache::default();
    cache.get_or_insert(&font, 'A', 16);
    assert_eq!(cache.len(), 1);

    let scale_2 = SizingKey::new(2, 0, 0, 0, 0, 0);
    cache.get_or_insert_sized(&font, 'A', 16, scale_2);
    assert_eq!(cache.len(), 2);

    let scale_2_bottom = SizingKey::new(2, 0, 0, 0, 1, 0);
    cache.get_or_insert_sized(&font, 'A', 16, scale_2_bottom);
    assert_eq!(cache.len(), 3);

    cache.get_or_insert_sized(&font, 'A', 16, scale_2);
    assert_eq!(cache.len(), 3);
}

#[test]
fn default_sizing_wrapper_shares_an_entry_with_explicit_default() {
    let font = font();
    let mut cache = ShapeCache::default();
    cache.get_or_insert(&font, 'B', 16);
    cache.get_or_insert_sized(&font, 'B', 16, SizingKey::default());
    cache.get_or_insert_sized(&font, 'B', 16, SizingKey::new(1, 0, 0, 0, 0, 0));
    assert_eq!(cache.len(), 1);
    assert_eq!(SizingKey::default(), SizingKey::new(1, 0, 0, 0, 0, 0));
}

#[test]
fn space_is_blank() {
    let font = font();
    let mut cache = ShapeCache::default();
    let bitmap = cache.get_or_insert(&font, ' ', 16);
    assert!(bitmap.is_blank());
}

use super::*;

#[cfg(not(windows))]
#[test]
fn an_overlay_cluster_is_shaped_and_its_glyphs_primed() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let mut shaper = Shaper::new();
    assert!(idx.overlay_cluster("e\u{301}").is_none());
    idx.ensure_overlay_cluster(&mut shaper, "e\u{301}");
    let glyphs = idx.overlay_cluster("e\u{301}").expect("shaped").to_vec();
    assert_ne!(glyphs.len(), 0);
    let h = idx.cell_metrics().height;
    let base = &glyphs[0];
    let key = SizingKey::default().with_font_id(usize::from(base.font_id));
    assert!(
        idx.glyph_id_slot(base.glyph_id, h, key).is_some(),
        "base primed"
    );
}

#[cfg(not(windows))]
#[test]
fn overlay_shapes_survive_the_frame_that_primed_them() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let mut shaper = Shaper::new();
    let clusters: Vec<String> = ('a'..='z')
        .flat_map(|base| ('\u{300}'..='\u{30F}').map(move |mark| format!("{base}{mark}")))
        .take(GlyphIndex::OVERLAY_CLUSTER_MAX + 50)
        .collect();
    idx.trim_overlay_clusters();
    for cluster in &clusters {
        idx.ensure_overlay_cluster(&mut shaper, cluster);
    }
    assert!(
        clusters.iter().all(|c| idx.overlay_cluster(c).is_some()),
        "a label past the bound keeps every cluster for the frame it paints"
    );
    idx.trim_overlay_clusters();
    assert!(
        idx.overlay_cluster(&clusters[0]).is_none(),
        "bounded between frames"
    );
}

#[cfg(not(windows))]
#[test]
fn ensure_records_a_slot_and_pending_upload_for_a_printable_glyph() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    let did_work = idx.ensure('A', h, sk);
    assert!(did_work, "first ensure must rasterize + allocate");
    assert!(idx.contains('A', h, sk));
    let slot = idx.slot('A', h, sk).expect("A produced a slot");
    assert!(slot.atlas_px[0] + slot.size_px[0] <= idx.sheet_side(AtlasKind::Mono));
    assert!(slot.atlas_px[1] + slot.size_px[1] <= idx.sheet_side(AtlasKind::Mono));
    assert!(slot.size_px[0] > 0 && slot.size_px[1] > 0);
    let mut iter = idx.drain_pending();
    let first = iter.next().expect("one upload queued");
    assert!(iter.next().is_none(), "exactly one upload queued");
    assert_eq!(
        first.pixels.as_bytes().len(),
        (first.alloc.width * first.alloc.height) as usize,
        "pixel buffer length matches the allocation"
    );
}

#[cfg(not(windows))]
#[test]
fn ensure_is_idempotent_for_the_same_key() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    assert!(idx.ensure('B', h, sk));
    assert!(!idx.ensure('B', h, sk), "second ensure is a hit");
    assert_eq!(
        idx.drain_pending().count(),
        1,
        "second ensure must not enqueue another upload"
    );
}

#[cfg(not(windows))]
#[test]
fn whitespace_caches_a_negative_entry_with_no_upload() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    idx.ensure(' ', h, sk);
    assert!(idx.contains(' ', h, sk));
    assert_eq!(idx.slot(' ', h, sk), None, "space resolves to no slot");
    assert!(
        idx.drain_pending().next().is_none(),
        "blank glyphs do not queue uploads"
    );
}

#[cfg(not(windows))]
#[test]
fn populate_walks_the_grid_and_caches_unique_glyphs_once() {
    let mut screen = Grid::new(2, 5);
    let mut parser = Parser::new();
    parser.advance(&mut screen, b"hello\r\nhello");
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    idx.populate(screen.screen());
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    for c in ['h', 'e', 'l', 'o'] {
        assert!(idx.contains(c, h, sk), "missing cache entry for {c:?}");
        assert!(idx.slot(c, h, sk).is_some(), "missing slot for {c:?}");
    }
    let pending_count = idx.drain_pending().count();
    assert_eq!(pending_count, 4);
}

#[cfg(not(windows))]
#[test]
fn sized_variant_takes_a_distinct_atlas_slot_from_default() {
    // An OSC 66 `s=2` 'A' must not share a slot with the default 'A'.
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(512));
    let h = idx.cell_metrics().height;
    let default_sk = SizingKey::default();
    let scale_2_sk = SizingKey::new(2, 0, 0, 0, 0, 0);
    assert!(idx.ensure('A', h, default_sk), "first ensure must work");
    assert!(
        idx.ensure('A', h, scale_2_sk),
        "sized variant must rasterize separately"
    );
    let small = idx.slot('A', h, default_sk).expect("default slot");
    let large = idx.slot('A', h, scale_2_sk).expect("scaled slot");
    assert_ne!(
        (small.atlas_px, small.size_px),
        (large.atlas_px, large.size_px),
        "sized variant must occupy a different atlas rect"
    );
    // Generous slack: hinted metrics are not exactly 2×.
    assert!(
        large.size_px[1] > small.size_px[1],
        "scaled bitmap height ({}) must exceed default ({})",
        large.size_px[1],
        small.size_px[1]
    );
}

#[cfg(not(windows))]
#[test]
fn a_full_atlas_resets_and_keeps_placing_glyphs() {
    // A small atlas so one page of distinct glyphs overflows; the
    // index must reset and keep handing out real slots.
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(48));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    let mut reset_seen = false;
    let mut placed_after_reset = false;
    let mut prev_len = idx.slots.len();
    for c in ('!'..='~').chain('\u{a1}'..='\u{17f}') {
        idx.ensure(c, h, sk);
        let len = idx.slots.len();
        if len < prev_len {
            reset_seen = true;
            assert!(
                idx.slot(c, h, sk).is_some(),
                "post-reset glyph {c:?} must get a real slot",
            );
            placed_after_reset = true;
        }
        prev_len = len;
    }
    assert!(
        reset_seen,
        "a page of glyphs must overflow the tiny atlas and reset it",
    );
    assert!(
        placed_after_reset,
        "allocations must resume after a reset instead of turning into permanent misses",
    );
}

/// The OSC 66 sizings the VT parser admits, each a distinct key.
fn osc66_sizings() -> impl Iterator<Item = SizingKey> {
    (1..=7u8).flat_map(|s| {
        (0..=7u8).flat_map(move |w| {
            (0..=15u8).flat_map(move |d| {
                (0..d.max(1)).flat_map(move |n| {
                    (0..=2u8)
                        .flat_map(move |v| (0..=2u8).map(move |h| SizingKey::new(s, w, n, d, v, h)))
                })
            })
        })
    })
}

#[cfg(not(windows))]
#[test]
fn distinct_blank_keys_keep_the_slot_map_under_the_blank_cap() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let h = idx.cell_metrics().height;
    for sk in osc66_sizings().take(3 * GlyphIndex::BLANK_SLOT_MAX) {
        idx.ensure(' ', h, sk);
        assert_eq!(idx.slot(' ', h, sk), None, "a sized space stays blank");
        assert!(
            idx.slots.len() <= GlyphIndex::BLANK_SLOT_MAX,
            "{} slot entries after blank key {sk:?}",
            idx.slots.len(),
        );
    }
}

#[cfg(not(windows))]
#[test]
fn dropping_blank_entries_keeps_placed_slots_and_the_atlas() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    idx.ensure('A', h, sk);
    let placed = idx.slot('A', h, sk).expect("A is placed");
    let _ = idx.drain_pending().count();
    for blank in osc66_sizings().take(2 * GlyphIndex::BLANK_SLOT_MAX) {
        idx.ensure(' ', h, blank);
    }
    assert_eq!(
        idx.atlas_resets(),
        0,
        "blank entries never recycle the atlas"
    );
    assert!(!idx.ensure('A', h, sk), "A is still resident");
    assert_eq!(idx.slot('A', h, sk), Some(placed));
    assert!(idx.drain_pending().next().is_none(), "nothing re-uploads");
}

fn placed_slot_count(idx: &GlyphIndex) -> usize {
    idx.slots.values().filter(|s| s.is_some()).count()
}

#[cfg(not(windows))]
#[test]
fn distinct_tiny_placed_glyphs_recycle_the_atlas_at_the_placed_cap() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(GLYPH_ATLAS_SIDE));
    let h = idx.cell_metrics().height;
    let fractional = osc66_sizings()
        .filter(|sk| sk.scale() == 1 && sk.frac_num() > 0 && sk.frac_num() < sk.frac_den());
    let keys = fractional.flat_map(|sk| ('A'..='Z').map(move |c| (c, sk)));
    let mut placed = 0;
    let mut last = None;
    for (c, sk) in keys {
        let before = idx.placed_slots;
        let resets = idx.atlas_resets();
        idx.ensure(c, h, sk);
        if idx.atlas_resets() != resets {
            assert_eq!(
                before,
                GlyphIndex::PLACED_SLOT_MAX,
                "the cap, not the sheet, recycles tiny glyphs",
            );
        }
        assert!(idx.placed_slots <= GlyphIndex::PLACED_SLOT_MAX);
        if let Some(slot) = idx.slot(c, h, sk) {
            placed += 1;
            last = Some((c, sk, slot));
            if placed == GlyphIndex::PLACED_SLOT_MAX + 1 {
                break;
            }
        }
    }
    assert_eq!(placed, GlyphIndex::PLACED_SLOT_MAX + 1);
    assert_eq!(idx.atlas_resets(), 1);
    assert_eq!(placed_slot_count(&idx), idx.placed_slots);
    let (c, sk, slot) = last.expect("a glyph was placed");
    assert!(
        !idx.ensure(c, h, sk),
        "the newest glyph survives the recycle"
    );
    assert_eq!(idx.slot(c, h, sk), Some(slot));
}

/// Overlay text (a chrome bar's label, the pre-edit) is primed
/// after the grid walk, so its own overflow drops the grid's fresh
/// slots; the renderer reads the same reported reset to walk again.
#[cfg(not(windows))]
#[test]
fn an_overlay_char_that_overflows_after_the_walk_reports_the_reset() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(32));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();

    let text = "uvwxyz";
    let mut screen = Grid::new(1, 8);
    Parser::new().advance(&mut screen, text.as_bytes());
    idx.populate(screen.screen());
    assert!(
        !idx.atlas_was_reset(),
        "the walk of one short row must fit, isolating the overlay overflow",
    );

    // Overlay text never goes through `populate`, so it primes on
    // top of a finished walk exactly as `populate_chars` does.
    for c in '\u{a1}'..='\u{2ff}' {
        if idx.atlas_was_reset() {
            break;
        }
        idx.ensure(c, h, sk);
    }
    assert!(
        idx.atlas_was_reset(),
        "priming overlay text after the walk must report the reset",
    );
    assert!(
        text.chars().any(|c| idx.slot(c, h, sk).is_none()),
        "the grid slots handed out before the overlay's reset are gone",
    );
}

/// A reset partway through a walk drops slots already handed out,
/// and rendering is damage-driven, so nothing would repaint those
/// cells; the reported reset drives a second walk.
#[cfg(not(windows))]
#[test]
fn a_mid_walk_reset_is_reported_and_a_second_walk_reslots_the_grid() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(48));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();

    // Measured, not assumed: it depends on the system face's bitmap
    // sizes.
    let capacity = {
        let mut probe = GlyphIndex::new(stack(), 14.0, nz(48));
        ('!'..='~')
            .take_while(|c| {
                let before = probe.slots.len();
                probe.ensure(*c, h, sk);
                probe.slots.len() > before
            })
            .count()
    };
    let text = "uvwxyz";
    assert!(
        capacity > text.len() + 2,
        "the sheet must hold the screen's own set with room to pre-fill",
    );

    // Fill the sheet with glyphs the screen does not use, stopping
    // just short of a reset, so the screen's walk resets partway.
    for c in ('!'..='~').take(capacity - 2) {
        idx.ensure(c, h, sk);
    }

    let mut screen = Grid::new(1, 8);
    Parser::new().advance(&mut screen, text.as_bytes());

    idx.populate(screen.screen());
    assert!(
        idx.atlas_was_reset(),
        "the pre-filled sheet must overflow during the walk",
    );
    assert!(
        text.chars().any(|c| idx.slot(c, h, sk).is_none()),
        "the glyphs primed before the reset lost their slots",
    );

    idx.populate(screen.screen());
    assert!(
        !idx.atlas_was_reset(),
        "the reclaimed sheet holds the screen's own set in one walk",
    );
    for c in text.chars() {
        assert!(
            idx.slot(c, h, sk).is_some(),
            "second walk must re-slot {c:?} so its cell paints this frame",
        );
    }
}

#[cfg(not(windows))]
#[test]
fn a_glyph_too_large_for_the_atlas_degrades_without_looping() {
    // An atlas so small nothing fits even when empty: a reset frees
    // no usable space, so without the retry-once guard this hangs.
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(2));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    idx.ensure('W', h, sk);
    assert_eq!(
        idx.slot('W', h, sk),
        None,
        "a glyph larger than the whole atlas degrades to no slot",
    );
}

#[cfg(not(windows))]
#[test]
fn allocations_within_the_atlas_never_overlap() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    let mut rects: Vec<(u32, u32, u32, u32)> = Vec::new();
    for c in 0x21u8..=0x7Eu8 {
        idx.ensure(c as char, h, sk);
        if let Some(s) = idx.slot(c as char, h, sk) {
            let [x, y] = s.atlas_px;
            rects.push((x, y, x + s.size_px[0], y + s.size_px[1]));
        }
    }
    for (i, a) in rects.iter().enumerate() {
        for b in &rects[i + 1..] {
            let separated = a.2 <= b.0 || b.2 <= a.0 || a.3 <= b.1 || b.3 <= a.1;
            assert!(separated, "overlap between {a:?} and {b:?}");
        }
    }
}

/// Same idempotence and negative-entry handling as the per-char
/// path.
#[cfg(not(windows))]
#[test]
fn ensure_glyph_id_is_idempotent_and_caches_a_slot() {
    use felis_shaping::Shaper;
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    let primary = idx.stack().primary().clone();
    let mut shaper = Shaper::new();
    let shaped = shaper.shape_run(&primary, 14.0, &[], "A");
    let glyph_id = shaped[0].glyph_id;
    assert_ne!(glyph_id, 0);
    let first = idx.ensure_glyph_id(glyph_id, h, sk);
    assert!(first, "first ensure must rasterize + allocate");
    assert!(idx.contains_glyph_id(glyph_id, h, sk));
    assert!(idx.glyph_id_slot(glyph_id, h, sk).is_some());
    let second = idx.ensure_glyph_id(glyph_id, h, sk);
    assert!(!second, "second ensure is a cache hit");
}

/// `.notdef` stores a negative entry, like a charmap miss.
#[cfg(not(windows))]
#[test]
fn ensure_glyph_id_zero_lands_a_negative_slot() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let h = idx.cell_metrics().height;
    let sk = SizingKey::default();
    idx.ensure_glyph_id(0, h, sk);
    assert!(idx.contains_glyph_id(0, h, sk));
    assert_eq!(idx.glyph_id_slot(0, h, sk), None);
    assert_eq!(idx.drain_pending().count(), 0);
}

/// The same raw `glyph_id` primed against two faces keys two slots;
/// the second `ensure_glyph_id` reports work only because the key
/// differs.
#[cfg(not(windows))]
#[test]
fn glyph_id_slots_key_on_font_id_so_faces_do_not_collide() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(512));
    let h = idx.cell_metrics().height;
    let face0 = SizingKey::default();
    let face2 = SizingKey::default().with_font_id(2);
    assert_ne!(face0, face2, "font-id must change the key");
    assert!(
        idx.ensure_glyph_id(7, h, face0),
        "first face-0 insert works"
    );
    assert!(
        idx.ensure_glyph_id(7, h, face2),
        "same id under a different font-id is a distinct slot, not a hit",
    );
    assert!(
        !idx.ensure_glyph_id(7, h, face0),
        "re-inserting the face-0 key is a cache hit",
    );
    assert!(idx.contains_glyph_id(7, h, face0));
    assert!(idx.contains_glyph_id(7, h, face2));
}

/// The style rides the `SizingKey`, so bold and regular `'A'` key
/// separate slots.
#[cfg(not(windows))]
#[test]
fn styled_cells_get_distinct_atlas_slots() {
    let stack = stack();
    let mut idx = GlyphIndex::new(stack, 14.0, nz(512));
    let h = idx.cell_metrics().height;
    let regular = SizingKey::default();
    let bold = SizingKey::default().with_style(FontStyle {
        bold: true,
        italic: false,
    });
    assert!(idx.ensure('A', h, regular), "regular 'A' is a miss");
    assert!(
        idx.ensure('A', h, bold),
        "bold 'A' must be a separate miss, not a hit on the regular slot",
    );
    assert!(idx.contains('A', h, regular));
    assert!(idx.contains('A', h, bold));
}

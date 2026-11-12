use super::*;

#[test]
fn empty_grid_emits_one_bg_per_cell_and_no_fg() {
    let g = drive(2, 3, b"");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.rects.len(), 6);
    assert_eq!(buffers.glyphs, Vec::<GlyphDraw>::new());
    let last = buffers.rects.last().unwrap();
    assert_eq!(last.origin_px, [16.0, 16.0]);
    assert_eq!(last.size_px, [8.0, 16.0]);
}

#[test]
fn ascii_glyph_emits_a_fg_instance_at_baseline_offset() {
    let g = drive(1, 3, b"A");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.rects.len(), 3);
    assert_eq!(buffers.glyphs.len(), 1);
    let inst = &buffers.glyphs[0];
    assert_eq!(inst.origin_px, [1.0, 2.0]);
    assert_eq!(inst.size_px, [6.0, 10.0]);
    assert_eq!(inst.src_min_px, [0.0, 0.0]);
    assert_eq!(inst.src_max_px, [6.0, 10.0]);
}

#[test]
fn reverse_video_swaps_fg_and_bg_colors() {
    // Cursor hidden so its reverse-video pass does not stack on SGR 7.
    let g = drive(1, 2, b"\x1b[?25lA\x1b[7mB");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let theme = Theme::default();
    assert_eq!(buffers.glyphs[0].color, theme.fg);
    assert_eq!(buffers.rects[0].color, theme.bg);
    assert_eq!(buffers.glyphs[1].color, theme.bg);
    assert_eq!(buffers.rects[1].color, theme.fg);
}

#[test]
fn a_glyph_and_its_decorations_are_weighted_against_the_bg_they_paint_on() {
    let g = drive(1, 2, b"\x1b[?25l\x1b[4mA\x1b[7mB");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
    let grounds: Vec<f32> = buffers.glyphs.iter().map(|g| g.ground_luminance).collect();
    assert_eq!(
        grounds,
        [relative_luminance(theme.bg), relative_luminance(theme.fg)]
    );
    let deco_grounds: Vec<f32> = buffers.decos.iter().map(|d| d.ground_luminance).collect();
    assert_eq!(deco_grounds, grounds, "one underline per cell");
}

#[test]
fn a_translucent_default_bg_weights_glyphs_by_its_configured_opaque_color() {
    let straight_bg = [1.0, 1.0, 1.0, 0.25];
    let premultiplied = Theme {
        bg: [0.25, 0.25, 0.25, 0.25],
        ..Theme::default()
    };
    let theme = ResolvedTheme::new(&premultiplied).with_straight_bg(straight_bg);
    let g = drive(1, 1, b"\x1b[?25lA");
    let atlas = MockAtlas::full(slot());
    let mut buffers = RowPaint::default();
    CellPainter::new(g.screen(), metrics(), &theme, &atlas, &ShapeFrame::empty())
        .without_compaction()
        .with_viewport(0, 1)
        .extend_screen(&mut buffers);
    assert_eq!(
        buffers.glyphs[0].ground_luminance,
        relative_luminance(straight_bg)
    );
}

#[test]
fn overlay_text_is_weighted_against_the_overlay_bg() {
    let atlas = MockAtlas::full(slot());
    let mut out = RowPaint::default();
    extend_confirm(
        &ConfirmOverlay {
            label: "y".to_owned(),
        },
        metrics(),
        2,
        4,
        &atlas,
        &mut out,
    );
    assert_eq!(
        out.glyphs[0].ground_luminance,
        relative_luminance(CONFIRM_BAR_BG)
    );
}

/// DECSCNM and SGR 7 compose by XOR on the *resolved* pair.
#[test]
fn decscnm_xors_with_sgr_reverse_over_the_resolved_pair() {
    let g = drive(1, 4, b"\x1b[?25lA\x1b[7mB\x1b[0;38;5;1;48;5;4mC\x1b[7mD");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let resolved = ResolvedTheme::new(&theme);
    let pal = |i: u8| resolve_fg(Color::Indexed(i), &resolved);
    let (dfg, dbg) = (theme.fg, theme.bg);
    let (efg, ebg) = (pal(1), pal(4));

    let plain = build_instances(g.screen(), metrics(), &theme, &atlas);
    let pairs: Vec<_> = (0..4)
        .map(|i| (plain.glyphs[i].color, plain.rects[i].color))
        .collect();
    assert_eq!(
        pairs,
        vec![(dfg, dbg), (dbg, dfg), (efg, ebg), (ebg, efg)],
        "without DECSCNM only SGR 7 swaps",
    );

    let reversed = build_instances_with(g.screen(), metrics(), &theme, &atlas, |painter| {
        painter.with_reverse_video(true)
    });
    let pairs: Vec<_> = (0..4)
        .map(|i| (reversed.glyphs[i].color, reversed.rects[i].color))
        .collect();
    assert_eq!(
        pairs,
        vec![(dbg, dfg), (dfg, dbg), (ebg, efg), (efg, ebg)],
        "DECSCNM flips every cell, SGR 7 cells back to normal",
    );
}

#[test]
fn atlas_miss_drops_only_the_fg_instance() {
    let mut atlas_map = HashMap::new();
    atlas_map.insert('A', slot());
    let atlas = MockAtlas {
        map: atlas_map,
        gid_map: HashMap::new(),
        fitted_map: HashMap::new(),
        clusters: HashMap::new(),
    };
    let g = drive(1, 2, b"AB");
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.rects.len(), 2, "every cell still has a bg");
    assert_eq!(buffers.glyphs.len(), 1, "B produced no glyph instance");
}

#[test]
fn sgr_indexed_color_is_resolved_via_palette() {
    let g = drive(1, 1, b"\x1b[?25l\x1b[31mZ");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let expected = resolve_fg(Color::Indexed(1), &ResolvedTheme::new(&Theme::default()));
    assert_eq!(buffers.glyphs[0].color, expected);
}

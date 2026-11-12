use super::*;

#[test]
fn preedit_overlay_span_is_reserved_from_the_cell_walk() {
    // Cells under the composition overlay must not be emitted, or
    // their glyph draws through it in the later fg pass.
    let g = drive(1, 5, b"ABCDE");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let overlay = PreeditOverlay {
        anchor: (0, 1),
        text: "あ".to_string(),
        cursor: None,
    };
    let span = preedit_covered_cols(&overlay, g.cols());
    assert_eq!(
        span,
        Some(PreeditSpan {
            row: 0,
            col_start: 1,
            col_end: 3,
        }),
        "wide char covers cols 1 and 2",
    );
    let buffers = build_instances_with(g.screen(), metrics(), &theme, &atlas, |painter| {
        painter.with_preedit(span)
    });
    let cw = metrics().width as f32;
    let leaked = |x: f32| {
        let col = (x / cw).round() as u16;
        (col == 1 || col == 2).then_some(col)
    };
    for inst in &buffers.rects {
        assert!(
            leaked(inst.origin_px[0]).is_none(),
            "cell bg leaked under the pre-edit overlay at col {:?}",
            leaked(inst.origin_px[0]),
        );
    }
    for inst in &buffers.glyphs {
        assert!(
            leaked(inst.origin_px[0]).is_none(),
            "glyph leaked under the pre-edit overlay at col {:?}",
            leaked(inst.origin_px[0]),
        );
    }
}

#[test]
fn preedit_span_matches_painted_extent() {
    // The reserved span and the painted span walk the same clamping
    // formula; pin them to the same right edge.
    let atlas = MockAtlas::full(slot());
    let overlay = PreeditOverlay {
        anchor: (0, 1),
        text: "あxい".to_string(), // 2 + 1 + 2 = 5 cells from col 1
        cursor: None,
    };
    let cols = 6; // col 1 + 5 cells == exactly the right edge
    let mut buffers = RowPaint::default();
    extend_preedit(&overlay, metrics(), 1, cols, &atlas, &mut buffers);
    let cw = metrics().width as f32;
    let painted_end = buffers
        .rects
        .iter()
        .map(|b| b.origin_px[0] + b.size_px[0])
        .fold(0.0_f32, f32::max);
    let span_end = preedit_covered_cols(&overlay, cols)
        .expect("non-empty overlay")
        .col_end;
    assert_eq!(
        f32::from(span_end) * cw,
        painted_end,
        "reserved span must end where the painted overlay ends",
    );
}

#[test]
fn extend_preedit_empty_text_is_noop() {
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (0, 0),
        text: String::new(),
        cursor: None,
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 5, 80, &atlas, &mut out);
    assert_eq!(out.rects, Vec::<Rect>::new());
    assert_eq!(out.glyphs, Vec::<GlyphDraw>::new());
}

#[test]
fn extend_preedit_emits_bg_underline_glyph_per_ascii_char() {
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (1, 2),
        text: "ab".to_owned(),
        cursor: None,
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 5, 80, &atlas, &mut out);
    assert_eq!(out.rects.len(), 4);
    assert_eq!(out.glyphs.len(), 2);
    assert_eq!(out.rects[0].origin_px, [16.0, 16.0]);
    assert_eq!(out.rects[0].size_px, [8.0, 16.0]);
    assert_eq!(out.rects[0].color, PREEDIT_BG);
    assert_eq!(out.rects[1].origin_px, [16.0, 30.0]);
    assert_eq!(out.rects[1].size_px[1], PREEDIT_UNDERLINE_PX);
    assert_eq!(out.rects[2].origin_px, [24.0, 16.0]);
}

#[test]
fn extend_preedit_advances_two_cells_for_wide_glyph() {
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (0, 0),
        text: "あい".to_owned(),
        cursor: None,
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 5, 80, &atlas, &mut out);
    assert_eq!(out.rects[0].origin_px, [0.0, 0.0]);
    assert_eq!(out.rects[0].size_px, [16.0, 16.0]);
    assert_eq!(out.rects[2].origin_px, [16.0, 0.0]);
    assert_eq!(out.rects[2].size_px, [16.0, 16.0]);
}

#[test]
fn extend_preedit_anchor_out_of_range_is_noop() {
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (10, 0),
        text: "x".to_owned(),
        cursor: None,
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 5, 80, &atlas, &mut out);
    assert_eq!(out.rects, Vec::<Rect>::new());
}

#[test]
fn extend_preedit_col_at_or_past_grid_cols_is_noop() {
    // A daemon-side cursor briefly reporting `col == cols` must not
    // push off-screen instances.
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (0, 80),
        text: "abc".to_owned(),
        cursor: None,
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 5, 80, &atlas, &mut out);
    assert!(out.rects.is_empty(), "no bg pushed when col >= grid_cols");
    assert!(
        out.glyphs.is_empty(),
        "no glyph pushed when col >= grid_cols"
    );
}

#[test]
fn extend_preedit_active_segment_paints_thicker_underline() {
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (0, 0),
        text: "abcde".to_owned(),
        cursor: Some((1, 3)),
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 1, 80, &atlas, &mut out);
    assert!(
        (out.rects[1].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
        "char 0 'a' outside active range: default underline",
    );
    assert!(
        (out.rects[3].size_px[1] - PREEDIT_ACTIVE_UNDERLINE_PX).abs() < f32::EPSILON,
        "char 1 'b' inside active range: thicker underline",
    );
    assert!(
        (out.rects[5].size_px[1] - PREEDIT_ACTIVE_UNDERLINE_PX).abs() < f32::EPSILON,
        "char 2 'c' inside active range: thicker underline",
    );
    assert!(
        (out.rects[7].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
        "char 3 'd' outside active range: default underline",
    );
}

#[test]
fn extend_preedit_empty_active_range_falls_back_to_default_underline() {
    // start == end is a position, not a range.
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (0, 0),
        text: "ab".to_owned(),
        cursor: Some((1, 1)),
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 1, 80, &atlas, &mut out);
    for i in [1, 3] {
        assert!(
            (out.rects[i].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
            "no thicker underline when cursor is a single point",
        );
    }
}

#[test]
fn extend_preedit_active_segment_handles_multibyte_chars() {
    // Each char is 3 bytes; (3, 6) covers exactly the second.
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (0, 0),
        text: "あいう".to_owned(),
        cursor: Some((3, 6)),
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 1, 80, &atlas, &mut out);
    assert!(
        (out.rects[1].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
        "あ outside active range: default underline",
    );
    assert!(
        (out.rects[3].size_px[1] - PREEDIT_ACTIVE_UNDERLINE_PX).abs() < f32::EPSILON,
        "い inside active range: thicker underline",
    );
    assert!(
        (out.rects[5].size_px[1] - PREEDIT_UNDERLINE_PX).abs() < f32::EPSILON,
        "う outside active range: default underline",
    );
}

#[test]
fn extend_preedit_clips_at_right_edge_of_row() {
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (0, 4),
        text: "abc".to_owned(),
        cursor: None,
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 1, 5, &atlas, &mut out);
    assert_eq!(out.rects.len(), 2);
    assert_eq!(out.glyphs.len(), 1);
}

#[test]
fn extend_preedit_clips_wide_glyph_at_right_edge_to_one_cell() {
    // A wide glyph at the last cell clips rather than wrapping (which
    // would lie about where the committed char lands) or vanishing.
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (0, 4),
        text: "あ".to_owned(),
        cursor: None,
    };
    let atlas = MockAtlas::full(slot());
    extend_preedit(&overlay, metrics(), 1, 5, &atlas, &mut out);
    // MockAtlas only knows ASCII, so the fg pass is empty; the bg /
    // underline geometry carries the clip.
    assert_eq!(out.rects.len(), 2, "bg quad + underline only");
    assert_eq!(out.rects[0].size_px, [8.0, 16.0], "bg clipped to 1 cell");
    assert_eq!(out.rects[1].size_px[0], 8.0, "underline clipped to 1 cell");
}

#[test]
fn extend_preedit_atlas_miss_still_paints_bg_and_underline() {
    let mut out = RowPaint::default();
    let overlay = PreeditOverlay {
        anchor: (0, 0),
        text: "?".to_owned(),
        cursor: None,
    };
    let atlas = MockAtlas {
        map: HashMap::new(),
        gid_map: HashMap::new(),
        fitted_map: HashMap::new(),
        clusters: HashMap::new(),
    };
    extend_preedit(&overlay, metrics(), 1, 5, &atlas, &mut out);
    assert_eq!(out.rects.len(), 2, "bg + underline survive an atlas miss");
    assert!(out.glyphs.is_empty(), "fg pass skipped on atlas miss");
}

fn preedit(text: &str, col: u16) -> PreeditOverlay {
    PreeditOverlay {
        anchor: (0, col),
        text: text.to_owned(),
        cursor: None,
    }
}

#[test]
fn preedit_gives_a_cluster_the_cells_the_grid_gives_it() {
    for (text, cells) in [
        ("e\u{301}x", 2),
        ("❤\u{FE0F}", 2),
        ("👍🏻", 2),
        ("🇯🇵", 2),
        ("a字", 3),
    ] {
        let span = preedit_covered_cols(&preedit(text, 1), 20).expect("non-empty");
        assert_eq!(span.col_end - span.col_start, cells, "{text:?}");
    }
}

#[test]
fn preedit_draws_a_cluster_with_its_shaped_glyphs() {
    let mut atlas = MockAtlas::full(slot());
    for (id, flag) in [(1, "🇯🇵"), (2, "🇯🇲")] {
        atlas.clusters.insert(
            flag.to_owned(),
            vec![ClusterGlyph {
                glyph_id: id,
                font_id: 0,
                advance_px: 16.0,
                x_offset_px: 0.0,
                y_offset_px: 0.0,
            }],
        );
        atlas.gid_map.insert(
            id,
            GlyphSlot {
                atlas_px: [id, 0],
                ..slot()
            },
        );
    }
    let src_of = |text: &str| {
        let mut out = RowPaint::default();
        extend_preedit(&preedit(text, 0), metrics(), 1, 10, &atlas, &mut out);
        assert_eq!(out.glyphs.len(), 1, "{text:?} draws one shaped glyph");
        out.glyphs[0].src_min_px
    };
    assert_ne!(src_of("🇯🇵"), src_of("🇯🇲"));
}

#[test]
fn overlay_glyphs_never_draw_past_the_right_edge() {
    let mut atlas = MockAtlas::full(slot());
    let wide = GlyphSlot {
        size_px: [14, 10],
        ..slot()
    };
    atlas.map.insert('字', wide);
    let cols = 5;
    let edge = f32::from(cols) * metrics().width as f32;
    let mut out = RowPaint::default();
    extend_preedit(&preedit("ab字", 2), metrics(), 1, cols, &atlas, &mut out);
    extend_search(
        &SearchOverlay {
            label: "abcd字".to_owned(),
            visible_hits: Vec::new(),
        },
        metrics(),
        2,
        cols,
        true,
        &atlas,
        &mut out,
    );
    assert_eq!(
        out.glyphs.len(),
        3 + 5,
        "each clipped 字 keeps its visible half"
    );
    for q in &out.glyphs {
        assert!(q.origin_px[0] + q.size_px[0] <= edge, "{q:?}");
    }
}

use super::*;

#[test]
fn reserved_bottom_rows_keeps_bar_row_free_of_cells() {
    // Cells under the bar must not be emitted, or their glyphs
    // double through the label.
    let g = drive(2, 3, b"AB\r\nCD");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let buffers = build_instances_with(g.screen(), metrics(), &theme, &atlas, |painter| {
        painter.with_reserved_bottom_rows(1)
    });
    assert_eq!(
        buffers.rects.len(),
        3,
        "bottom row's bg cells must be suppressed"
    );
    let bar_y = f32::from(g.rows() - 1) * metrics().height as f32;
    for inst in &buffers.rects {
        assert!(
            inst.origin_px[1] < bar_y,
            "bg cell leaked onto the reserved bar row"
        );
    }
    for inst in &buffers.glyphs {
        assert!(
            inst.origin_px[1] < bar_y,
            "fg glyph leaked onto the reserved bar row"
        );
    }
}

#[test]
fn extend_search_empty_overlay_is_noop() {
    // The App can push a fresh-but-empty overlay between
    // Off and Composing without polluting the frame.
    let mut out = RowPaint::default();
    let overlay = SearchOverlay::default();
    let atlas = MockAtlas::full(slot());
    extend_search(&overlay, metrics(), 5, 80, true, &atlas, &mut out);
    assert_eq!(out.rects, Vec::<Rect>::new());
    assert_eq!(out.glyphs, Vec::<GlyphDraw>::new());
}

#[test]
fn extend_search_keeps_its_highlights_when_it_loses_the_bar_row() {
    // A chord can arm a confirmation while a search is composing.
    // The question takes the row; the hit highlights are the
    // search's own rows and stay.
    let mut out = RowPaint::default();
    let overlay = SearchOverlay {
        label: "search: ab".to_owned(),
        visible_hits: vec![SearchHitSpan {
            row: 2,
            col_start: 3,
            col_end: 7,
            is_current: true,
        }],
    };
    let atlas = MockAtlas::full(slot());
    extend_search(&overlay, metrics(), 5, 80, false, &atlas, &mut out);
    assert_eq!(out.rects.len(), 1, "the highlight, and no bar background");
    assert_eq!(out.rects[0].color, SEARCH_CURRENT_BG);
    assert!(out.glyphs.is_empty(), "no label glyphs without the bar");
}

#[test]
fn extend_search_paints_match_bg_for_visible_hit() {
    let mut out = RowPaint::default();
    let overlay = SearchOverlay {
        label: String::new(),
        visible_hits: vec![SearchHitSpan {
            row: 2,
            col_start: 3,
            col_end: 7,
            is_current: false,
        }],
    };
    let atlas = MockAtlas::full(slot());
    extend_search(&overlay, metrics(), 5, 80, true, &atlas, &mut out);
    assert_eq!(out.rects.len(), 1);
    assert_eq!(out.rects[0].origin_px, [24.0, 32.0]);
    assert_eq!(out.rects[0].size_px, [32.0, 16.0]);
    assert_eq!(out.rects[0].color, SEARCH_MATCH_BG);
}

#[test]
fn extend_search_current_hit_paints_in_vivid_color() {
    let mut out = RowPaint::default();
    let overlay = SearchOverlay {
        label: String::new(),
        visible_hits: vec![SearchHitSpan {
            row: 0,
            col_start: 0,
            col_end: 2,
            is_current: true,
        }],
    };
    let atlas = MockAtlas::full(slot());
    extend_search(&overlay, metrics(), 5, 80, true, &atlas, &mut out);
    assert_eq!(out.rects.len(), 1);
    assert_eq!(out.rects[0].color, SEARCH_CURRENT_BG);
}

#[test]
fn extend_search_clips_hit_past_grid_edges() {
    // The daemon's viewport edits and the client's hit list can race
    // during a rapid scroll; a panic there would lose a frame.
    let mut out = RowPaint::default();
    let overlay = SearchOverlay {
        label: String::new(),
        visible_hits: vec![
            SearchHitSpan {
                row: 99,
                col_start: 0,
                col_end: 4,
                is_current: false,
            },
            SearchHitSpan {
                row: 1,
                col_start: 6,
                col_end: 999,
                is_current: false,
            },
        ],
    };
    let atlas = MockAtlas::full(slot());
    extend_search(&overlay, metrics(), 5, 8, true, &atlas, &mut out);
    assert_eq!(out.rects.len(), 1);
    assert_eq!(out.rects[0].size_px, [16.0, 16.0]);
}

#[test]
fn extend_search_paints_bar_at_bottom_row_with_label_glyphs() {
    let mut out = RowPaint::default();
    let overlay = SearchOverlay {
        label: "ab".to_owned(),
        visible_hits: vec![],
    };
    let atlas = MockAtlas::full(slot());
    extend_search(&overlay, metrics(), 3, 10, true, &atlas, &mut out);
    assert_eq!(out.rects.len(), 1, "1 bar bg quad");
    assert_eq!(out.rects[0].origin_px, [0.0, 32.0]);
    assert_eq!(out.rects[0].size_px, [80.0, 16.0]);
    assert_eq!(out.rects[0].color, SEARCH_BAR_BG);
    assert_eq!(out.glyphs.len(), 2, "1 fg quad per label char");
}

#[test]
fn extend_search_label_clips_at_right_edge() {
    let mut out = RowPaint::default();
    let overlay = SearchOverlay {
        label: "abcdef".to_owned(),
        visible_hits: vec![],
    };
    let atlas = MockAtlas::full(slot());
    extend_search(&overlay, metrics(), 2, 3, true, &atlas, &mut out);
    assert_eq!(out.glyphs.len(), 3);
}

#[test]
fn extend_search_zero_grid_is_noop() {
    let mut out = RowPaint::default();
    let overlay = SearchOverlay {
        label: "find: x".to_owned(),
        visible_hits: vec![SearchHitSpan {
            row: 0,
            col_start: 0,
            col_end: 1,
            is_current: false,
        }],
    };
    let atlas = MockAtlas::full(slot());
    extend_search(&overlay, metrics(), 0, 80, true, &atlas, &mut out);
    assert_eq!(out.rects, Vec::<Rect>::new());
    assert_eq!(out.glyphs, Vec::<GlyphDraw>::new());
    let mut out2 = RowPaint::default();
    extend_search(&overlay, metrics(), 5, 0, true, &atlas, &mut out2);
    assert_eq!(out2.rects, Vec::<Rect>::new());
    assert_eq!(out2.glyphs, Vec::<GlyphDraw>::new());
}

#[test]
fn extend_link_preview_paints_bar_at_bottom_row_with_search_bar_color() {
    // Same neutral chrome as the search bar: a hover preview is
    // informational, not the destructive-prompt CONFIRM_BAR_BG.
    let mut out = RowPaint::default();
    let overlay = LinkPreviewOverlay {
        text: "https://e".to_owned(),
    };
    let atlas = MockAtlas::full(slot());
    extend_link_preview(&overlay, metrics(), 3, 10, &atlas, &mut out);
    assert_eq!(out.rects.len(), 1, "1 bar bg quad");
    assert_eq!(out.rects[0].origin_px, [0.0, 32.0]);
    assert_eq!(out.rects[0].color, SEARCH_BAR_BG);
    assert_eq!(out.glyphs.len(), overlay.text.chars().count());
}

#[test]
fn a_glyph_reaching_down_into_the_chrome_row_is_cut_at_its_top_edge() {
    // An OSC 66 run scaled across rows is bottom-aligned inside its
    // block, so a producer can aim ink into the reserved row from
    // the row above; it rides the same pass as the bar's own text.
    let mut out = RowPaint::default();
    out.glyphs.push(GlyphDraw {
        origin_px: [0.0, 16.0],
        size_px: [8.0, 32.0],
        atlas: AtlasKind::Mono,
        src_min_px: [0.0, 0.0],
        src_max_px: [8.0, 16.0],
        color: [1.0; 4],
        ground_luminance: 0.0,
    });
    out.glyphs.push(GlyphDraw {
        origin_px: [0.0, 34.0],
        size_px: [8.0, 10.0],
        atlas: AtlasKind::Mono,
        src_min_px: [0.0, 0.0],
        src_max_px: [8.0, 16.0],
        color: [1.0; 4],
        ground_luminance: 0.0,
    });
    clip_cells_above(&mut out, 32.0);
    assert_eq!(
        out.glyphs.len(),
        1,
        "the quad starting inside the row is gone"
    );
    assert_eq!(out.glyphs[0].size_px[1], 16.0);
    // Half the height kept, so half the source `v` span.
    assert!((out.glyphs[0].src_max_px[1] - 8.0).abs() < f32::EPSILON);
}

#[test]
fn link_preview_wider_than_the_grid_ends_in_an_ellipsis() {
    // The whole point of the preview is that the user reads the true
    // target; a cut that looks like the end of the URI would be read
    // as one.
    let mut out = RowPaint::default();
    let overlay = LinkPreviewOverlay {
        text: "https://example.com/very/long/path".to_owned(),
    };
    // The renderer primes the ellipsis alongside the text, so
    // the atlas answers for it here too.
    let mut atlas = MockAtlas::full(slot());
    atlas.map.insert(BAR_ELLIPSIS, slot());
    extend_link_preview(&overlay, metrics(), 3, 10, &atlas, &mut out);
    assert_eq!(fit_bar_text_to_cells(&overlay.text, 10), "https://e…");
    assert_eq!(out.glyphs.len(), 10, "one quad per column");
    // The mark occupies the last cell, so a clip is visible rather
    // than a URI that merely stops.
    let cw = metrics().width as f32;
    let offset_x = slot().offset_px[0] as f32;
    assert_eq!(out.glyphs[9].origin_px[0], 9.0f32.mul_add(cw, offset_x));
}

#[test]
fn link_preview_within_the_grid_is_left_verbatim() {
    assert_eq!(
        fit_bar_text_to_cells("https://e", 10),
        Cow::Borrowed("https://e")
    );
    // Exactly filling the row is not truncation.
    assert_eq!(
        fit_bar_text_to_cells("https://ex", 10),
        Cow::Borrowed("https://ex")
    );
}

#[test]
fn link_preview_never_puts_half_a_wide_glyph_in_the_last_cell() {
    // A two-cell glyph with one cell left would otherwise get its
    // span clamped to one while its full quad painted past the bar.
    assert_eq!(fit_bar_text_to_cells("ab漢字", 5), "ab漢…");
    assert_eq!(fit_bar_text_to_cells("漢字漢", 5), "漢字…");
    // A single column can hold nothing but the truncation mark.
    assert_eq!(fit_bar_text_to_cells("漢字", 1), "…");
}

#[test]
fn bottom_bar_claim_gives_the_row_to_search_and_confirm_over_the_preview() {
    // The preview's bg is opaque, so drawing it into a row a search
    // or confirm bar already claimed would hide that bar's text.
    for (search, confirm) in [(true, false), (false, true), (true, true)] {
        assert_eq!(
            bottom_bar_claim(ChromeBar::from_flags(search, confirm), false, true),
            BottomBarClaim::Chrome
        );
    }
}

#[test]
fn bottom_bar_claim_gives_the_row_to_an_ime_composition_over_the_preview() {
    // A preedit anchored on the bottom row emits its glyphs before
    // the preview's, and every fg quad draws after every bg one, so
    // the two would overprint. The composition is the deliberate
    // interaction, so the preview yields the row entirely.
    assert_eq!(
        bottom_bar_claim(ChromeBar::None, true, true),
        BottomBarClaim::Free
    );
}

#[test]
fn bottom_bar_claim_draws_the_preview_when_no_other_bar_is_open() {
    assert_eq!(
        bottom_bar_claim(ChromeBar::None, false, true),
        BottomBarClaim::LinkPreview
    );
}

#[test]
fn bottom_bar_claim_reserves_nothing_with_every_bar_closed() {
    assert_eq!(
        bottom_bar_claim(ChromeBar::None, false, false),
        BottomBarClaim::Free
    );
}

#[test]
fn extend_link_preview_empty_text_is_noop() {
    let mut out = RowPaint::default();
    let overlay = LinkPreviewOverlay::default();
    let atlas = MockAtlas::full(slot());
    extend_link_preview(&overlay, metrics(), 3, 10, &atlas, &mut out);
    assert_eq!(out.rects, Vec::<Rect>::new());
    assert_eq!(out.glyphs, Vec::<GlyphDraw>::new());
}

#[test]
fn extend_link_preview_zero_grid_is_noop() {
    let mut out = RowPaint::default();
    let overlay = LinkPreviewOverlay {
        text: "https://example.com".to_owned(),
    };
    let atlas = MockAtlas::full(slot());
    extend_link_preview(&overlay, metrics(), 0, 80, &atlas, &mut out);
    assert_eq!(out.rects, Vec::<Rect>::new());
    assert_eq!(out.glyphs, Vec::<GlyphDraw>::new());
}

proptest::proptest! {
    /// No fitted bar text overflows its budget, and a truncated one
    /// is a prefix of the original plus the mark: a wide glyph must
    /// not be half-drawn into the cell the mark needs.
    #[test]
    fn fitted_bar_text_never_overflows_its_budget(
        text in "\\PC{0,40}",
        cols in 1u16..40,
    ) {
        let fitted = fit_bar_text_to_cells(&text, cols);
        let width: u32 = felis_grid::text_cells(&fitted)
            .map(|(_, w)| u32::from(w))
            .sum();
        proptest::prop_assert!(
            width <= u32::from(cols),
            "{fitted:?} occupies {width} cells, past {cols}"
        );
        if fitted.as_ref() != text {
            let head = fitted
                .strip_suffix(BAR_ELLIPSIS)
                .expect("truncated text ends in the truncation mark");
            proptest::prop_assert!(text.starts_with(head));
        }
    }
}

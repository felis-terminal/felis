use super::*;

#[test]
fn bidi_override_cell_paints_with_warning_marker() {
    // The cell carrying U+202E paints the marker whether the parser
    // places it as its own cell or folds it into a Cluster.
    let mut g = Grid::new(1, 4);
    let mut p = Parser::new();
    p.advance(&mut g, "\x1b[?25lA\u{202E}B".as_bytes());
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let mut marked = false;
    for r in 0..g.rows() {
        for c in 0..g.cols() {
            let Some(cell) = g.cell(r, c) else { continue };
            if cell_contains_bidi_override(cell.grapheme, g.screen().cluster_table()) {
                let idx = usize::from(r) * usize::from(g.cols()) + usize::from(c);
                assert_eq!(
                    buffers.rects[idx].color, BIDI_MARKER_BG,
                    "bidi-override cell at ({r},{c}) must paint with marker bg"
                );
                marked = true;
            }
        }
    }
    assert!(
        marked,
        "parser should have placed the RLO in some cell — neither Char nor Cluster found"
    );
}

#[test]
fn bidi_override_at_column_zero_marks_the_character_after_it() {
    let g = drive(1, 4, "\x1b[?25l\u{202E}AB".as_bytes());
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
    assert_eq!(buffers.rects[0].color, BIDI_MARKER_BG);
    assert_eq!(buffers.rects[1].color, theme.bg);
}

#[test]
fn ordinary_text_does_not_trigger_bidi_marker() {
    // RTL script content is text, not an attack vector.
    let mut g = Grid::new(1, 8);
    let mut p = Parser::new();
    p.advance(&mut g, b"\x1b[?25lhello");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let theme = Theme::default();
    for inst in &buffers.rects {
        assert_ne!(
            inst.color, BIDI_MARKER_BG,
            "non-bidi text must not paint the warning marker"
        );
    }
    assert_eq!(buffers.rects[0].color, theme.bg);
}

#[test]
fn bidi_marker_overrides_cursor_swap() {
    // The cursor's reverse-video pass must not mask the marker.
    let g = drive(1, 4, "A\u{202E}\x1b[1;1H".as_bytes());
    assert!(cell_contains_bidi_override(
        g.cell(0, 0).unwrap().grapheme,
        g.screen().cluster_table()
    ));
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.rects[0].color, BIDI_MARKER_BG);
    assert!(
        !buffers.glyphs.is_empty(),
        "the `A` under the cursor draws a glyph"
    );
    assert!(
        buffers.glyphs.iter().all(|f| f.color == BIDI_MARKER_FG),
        "the glyph on the cursor cell keeps the marker fg"
    );
}

#[test]
fn bidi_marker_overrides_underline_cursor_marker() {
    // The bidi warning paints the whole cell; the marker must not
    // also fire.
    let mut g = Grid::new(1, 4);
    let mut p = Parser::new();
    p.advance(&mut g, "\x1b[4 qA\u{202E}\x1b[1;1H".as_bytes());
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let marker = buffers
        .rects
        .iter()
        .find(|b| b.size_px[1] == CURSOR_MARKER_PX);
    assert!(
        marker.is_none(),
        "bidi marker must suppress the underline marker"
    );
}

#[test]
fn bidi_marker_overrides_bar_cursor_marker() {
    let mut g = Grid::new(1, 4);
    let mut p = Parser::new();
    p.advance(&mut g, "\x1b[6 qA\u{202E}\x1b[1;1H".as_bytes());
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let marker = buffers
        .rects
        .iter()
        .find(|b| b.size_px[0] == CURSOR_MARKER_PX);
    assert!(
        marker.is_none(),
        "bidi marker must suppress the bar marker too"
    );
}

#[test]
fn selection_highlight_paints_cells_inside_the_range() {
    let g = drive(1, 5, b"\x1b[?25l"); // hide cursor for isolation
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let buffers = build_instances_with(g.screen(), metrics(), &theme, &atlas, |painter| {
        painter.with_selection(Some(SelectionRange {
            start: (0, 1),
            end: (0, 3),
            rectangle: false,
        }))
    });
    assert_eq!(buffers.rects.len(), 5);
    assert_eq!(buffers.rects[0].color, theme.bg, "cell 0 outside selection");
    for (i, buffer) in buffers.rects.iter().enumerate().skip(1).take(3) {
        assert_eq!(buffer.color, SELECTION_BG, "cell {i} should be highlighted");
    }
    assert_eq!(buffers.rects[4].color, theme.bg, "cell 4 outside selection");
}

#[test]
fn selection_highlight_clips_to_boundary_columns_across_rows() {
    let g = drive(3, 4, b"\x1b[?25l");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let buffers = build_instances_with(g.screen(), metrics(), &theme, &atlas, |painter| {
        painter.with_selection(Some(SelectionRange {
            start: (0, 2),
            end: (2, 1),
            rectangle: false,
        }))
    });
    let inside = |idx: usize| matches!(idx, 2..=9);
    for (idx, inst) in buffers.rects.iter().enumerate() {
        if inside(idx) {
            assert_eq!(inst.color, SELECTION_BG, "idx {idx} should be highlighted");
        } else {
            assert_eq!(inst.color, theme.bg, "idx {idx} outside selection");
        }
    }
}

#[test]
fn no_selection_leaves_every_cell_on_theme_bg() {
    let g = drive(1, 5, b"\x1b[?25l");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
    assert_eq!(buffers.rects.len(), 5);
    for (i, inst) in buffers.rects.iter().enumerate() {
        assert_eq!(inst.color, theme.bg, "cell {i}");
    }
}

#[test]
fn bidi_marker_wins_over_selection_highlight() {
    // A bidi-override cell inside a selection paints the marker, or
    // an attacker could hide behind the selection tint.
    let mut g = Grid::new(1, 4);
    let mut p = Parser::new();
    p.advance(&mut g, "\x1b[?25lA\u{202E}B".as_bytes());
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let buffers = build_instances_with(g.screen(), metrics(), &theme, &atlas, |painter| {
        painter.with_selection(Some(SelectionRange {
            start: (0, 0),
            end: (0, 3),
            rectangle: false,
        }))
    });
    for r in 0..g.rows() {
        for c in 0..g.cols() {
            let Some(cell) = g.cell(r, c) else { continue };
            if cell_contains_bidi_override(cell.grapheme, g.screen().cluster_table()) {
                let idx = usize::from(r) * usize::from(g.cols()) + usize::from(c);
                assert_eq!(
                    buffers.rects[idx].color, BIDI_MARKER_BG,
                    "bidi cell at ({r},{c}) must keep marker bg",
                );
            }
        }
    }
}

#[test]
fn rectangle_selection_clips_every_row_to_column_range() {
    let g = drive(3, 4, b"\x1b[?25l");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let buffers = build_instances_with(g.screen(), metrics(), &theme, &atlas, |painter| {
        painter.with_selection(Some(SelectionRange {
            start: (0, 1),
            end: (2, 2),
            rectangle: true,
        }))
    });
    let inside = |r: u16, c: u16| (1..=2).contains(&c) && (0..=2).contains(&r);
    for r in 0..3u16 {
        for c in 0..4u16 {
            let idx = (usize::from(r) * 4) + usize::from(c);
            let want_highlight = inside(r, c);
            let got = buffers.rects[idx].color;
            if want_highlight {
                assert_eq!(got, SELECTION_BG, "({r},{c}) should be highlighted");
            } else {
                assert_eq!(got, theme.bg, "({r},{c}) should NOT be highlighted");
            }
        }
    }
}

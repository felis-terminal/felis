use super::*;

/// Non-ASCII graphemes split the run; the wide cell falls back to
/// the per-char path.
#[cfg(not(windows))]
#[test]
fn shape_run_stops_at_non_ascii_grapheme() {
    let mut screen = Grid::new(1, 5);
    let mut parser = Parser::new();
    parser.advance(&mut screen, "aあb".as_bytes());
    let stack = stack();
    let primary = stack.primary().clone();
    let mut text = String::new();
    let run = next_shape_run(screen.screen(), 0, 0, &mut text, |c, _style| {
        primary.has_glyph(c)
    })
    .expect("first cell must start a run");
    assert_eq!(text, "a");
    assert_eq!(run.cell_count, 1, "non-ASCII grapheme stops the sweep");
}

/// Single-glyph ligature cluster (the FiraCode-style collapse).
/// Hand-rolled `ShapedGlyph` values so the shape under test cannot
/// drift with the pinned font's GSUB.
#[cfg(not(windows))]
#[test]
fn single_glyph_ligature_cluster_marks_primary_then_trailing() {
    let stack = stack();
    let mut idx = GlyphIndex::new(stack, 14.0, nz(512));
    let mut frame_cells = vec![ShapedCell::None; 4];
    let shaped = [ShapedGlyph {
        glyph_id: 42,
        source_byte_start: 0,
        source_byte_len: 2,
        advance_px: 16.0,
        x_offset_px: 0.0,
        y_offset_px: 0.0,
    }];
    apply_shaped_run(
        &mut frame_cells,
        &mut idx,
        4,
        &RunPlacement {
            row: 0,
            start_col: 0,
            cell_count: 2,
            style: FontStyle::REGULAR,
        },
        &shaped,
    );
    assert_eq!(
        frame_cells[0],
        ShapedCell::Primary {
            glyph_id: 42,
            span_cols: 2,
        },
    );
    assert_eq!(frame_cells[1], ShapedCell::Trailing);
    assert_eq!(frame_cells[2], ShapedCell::None);
}

/// Multi-glyph ligature cluster (the Monaspace-style piece form: a
/// blank lead glyph plus the body, both claiming the full span). The
/// body's ink hangs off the second cell's pen with a negative left
/// bearing, so it must land on cell 1.
#[cfg(not(windows))]
#[test]
fn multi_glyph_ligature_cluster_places_pieces_by_pen_position() {
    let stack = stack();
    let mut idx = GlyphIndex::new(stack, 14.0, nz(512));
    let mut frame_cells = vec![ShapedCell::None; 4];
    let piece = |glyph_id| ShapedGlyph {
        glyph_id,
        source_byte_start: 0,
        source_byte_len: 2,
        advance_px: 8.0,
        x_offset_px: 0.0,
        y_offset_px: 0.0,
    };
    let shaped = [piece(100), piece(200)];
    apply_shaped_run(
        &mut frame_cells,
        &mut idx,
        4,
        &RunPlacement {
            row: 0,
            start_col: 0,
            cell_count: 2,
            style: FontStyle::REGULAR,
        },
        &shaped,
    );
    assert_eq!(
        frame_cells[0],
        ShapedCell::Primary {
            glyph_id: 100,
            span_cols: 1,
        },
        "lead piece (pen 0) anchors at the cluster's first cell",
    );
    assert_eq!(
        frame_cells[1],
        ShapedCell::Primary {
            glyph_id: 200,
            span_cols: 1,
        },
        "body piece (pen = one advance) anchors at the second cell",
    );
    assert_eq!(frame_cells[2], ShapedCell::None);
}

/// A ligature wider than the cells it replaces is dropped, so the
/// per-char path draws its source chars; one within them is kept.
#[cfg(not(windows))]
#[test]
fn a_ligature_wider_than_its_cells_leaves_them_unshaped() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(512));
    let cw = idx.cell_metrics().width as f32;
    let run = RunPlacement {
        row: 0,
        start_col: 0,
        cell_count: 2,
        style: FontStyle::REGULAR,
    };
    let ligature = |advance_px| ShapedGlyph {
        glyph_id: 42,
        source_byte_start: 0,
        source_byte_len: 2,
        advance_px,
        x_offset_px: 0.0,
        y_offset_px: 0.0,
    };
    let mut wide = vec![ShapedCell::None; 2];
    apply_shaped_run(
        &mut wide,
        &mut idx,
        2,
        &run,
        &[ligature(cw.mul_add(2.0, 3.0))],
    );
    assert_eq!(wide, [ShapedCell::None, ShapedCell::None]);
    let mut fitting = vec![ShapedCell::None; 2];
    apply_shaped_run(
        &mut fitting,
        &mut idx,
        2,
        &run,
        &[ligature(cw.mul_add(2.0, 1.0))],
    );
    assert_eq!(
        fitting,
        [
            ShapedCell::Primary {
                glyph_id: 42,
                span_cols: 2,
            },
            ShapedCell::Trailing,
        ],
    );
}

/// Shaping `"->"` with `ss03` through the real run-scan + shape +
/// apply pipeline against Monaspace Neon: both cells consumed, the
/// visible arrow on cell 1.
#[cfg(not(windows))]
#[test]
fn ligature_run_places_monaspace_arrow_pieces() {
    use felis_shaping::{Font, FontStack, Shaper};
    let font = Font::load_test_font();
    let features = vec!["ss03".to_owned()];
    let mut screen = Grid::new(1, 6);
    let mut parser = Parser::new();
    parser.advance(&mut screen, b"-> ab");
    let primary = Arc::new(font);
    let stack = FontStack::with_primary_features(primary, features.clone());
    let mut idx = GlyphIndex::new(stack, 14.0, nz(512));
    let cell_height = idx.cell_metrics().height;
    let cols = screen.cols();
    let cell_count = usize::from(screen.rows()) * usize::from(cols);
    let mut frame_cells = vec![ShapedCell::None; cell_count];
    let mut shaper = Shaper::new();
    let primary_face = idx.stack().primary().clone();
    let mut col = 0u16;
    let mut run_text = String::new();
    while col < cols {
        let Some(run) = next_shape_run(screen.screen(), 0, col, &mut run_text, |c, _style| {
            primary_face.has_glyph(c)
        }) else {
            col += 1;
            continue;
        };
        let run_start = run.start_col;
        let run_cell_count = run.cell_count;
        col = run_start + run_cell_count;
        if run_text.is_empty() {
            continue;
        }
        let shaped = shaper.shape_run(&primary_face, 14.0, &features, &run_text);
        apply_shaped_run(
            &mut frame_cells,
            &mut idx,
            cols,
            &RunPlacement {
                row: 0,
                start_col: run_start,
                cell_count: run_cell_count,
                style: FontStyle::REGULAR,
            },
            &shaped,
        );
    }
    // Unsubstituted gids via a features-off shape of each char alone:
    // the raw charmap is not public outside felis-shaping.
    let plain_gid = |shaper: &mut Shaper, c: &str| {
        let out = shaper.shape_run(&primary_face, 14.0, &[], c);
        assert_eq!(out.len(), 1);
        out[0].glyph_id
    };
    let plain_hyphen = plain_gid(&mut shaper, "-");
    let plain_greater = plain_gid(&mut shaper, ">");
    match frame_cells[1] {
        ShapedCell::Primary {
            glyph_id,
            span_cols,
        } => {
            assert_ne!(glyph_id, 0, "arrow body glyph id must be non-zero");
            assert_ne!(
                glyph_id, plain_greater,
                "cell 1 must carry the substituted arrow piece, not `>`",
            );
            assert_eq!(span_cols, 1, "piece glyphs claim one cell each");
            assert!(idx.contains_glyph_id(glyph_id, cell_height, SizingKey::default()));
        }
        other => panic!("expected the arrow body Primary at (0,1), got {other:?}"),
    }
    match frame_cells[0] {
        ShapedCell::Primary { glyph_id, .. } => {
            assert_ne!(
                glyph_id, plain_hyphen,
                "cell 0 must carry the blank lead piece, not `-`",
            );
        }
        ShapedCell::Trailing => {}
        ShapedCell::None | ShapedCell::Cluster { .. } => {
            panic!("cell 0 must be consumed by the shaped ligature cluster")
        }
    }
}

/// The memo returns byte-identical output to a direct
/// `Shaper::shape_run` and reports hit/miss like `ensure`.
#[cfg(not(windows))]
#[test]
fn shape_run_cached_matches_direct_output_and_hits_on_repeat() {
    let stack = stack();
    let primary = stack.primary().clone();
    let features = vec!["calt".to_owned()];
    let mut idx = GlyphIndex::new(stack, 14.0, nz(256));
    let mut shaper = Shaper::new();
    let direct = shaper.shape_run(&primary, 14.0, &features, "Hello");
    let mut out = Vec::new();
    let first = idx.shape_run_cached(
        &mut shaper,
        &features,
        FontStyle::REGULAR,
        "Hello",
        &mut out,
    );
    assert!(first, "first call must shape (miss)");
    assert_eq!(out, direct, "memoized output must match a direct shape");
    let second = idx.shape_run_cached(
        &mut shaper,
        &features,
        FontStyle::REGULAR,
        "Hello",
        &mut out,
    );
    assert!(!second, "second call must be a cache hit");
    assert_eq!(out, direct, "hit must replay the identical output");
}

/// Scrollback produces unbounded unique row texts, so the memo must
/// stay bounded.
#[cfg(not(windows))]
#[test]
fn shape_run_cache_stays_bounded_under_unique_text_pressure() {
    let stack = stack();
    let mut idx = GlyphIndex::new(stack, 14.0, nz(256));
    let mut shaper = Shaper::new();
    let mut out = Vec::new();
    let cap = GlyphIndex::SHAPED_RUN_CACHE_MAX;
    for i in 0..(cap + 16) {
        let text = format!("line {i}");
        idx.shape_run_cached(&mut shaper, &[], FontStyle::REGULAR, &text, &mut out);
    }
    assert!(
        idx.shaped_run_cache_len() <= cap,
        "cache len {} must not exceed cap {cap}",
        idx.shaped_run_cache_len(),
    );
    let kept = format!("line {}", cap + 15);
    assert!(
        !idx.shape_run_cached(&mut shaper, &[], FontStyle::REGULAR, &kept, &mut out),
        "most-recent insert must survive the eviction sweep",
    );
}

/// A color change mid-row must not end a run (kitty ligates across
/// syntax-highlight boundaries); per-cell color is applied at
/// emission by `push_sliced_by_cell`.
#[cfg(not(windows))]
#[test]
fn shape_run_continues_across_attribute_boundary() {
    use felis_grid::{Attributes, Cell, Color};
    let mut screen = Grid::new(1, 4);
    let default_style = screen.style_table_mut().intern(Attributes::default());
    screen.set_cell(
        0,
        0,
        Cell {
            grapheme: Grapheme::Ascii(b'='),
            style: default_style,
            link: None,
            sizing: None,
        },
    );
    let differ = screen.style_table_mut().intern(Attributes {
        fg: Color::Indexed(1),
        ..Attributes::default()
    });
    screen.set_cell(
        0,
        1,
        Cell {
            grapheme: Grapheme::Ascii(b'>'),
            style: differ,
            link: None,
            sizing: None,
        },
    );
    let stack = stack();
    let primary = stack.primary().clone();
    let mut text = String::new();
    let run0 = next_shape_run(screen.screen(), 0, 0, &mut text, |c, _style| {
        primary.has_glyph(c)
    })
    .expect("first cell must start a run");
    assert_eq!(run0.cell_count, 2, "color boundary must not end the run");
    assert_eq!(text, "=>");
}

/// Unlike color, a bold/italic change ends the run.
#[cfg(not(windows))]
#[test]
fn shape_run_breaks_at_bold_italic_boundary() {
    use felis_grid::{AttrFlags, Attributes, Cell};
    let mut screen = Grid::new(1, 4);
    let default_style = screen.style_table_mut().intern(Attributes::default());
    screen.set_cell(
        0,
        0,
        Cell {
            grapheme: Grapheme::Ascii(b'='),
            style: default_style,
            link: None,
            sizing: None,
        },
    );
    let bold = screen.style_table_mut().intern(Attributes {
        flags: AttrFlags::BOLD,
        ..Attributes::default()
    });
    screen.set_cell(
        0,
        1,
        Cell {
            grapheme: Grapheme::Ascii(b'>'),
            style: bold,
            link: None,
            sizing: None,
        },
    );
    let stack = stack();
    let primary = stack.primary().clone();
    let mut text = String::new();
    let run0 = next_shape_run(screen.screen(), 0, 0, &mut text, |c, _style| {
        primary.has_glyph(c)
    })
    .expect("first cell must start a run");
    assert_eq!(run0.cell_count, 1, "bold boundary must end the run");
    assert_eq!(text, "=");
    assert_eq!(run0.style, FontStyle::REGULAR);
    let run1 = next_shape_run(screen.screen(), 0, 1, &mut text, |c, _style| {
        primary.has_glyph(c)
    })
    .expect("second cell starts a fresh run");
    assert_eq!(text, ">");
    assert_eq!(
        run1.style,
        FontStyle {
            bold: true,
            italic: false
        },
    );
}

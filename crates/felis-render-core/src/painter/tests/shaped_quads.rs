use super::*;

/// The slice arithmetic accumulates one f32 rounding step.
#[track_caller]
fn assert_px_close(got: f32, want: f32) {
    assert!(
        (got - want).abs() < 1e-5,
        "source px mismatch: got {got}, want {want}"
    );
}

/// 24 px of ink spanning three 8 px cells.
fn ligature_slot(left: i32) -> GlyphSlot {
    GlyphSlot {
        atlas_px: [0, 0],
        size_px: [24, 10],
        offset_px: [left, 10],
        is_color: false,
        fills_cell: false,
    }
}

/// Cursor hidden so the Block-cursor fg swap cannot leak into the
/// color assertions.
fn build_shaped(screen: &ScreenBuffer, atlas: &MockAtlas, frame: &ShapeFrame) -> RowPaint {
    let mut buffers = RowPaint::default();
    CellPainter::new(
        screen,
        metrics(),
        &ResolvedTheme::new(&Theme::default()),
        atlas,
        frame,
    )
    .without_compaction()
    .with_cursor_visible(false)
    .with_viewport(0, u32::from(screen.rows()))
    .extend_screen(&mut buffers);
    buffers
}

/// A shaped ligature quad covering differently-colored cells splits
/// into per-cell strips (kitty's behavior; fish recolors the `>` of
/// `<->`).
#[test]
fn shaped_quad_slices_per_cell_when_colors_differ() {
    let g = drive(1, 3, b"<-\x1b[31m>");
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(42, ligature_slot(0));
    let frame = ShapeFrame::from_cells(
        vec![
            ShapedCell::Primary {
                glyph_id: 42,
                span_cols: 3,
            },
            ShapedCell::Trailing,
            ShapedCell::Trailing,
        ],
        3,
    );
    let buffers = build_shaped(g.screen(), &atlas, &frame);
    assert_eq!(buffers.glyphs.len(), 3, "one strip per covered cell");
    for (i, inst) in buffers.glyphs.iter().enumerate() {
        let i_f = i as f32;
        assert_eq!(inst.origin_px, [8.0 * i_f, 2.0]);
        assert_eq!(inst.size_px, [8.0, 10.0]);
        assert_px_close(inst.src_min_px[0], 8.0 * i_f);
        assert_px_close(inst.src_max_px[0], 8.0 * (i_f + 1.0));
        assert_eq!(inst.src_min_px[1], 0.0);
        assert_eq!(inst.src_max_px[1], 10.0);
    }
    let red = resolve_fg(Color::Indexed(1), &ResolvedTheme::new(&Theme::default()));
    assert_eq!(buffers.glyphs[0].color, buffers.glyphs[1].color);
    assert_eq!(buffers.glyphs[2].color, red);
    assert_ne!(buffers.glyphs[0].color, red);
}

#[test]
fn shaped_quad_slices_per_cell_when_only_the_background_changes() {
    let g = drive(1, 3, b"<-\x1b[41m>");
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(42, ligature_slot(0));
    let frame = ShapeFrame::from_cells(
        vec![
            ShapedCell::Primary {
                glyph_id: 42,
                span_cols: 3,
            },
            ShapedCell::Trailing,
            ShapedCell::Trailing,
        ],
        3,
    );
    let buffers = build_shaped(g.screen(), &atlas, &frame);
    let theme = ResolvedTheme::new(&Theme::default());
    let red = resolve_fg(Color::Indexed(1), &theme);
    let grounds: Vec<f32> = buffers.glyphs.iter().map(|g| g.ground_luminance).collect();
    let plain = relative_luminance(theme.bg);
    assert_eq!(grounds, [plain, plain, relative_luminance(red)]);
    assert!(buffers.glyphs.iter().all(|g| g.color == theme.fg));
}

/// Uniform colors take the unsliced fast path.
#[test]
fn shaped_quad_stays_unsliced_when_colors_match() {
    let g = drive(1, 3, b"<->");
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(42, ligature_slot(0));
    let frame = ShapeFrame::from_cells(
        vec![
            ShapedCell::Primary {
                glyph_id: 42,
                span_cols: 3,
            },
            ShapedCell::Trailing,
            ShapedCell::Trailing,
        ],
        3,
    );
    let buffers = build_shaped(g.screen(), &atlas, &frame);
    assert_eq!(buffers.glyphs.len(), 1, "uniform colors must not slice");
    let inst = &buffers.glyphs[0];
    assert_eq!(inst.origin_px, [0.0, 2.0]);
    assert_eq!(inst.size_px, [24.0, 10.0]);
    assert_eq!(inst.src_min_px, [0.0, 0.0]);
    assert_eq!(inst.src_max_px, [24.0, 10.0]);
}

/// Monaspace-style pieces reach back across earlier cells via
/// negative left bearing; the slices carry those cells' own colors.
#[test]
fn shaped_quad_with_negative_bearing_slices_back_across_cells() {
    let g = drive(1, 3, b"\x1b[31m<\x1b[39m->");
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(43, ligature_slot(-16));
    let frame = ShapeFrame::from_cells(
        vec![
            // Blank lead pieces have no atlas slot, as in the real
            // pipeline.
            ShapedCell::Primary {
                glyph_id: 41,
                span_cols: 1,
            },
            ShapedCell::Primary {
                glyph_id: 41,
                span_cols: 1,
            },
            ShapedCell::Primary {
                glyph_id: 43,
                span_cols: 1,
            },
        ],
        3,
    );
    let buffers = build_shaped(g.screen(), &atlas, &frame);
    assert_eq!(buffers.glyphs.len(), 3, "one strip per reached-back cell");
    let red = resolve_fg(Color::Indexed(1), &ResolvedTheme::new(&Theme::default()));
    assert_eq!(buffers.glyphs[0].origin_px, [0.0, 2.0]);
    assert_eq!(
        buffers.glyphs[0].color, red,
        "reached-back cell keeps its own red"
    );
    assert_eq!(
        buffers.glyphs[1].color, buffers.glyphs[2].color,
        "default-fg cells match"
    );
    assert_ne!(buffers.glyphs[1].color, red);
}

/// A combining mark (advance 0, reaching-back GPOS x, positive GPOS
/// y) lands over its base. Hand-rolled `ClusterGlyph`s so the
/// placement cannot drift with a real font's GPOS.
#[test]
fn cluster_cell_emits_one_instance_per_glyph_stacked_by_gpos() {
    let g = drive(1, 4, b"x");
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(100, slot());
    atlas.gid_map.insert(200, slot());
    let frame = ShapeFrame::from_cluster_cells(
        vec![
            ShapedCell::Cluster {
                start: 0,
                len: 2,
                fit_px: 0,
            },
            ShapedCell::None,
            ShapedCell::None,
            ShapedCell::None,
        ],
        4,
        vec![
            ClusterGlyph {
                glyph_id: 100,
                font_id: 0,
                advance_px: 8.0,
                x_offset_px: 0.0,
                y_offset_px: 0.0,
            },
            ClusterGlyph {
                glyph_id: 200,
                font_id: 0,
                advance_px: 0.0,
                x_offset_px: -8.0,
                y_offset_px: 4.0,
            },
        ],
    );
    let buffers = build_shaped(g.screen(), &atlas, &frame);
    assert_eq!(buffers.glyphs.len(), 2, "one instance per cluster glyph");
    assert_eq!(buffers.glyphs[0].origin_px, [1.0, 2.0]);
    assert_eq!(buffers.glyphs[1].origin_px, [1.0, -2.0]);
    assert_eq!(
        buffers.glyphs[0].origin_px[0], buffers.glyphs[1].origin_px[0],
        "mark shares the base's x column",
    );
    assert!(
        buffers.glyphs[1].origin_px[1] < buffers.glyphs[0].origin_px[1],
        "mark sits above its base (smaller screen y)",
    );
}

/// A sized (OSC 66 `s=2`) composited cluster scales pen advance and
/// GPOS by the glyph scale and lifts the baseline to `ascent × scale`.
#[test]
fn sized_cluster_scales_pen_and_gpos_by_glyph_scale() {
    let g = drive(2, 4, b"\x1b]66;s=2;X\x07");
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(100, slot());
    atlas.gid_map.insert(200, slot());
    let glyphs = vec![
        ClusterGlyph {
            glyph_id: 100,
            font_id: 0,
            advance_px: 8.0,
            x_offset_px: 0.0,
            y_offset_px: 0.0,
        },
        ClusterGlyph {
            glyph_id: 200,
            font_id: 0,
            advance_px: 0.0,
            x_offset_px: -8.0,
            y_offset_px: 4.0,
        },
    ];
    let mut cells = vec![ShapedCell::None; 8];
    cells[0] = ShapedCell::Cluster {
        start: 0,
        len: 2,
        fit_px: 0,
    };
    let frame = ShapeFrame::from_cluster_cells(cells, 4, glyphs);
    let buffers = build_shaped(g.screen(), &atlas, &frame);
    assert_eq!(buffers.glyphs.len(), 2, "one instance per cluster glyph");
    assert_eq!(buffers.glyphs[0].origin_px, [1.0, 14.0]);
    assert_eq!(buffers.glyphs[1].origin_px, [1.0, 6.0]);
    let rise = buffers.glyphs[0].origin_px[1] - buffers.glyphs[1].origin_px[1];
    assert!(
        (rise - 8.0).abs() < f32::EPSILON,
        "mark rises 2× the unscaled 4 px"
    );
    assert_eq!(
        buffers.glyphs[0].origin_px[0], buffers.glyphs[1].origin_px[0],
        "mark still shares the base's x column at scale",
    );
}

/// Non-default alignment aligns the cluster's union ink box, not the
/// base glyph's.
#[test]
fn sized_cluster_alignment_uses_the_union_ink_box() {
    let g = drive(1, 4, b"\x1b]66;w=2:h=1:v=1;X\x07");
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(100, slot());
    atlas.gid_map.insert(
        200,
        GlyphSlot {
            atlas_px: [0, 0],
            size_px: [10, 12],
            offset_px: [0, 11],
            is_color: false,
            fills_cell: false,
        },
    );
    let glyphs = vec![
        ClusterGlyph {
            glyph_id: 100,
            font_id: 0,
            advance_px: 8.0,
            x_offset_px: 0.0,
            y_offset_px: 0.0,
        },
        ClusterGlyph {
            glyph_id: 200,
            font_id: 0,
            advance_px: 0.0,
            x_offset_px: -8.0,
            y_offset_px: 0.0,
        },
    ];
    let mut cells = vec![ShapedCell::None; 4];
    cells[0] = ShapedCell::Cluster {
        start: 0,
        len: 2,
        fit_px: 0,
    };
    let frame = ShapeFrame::from_cluster_cells(cells, 4, glyphs);
    let buffers = build_shaped(g.screen(), &atlas, &frame);
    assert_eq!(buffers.glyphs.len(), 2, "one instance per cluster glyph");
    // Union ink box [0,1]..[10,13]: Right shifts 16 − 10 = 6, Bottom
    // 16 − 13 = 3 (base-referenced alignment would shift 9 and
    // overflow by 3 px).
    assert_eq!(
        buffers.glyphs[0].origin_px,
        [7.0, 5.0],
        "base rides the shift"
    );
    assert_eq!(
        buffers.glyphs[1].origin_px,
        [6.0, 4.0],
        "the mark's ink ends flush with the block's right/bottom edges",
    );
}

/// The block is the grid's span, not the base char's width: VS16
/// widens a width-1 heart to two cells unless the last column or an
/// OSC 66 block placed before it refuses it, and OSC 66 `w`
/// overrides both; `s` scales the block after.
#[test]
fn a_cluster_block_spans_the_cells_the_grid_gives_it() {
    let cols = |bytes: &[u8], col| {
        let g = drive(2, 8, bytes);
        assert!(!matches!(
            g.screen().cell(0, col).map(|c| c.grapheme),
            Some(Grapheme::Empty)
        ));
        cluster_block_cols(g.screen(), 0, col)
    };
    assert_eq!(cols("\u{2764}\u{FE0F}".as_bytes(), 0), 2, "widened");
    assert_eq!(cols("\x1b[8G\u{2764}\u{FE0F}".as_bytes(), 7), 1, "refused");
    assert_eq!(
        cols("\x1b]66;w=3;e\u{301}\x07".as_bytes(), 0),
        3,
        "OSC 66 w"
    );
    assert_eq!(
        cols("\x1b]66;s=2;\u{2764}\u{FE0F}\x07".as_bytes(), 0),
        2,
        "sized, VS16 in the run"
    );
    assert_eq!(
        cols("\x1b]66;s=2;\u{2764}\x07\u{FE0F}".as_bytes(), 0),
        1,
        "sized, VS16 printed after"
    );
    assert_eq!(
        cols("\x1b]66;s=2;\u{5B57}\u{301}\x07".as_bytes(), 0),
        2,
        "sized, wide base"
    );
}

/// A centred cluster centres in the two cells a widened VS16 heart
/// takes, where its width-1 base would centre it in one.
#[test]
fn a_centred_cluster_centres_in_its_grid_span() {
    let g = drive(1, 4, "\x1b]66;h=2;\u{2764}\u{FE0F}\x07".as_bytes());
    assert_eq!(g.screen().char_span(0, 0), (0, 1), "the grid widened it");
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(100, slot());
    let glyph = ClusterGlyph {
        glyph_id: 100,
        font_id: 0,
        advance_px: 8.0,
        x_offset_px: 0.0,
        y_offset_px: 0.0,
    };
    let mut cells = vec![ShapedCell::None; 4];
    cells[0] = ShapedCell::Cluster {
        start: 0,
        len: 1,
        fit_px: 0,
    };
    let frame = ShapeFrame::from_cluster_cells(cells, 4, vec![glyph]);
    let buffers = build_shaped(g.screen(), &atlas, &frame);
    // Ink 6 px wide from x = 1: (16 − 6) / 2 − 1 = 4, plus the bearing.
    assert_eq!(buffers.glyphs[0].origin_px[0], 5.0);
}

/// A fitted cluster reads the slots rasterized at its fitted size,
/// never the native ones, and Top/Left centres its ink in the block
/// as a fitted char's bitmap is centred.
#[test]
fn a_fitted_cluster_draws_its_fitted_slots_centred() {
    let g = drive(1, 4, b"x");
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(100, slot());
    atlas.fitted_map.insert(
        (100, 9),
        GlyphSlot {
            size_px: [4, 6],
            offset_px: [3, 9],
            ..slot()
        },
    );
    let glyph = ClusterGlyph {
        glyph_id: 100,
        font_id: 0,
        advance_px: 8.0,
        x_offset_px: 0.0,
        y_offset_px: 0.0,
    };
    let mut cells = vec![ShapedCell::None; 4];
    cells[0] = ShapedCell::Cluster {
        start: 0,
        len: 1,
        fit_px: 9,
    };
    let frame = ShapeFrame::from_cluster_cells(cells, 4, vec![glyph]);
    let buffers = build_shaped(g.screen(), &atlas, &frame);
    assert_eq!(buffers.glyphs.len(), 1);
    assert_eq!(buffers.glyphs[0].size_px, [4.0, 6.0], "the fitted slot");
    // An 8×16 cell centres a 4×6 ink at (2, 5).
    assert_eq!(buffers.glyphs[0].origin_px, [2.0, 5.0]);
}

/// A single-glyph cluster places identically to the equivalent
/// `Char` cell.
#[test]
fn single_glyph_cluster_matches_char_placement() {
    let char_grid = drive(1, 4, b"A");
    let atlas = MockAtlas::full(slot());
    let char_buffers = build_instances(char_grid.screen(), metrics(), &Theme::default(), &atlas);
    let char_inst = char_buffers.glyphs[0];
    let g = drive(1, 4, b"x");
    let mut cluster_atlas = MockAtlas::full(slot());
    cluster_atlas.gid_map.insert(100, slot());
    let frame = ShapeFrame::from_cluster_cells(
        vec![
            ShapedCell::Cluster {
                start: 0,
                len: 1,
                fit_px: 0,
            },
            ShapedCell::None,
            ShapedCell::None,
            ShapedCell::None,
        ],
        4,
        vec![ClusterGlyph {
            glyph_id: 100,
            font_id: 0,
            advance_px: 8.0,
            x_offset_px: 0.0,
            y_offset_px: 0.0,
        }],
    );
    let buffers = build_shaped(g.screen(), &cluster_atlas, &frame);
    assert_eq!(
        buffers.glyphs.len(),
        1,
        "single-glyph cluster emits one instance"
    );
    assert_eq!(buffers.glyphs[0].origin_px, char_inst.origin_px);
    assert_eq!(buffers.glyphs[0].size_px, char_inst.size_px);
    assert_eq!(buffers.glyphs[0].src_min_px, char_inst.src_min_px);
    assert_eq!(buffers.glyphs[0].src_max_px, char_inst.src_max_px);
}

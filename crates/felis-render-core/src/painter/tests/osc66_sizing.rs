use super::*;

#[test]
fn osc_66_valign_bottom_drops_bitmap_to_block_bottom() {
    // `v=1` (bottom) puts the 10 px bitmap's top at
    // `block_h - bitmap_h = 16 - 10 = 6`.
    let g = drive(1, 4, b"\x1b]66;v=1;A\x07");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.glyphs.len(), 1);
    assert_eq!(buffers.glyphs[0].origin_px, [1.0, 6.0]);
}

#[test]
fn osc_66_halign_right_with_w_override_pushes_bitmap_to_block_right_edge() {
    // `w=3, h=1`: block 24 px wide, bitmap 6 px, so its left edge sits
    // at 18; v=Top keeps the baseline placement at 2.
    let g = drive(1, 4, b"\x1b]66;w=3:h=1;A\x07");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.glyphs.len(), 1);
    assert_eq!(buffers.glyphs[0].origin_px, [18.0, 2.0]);
}

#[test]
fn osc_66_center_alignment_centers_the_bitmap_in_the_block() {
    // `w=3, h=2, v=2`: centering puts the 6×10 bitmap at
    // `((24 - 6) / 2, (16 - 10) / 2) = (9, 3)`.
    let g = drive(1, 4, b"\x1b]66;w=3:h=2:v=2;A\x07");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.glyphs.len(), 1);
    assert_eq!(buffers.glyphs[0].origin_px, [9.0, 3.0]);
}

#[test]
fn osc_66_fractional_scale_shrinks_the_glyph_without_shrinking_the_block() {
    let atlas = MockAtlas::full(slot());
    // `s=2` alone: the glyph rasterizes at 2×, so the v=Top baseline
    // sits at `ascent × 2 - offset_px[1] = 24 - 10 = 14`.
    let integer = drive(2, 4, b"\x1b]66;s=2;A\x07");
    let integer = build_instances(integer.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(integer.glyphs.len(), 1);
    assert_eq!(integer.glyphs[0].origin_px, [1.0, 14.0]);

    // `s=2:n=1:d=2`: effective glyph scale `s × n/d = 1`, so the
    // baseline drops back to 2.
    let fractional = drive(2, 4, b"\x1b]66;s=2:n=1:d=2;A\x07");
    let fractional = build_instances(fractional.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(fractional.glyphs.len(), 1);
    assert_eq!(fractional.glyphs[0].origin_px, [1.0, 2.0]);

    // The block still measures `s` cells: v=1 anchors at
    // `ch × s - bitmap_h = 32 - 10 = 22`.
    let bottom = drive(2, 4, b"\x1b]66;s=2:n=1:d=2:v=1;A\x07");
    let bottom = build_instances(bottom.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(bottom.glyphs.len(), 1);
    assert_eq!(bottom.glyphs[0].origin_px, [1.0, 22.0]);
}

/// The 8×16 cell grown to 12×24: 2 px of pad left and right, 4 above
/// and below.
fn grown_metrics() -> CellMetrics {
    metrics().with_cell_scale([1.5, 1.5])
}

fn lone_glyph_origin(rows: u16, bytes: &[u8], m: CellMetrics, atlas: &MockAtlas) -> [f32; 2] {
    let g = drive(rows, 4, bytes);
    let buffers = build_instances(g.screen(), m, &Theme::default(), atlas);
    assert_eq!(buffers.glyphs.len(), 1);
    buffers.glyphs[0].origin_px
}

#[test]
fn osc_66_right_and_bottom_alignment_stay_inside_the_grown_cell_pad() {
    // `w=3, h=1, v=1`: block 36×24, so the 6×10 bitmap sits at
    // `(36 - 6 - 2, 24 - 10 - 4) = (28, 10)`.
    let atlas = MockAtlas::full(slot());
    let origin = lone_glyph_origin(1, b"\x1b]66;w=3:h=1:v=1;A\x07", grown_metrics(), &atlas);
    assert_eq!(origin, [28.0, 10.0]);
}

#[test]
fn osc_66_alignment_ignores_the_negative_pad_of_a_shrunk_cell() {
    // The 8×16 cell shrunk to 4×8: `w=3, h=1, v=1` aligns to the
    // block's own edges at `(12 - 6, 8 - 10) = (6, -2)`.
    let atlas = MockAtlas::full(slot());
    let shrunk = metrics().with_cell_scale([0.5, 0.5]);
    let origin = lone_glyph_origin(1, b"\x1b]66;w=3:h=1:v=1;A\x07", shrunk, &atlas);
    assert_eq!(origin, [6.0, -2.0]);
}

#[test]
fn a_wide_char_takes_the_pad_of_every_cell_it_spans_but_the_first() {
    // The slot's own offset (1) holds the first cell's pad; the
    // second cell adds its 2 px so the glyph box stays centered
    // across both.
    let mut atlas = MockAtlas::full(slot());
    atlas.map.insert('中', slot());
    let origin = lone_glyph_origin(1, "中".as_bytes(), grown_metrics(), &atlas);
    assert_eq!(origin[0], 3.0);
}

#[test]
fn a_cell_filling_glyph_takes_no_pad_whatever_its_alignment() {
    // `w=2, v=1` on a 12×24 cell: a box glyph sized to one scaled
    // cell aligns right and bottom to the block's own edges.
    let mut atlas = MockAtlas::full(slot());
    atlas.map.insert(
        '█',
        GlyphSlot {
            atlas_px: [0, 0],
            size_px: [12, 24],
            offset_px: [0, 16],
            is_color: false,
            fills_cell: true,
        },
    );
    let bytes = "\x1b]66;w=2:h=1:v=1;█\x07".as_bytes();
    assert_eq!(
        lone_glyph_origin(1, bytes, grown_metrics(), &atlas),
        [12.0, 0.0]
    );
}

#[test]
fn a_fractional_osc_66_scale_puts_the_top_pad_at_the_layout_scale() {
    // `s=2:n=1:d=2` on the 12×24 cell: the font's 12 px ascent at glyph
    // scale 1, below a 4 px pad at layout scale 2, so the baseline is
    // at 20 and the bitmap top at 20 - 10.
    let atlas = MockAtlas::full(slot());
    let origin = lone_glyph_origin(2, b"\x1b]66;s=2:n=1:d=2;A\x07", grown_metrics(), &atlas);
    assert_eq!(origin, [1.0, 10.0]);
}

#[test]
fn decorations_follow_the_glyph_box_down_by_the_top_pad() {
    let tall = metrics().with_cell_scale([1.0, 3.0]);
    let top = |sgr: &[u8], m: CellMetrics| {
        let mut bytes = sgr.to_vec();
        bytes.push(b'A');
        let g = drive(1, 4, &bytes);
        let atlas = MockAtlas::full(slot());
        build_instances(g.screen(), m, &Theme::default(), &atlas).decos[0].origin_px[1]
    };
    let pad = tall.pad[1] as f32;
    for sgr in [&b"\x1b[4m"[..], b"\x1b[9m", b"\x1b[53m"] {
        assert_eq!(
            top(sgr, tall),
            top(sgr, metrics()) + pad,
            "{:?}",
            String::from_utf8_lossy(sgr)
        );
    }
}

#[test]
fn a_wide_cluster_and_a_wide_preedit_char_take_the_pad_of_their_second_cell() {
    let g = drive(1, 4, "中".as_bytes());
    let mut atlas = MockAtlas::full(slot());
    atlas.gid_map.insert(7, slot());
    let glyph = ClusterGlyph {
        glyph_id: 7,
        font_id: 0,
        advance_px: 16.0,
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
    let mut buffers = RowPaint::default();
    CellPainter::new(
        g.screen(),
        grown_metrics(),
        &ResolvedTheme::new(&Theme::default()),
        &atlas,
        &frame,
    )
    .without_compaction()
    .with_viewport(0, 1)
    .extend_screen(&mut buffers);
    assert_eq!(buffers.glyphs[0].origin_px[0], 3.0, "cluster");

    let mut out = RowPaint::default();
    let mut atlas = MockAtlas::full(slot());
    atlas.map.insert('あ', slot());
    let overlay = PreeditOverlay {
        anchor: (0, 0),
        text: "あ".to_owned(),
        cursor: None,
    };
    extend_preedit(&overlay, grown_metrics(), 5, 80, &atlas, &mut out);
    assert_eq!(out.glyphs[0].origin_px[0], 3.0, "pre-edit");
}

#[test]
fn osc_66_sized_spacer_cells_emit_bg_but_not_fg() {
    // REQ-603: only the primary feeds the fg pass; the three
    // SizedSpacers still emit bg quads.
    let g = drive(2, 4, b"\x1b]66;s=2;X\x07");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.rects.len(), 8);
    assert_eq!(buffers.glyphs.len(), 1);
    let inst = &buffers.glyphs[0];
    assert_eq!(inst.origin_px, [1.0, 14.0]);
    assert_eq!(inst.size_px, [6.0, 10.0]);
}

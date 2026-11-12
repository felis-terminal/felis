use super::*;

/// `extend_screen` at viewport=0 is byte-identical to the live
/// path.
#[test]
fn extend_screen_viewport_zero_paints_same_bg_as_live_path() {
    let g = drive(2, 4, b"\x1b[?25lAB\r\nCD");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let mut buffers = RowPaint::default();
    CellPainter::new(
        g.screen(),
        metrics(),
        &ResolvedTheme::new(&theme),
        &atlas,
        &ShapeFrame::empty(),
    )
    .without_compaction()
    .with_cursor_visible(true)
    .with_viewport(0, u32::from(g.rows()))
    .extend_screen(&mut buffers);
    assert_eq!(
        buffers.rects.len(),
        8,
        "viewport=0 must paint cells only — no scrollbar lane",
    );
    for inst in &buffers.rects {
        assert_ne!(inst.color, SCROLLBAR_TRACK);
        assert_ne!(inst.color, SCROLLBAR_THUMB);
    }
}

/// While `viewport > 0` a track and thumb paint along the right
/// edge, track behind thumb.
#[test]
fn extend_screen_paints_scrollbar_lane_while_browsing() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    p.advance(&mut g, b"\x1b[?25l");
    p.advance(&mut g, b"row1\r\nrow2\r\nrow3");
    assert!(!g.scrollback().is_empty());
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let mut buffers = RowPaint::default();
    CellPainter::new(
        g.screen(),
        metrics(),
        &ResolvedTheme::new(&theme),
        &atlas,
        &ShapeFrame::empty(),
    )
    .without_compaction()
    .with_cursor_visible(true)
    .with_viewport(1, 3)
    .extend_screen(&mut buffers);
    assert_eq!(
        buffers.rects.len(),
        8 + 2,
        "browse mode must add a track and a thumb to the cell quads",
    );
    let last_two = &buffers.rects[buffers.rects.len() - 2..];
    assert_eq!(last_two[0].color, SCROLLBAR_TRACK, "track painted first");
    assert_eq!(last_two[1].color, SCROLLBAR_THUMB, "thumb wins on top");
    let lane_x = (g.cols() as f32).mul_add(metrics().width as f32, -SCROLLBAR_LANE_PX);
    assert_eq!(last_two[0].origin_px[0], lane_x);
    assert_eq!(last_two[1].origin_px[0], lane_x);
}

/// While browsing, the cursor would smear onto whichever scrollback
/// row landed at its `(row, col)`, so it is hidden.
#[test]
fn extend_screen_hides_cursor_while_viewport_above_zero() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    p.advance(&mut g, b"row1\r\nrow2\r\nrow3");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let mut buffers_live = RowPaint::default();
    let mut buffers_browse = RowPaint::default();
    CellPainter::new(
        g.screen(),
        metrics(),
        &ResolvedTheme::new(&theme),
        &atlas,
        &ShapeFrame::empty(),
    )
    .without_compaction()
    .with_cursor_visible(true)
    .with_viewport(0, 3)
    .extend_screen(&mut buffers_live);
    CellPainter::new(
        g.screen(),
        metrics(),
        &ResolvedTheme::new(&theme),
        &atlas,
        &ShapeFrame::empty(),
    )
    .without_compaction()
    .with_cursor_visible(true)
    .with_viewport(1, 3)
    .extend_screen(&mut buffers_browse);
    let cursor = g.cursor();
    let idx = usize::from(cursor.row) * usize::from(g.cols()) + usize::from(cursor.col);
    assert_eq!(
        buffers_live.rects[idx].color, theme.fg,
        "live cursor paints with the swap (cursor bg = theme fg)",
    );
    assert_ne!(
        buffers_browse.rects[idx].color, theme.fg,
        "cursor must hide in browse mode — no swap on the cursor cell",
    );
}

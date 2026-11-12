use super::*;

/// The per-cell background quads, without the cursor or link markers
/// pushed between them.
fn cell_bgs(buffers: &RowPaint) -> Vec<[f32; 4]> {
    let m = metrics();
    buffers
        .rects
        .iter()
        .filter(|b| b.size_px == [m.width as f32, m.height as f32])
        .map(|b| b.color)
        .collect()
}

fn fg_in_col(buffers: &RowPaint, col: u16) -> Option<&GlyphDraw> {
    let cw = metrics().width as f32;
    let left = f32::from(col) * cw;
    buffers
        .glyphs
        .iter()
        .find(|q| q.origin_px[0] >= left && q.origin_px[0] < left + cw)
}

fn build_with_selection(screen: &ScreenBuffer, range: SelectionRange) -> RowPaint {
    let theme = Theme::default();
    let atlas = MockAtlas::full(slot());
    let mut buffers = RowPaint::default();
    CellPainter::new(
        screen,
        metrics(),
        &ResolvedTheme::new(&theme),
        &atlas,
        &ShapeFrame::empty(),
    )
    .without_compaction()
    .with_cursor_visible(true)
    .with_selection(Some(range))
    .with_viewport(0, u32::from(screen.rows()))
    .extend_screen(&mut buffers);
    buffers
}

#[test]
fn block_cursor_on_a_wide_character_covers_both_halves() {
    let theme = Theme::default();
    let mut atlas = MockAtlas::full(slot());
    atlas.map.insert('字', slot());
    // Addressed onto the lead, then onto the Spacer.
    for to_col in [b"\x1b[2G", b"\x1b[3G"] {
        let g = drive(1, 6, &["a字b".as_bytes(), to_col].concat());
        let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
        let bgs = cell_bgs(&buffers);
        assert_eq!(bgs[0], theme.bg);
        assert_eq!(bgs[1], theme.fg, "lead under the cursor");
        assert_eq!(bgs[2], theme.fg, "Spacer under the cursor");
        assert_eq!(bgs[3], theme.bg);
        let glyph = fg_in_col(&buffers, 1).expect("字 draws");
        assert_eq!(glyph.color, theme.bg, "the glyph takes the swapped fg");
    }
}

#[test]
fn underline_cursor_on_a_wide_character_spans_both_halves() {
    let g = drive(1, 6, "\x1b[4 qa字b\x1b[3G".as_bytes());
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let cw = metrics().width as f32;
    let xs: Vec<f32> = buffers
        .rects
        .iter()
        .filter(|b| b.size_px == [cw, CURSOR_MARKER_PX])
        .map(|b| b.origin_px[0])
        .collect();
    assert_eq!(xs, [cw, 2.0 * cw]);
}

#[test]
fn bar_cursor_on_a_wide_character_sits_at_its_left_edge() {
    let g = drive(1, 6, "\x1b[6 qa字b\x1b[3G".as_bytes());
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let xs: Vec<f32> = buffers
        .rects
        .iter()
        .filter(|b| b.size_px[0] == CURSOR_MARKER_PX)
        .map(|b| b.origin_px[0])
        .collect();
    assert_eq!(xs, [metrics().width as f32]);
}

#[test]
fn a_selection_touching_either_half_highlights_the_whole_character() {
    let g = drive(1, 6, "\x1b[?25la字b".as_bytes());
    let theme = Theme::default();
    let ends_on_lead = build_with_selection(
        g.screen(),
        SelectionRange {
            start: (0, 0),
            end: (0, 1),
            rectangle: false,
        },
    );
    assert_eq!(
        cell_bgs(&ends_on_lead)[..4],
        [SELECTION_BG, SELECTION_BG, SELECTION_BG, theme.bg]
    );
    let starts_on_spacer = build_with_selection(
        g.screen(),
        SelectionRange {
            start: (0, 2),
            end: (0, 3),
            rectangle: false,
        },
    );
    assert_eq!(
        cell_bgs(&starts_on_spacer)[..5],
        [theme.bg, SELECTION_BG, SELECTION_BG, SELECTION_BG, theme.bg]
    );
}

#[test]
fn a_rectangle_selection_widens_per_row_to_the_characters_it_touches() {
    let g = drive(2, 4, "\x1b[?25la字b\r\n字cd".as_bytes());
    let theme = Theme::default();
    let buffers = build_with_selection(
        g.screen(),
        SelectionRange {
            start: (0, 1),
            end: (1, 1),
            rectangle: true,
        },
    );
    let bgs = cell_bgs(&buffers);
    assert_eq!(bgs[..4], [theme.bg, SELECTION_BG, SELECTION_BG, theme.bg]);
    assert_eq!(bgs[4..], [SELECTION_BG, SELECTION_BG, theme.bg, theme.bg]);
}

#[test]
fn a_wide_cluster_carrying_a_bidi_override_is_marked_on_both_halves() {
    let g = drive(1, 6, "\x1b[?25la字\u{202E}b".as_bytes());
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let bgs = cell_bgs(&buffers);
    assert_eq!(bgs[1], BIDI_MARKER_BG);
    assert_eq!(bgs[2], BIDI_MARKER_BG);
    assert_ne!(bgs[3], BIDI_MARKER_BG);
}

#[test]
fn a_preedit_over_the_spacer_hides_the_lead_glyph_but_keeps_its_background() {
    let g = drive(1, 6, "\x1b[?25la\x1b[41m字\x1b[mb".as_bytes());
    let theme = Theme::default();
    let atlas = MockAtlas::full(slot());
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
    .with_preedit(Some(PreeditSpan {
        row: 0,
        col_start: 2,
        col_end: 4,
    }))
    .extend_screen(&mut buffers);
    assert!(fg_in_col(&buffers, 0).is_some(), "a draws");
    assert!(
        fg_in_col(&buffers, 1).is_none(),
        "字 would ink under the overlay"
    );
    let cw = metrics().width as f32;
    assert!(
        buffers
            .rects
            .iter()
            .any(|b| b.origin_px[0] == cw && b.size_px[0] == cw && b.color != theme.bg),
        "the lead keeps its red background"
    );
}

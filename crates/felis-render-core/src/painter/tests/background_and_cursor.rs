use super::*;

/// Pixels the bg tier leaves: `clear`, then every rect in list order
/// without blending, sampled at pixel centers, padding included.
fn raster_rects(rects: &[Rect], clear: [f32; 4], size: [usize; 2]) -> Vec<[f32; 4]> {
    let [w, h] = size;
    let mut px = vec![clear; w * h];
    let span = |origin: f32, extent: f32, limit: usize| {
        let lo = (origin - 0.5).ceil().max(0.0) as usize;
        let hi = ((origin + extent - 0.5).ceil().max(0.0) as usize).min(limit);
        lo..hi
    };
    for r in rects {
        for y in span(r.origin_px[1], r.size_px[1], h) {
            for x in span(r.origin_px[0], r.size_px[0], w) {
                px[y * w + x] = r.color;
            }
        }
    }
    px
}

#[derive(Debug)]
struct Frame {
    bytes: Vec<u8>,
    metrics: CellMetrics,
    theme: Theme,
    reverse: bool,
    cursor_visible: bool,
    selection: Option<SelectionRange>,
    browsing: bool,
    reserved_bottom_rows: u16,
    preedit: Option<(u16, u16, String)>,
}

const ROWS: u16 = 3;

const COLS: u16 = 6;

/// `SGR 41` resolves to the same linear color as this theme's
/// background: a non-default bg that matches the clear value.
fn theme_with_red_ground(translucent: bool) -> Theme {
    let red = ResolvedTheme::new(&Theme::default()).palette[1];
    Theme {
        bg: [red[0], red[1], red[2], if translucent { 0.5 } else { 1.0 }],
        ..Theme::default()
    }
}

fn arb_cell() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        " ",
        "a",
        "\x1b[41m ",
        "\x1b[42mb",
        "\x1b[48;5;200m ",
        "\x1b[48;2;10;200;30md",
        "\x1b[7mc",
        "\x1b[0m ",
        "\x1b]8;;https://example.com\x07l\x1b]8;;\x07",
        "\u{3042}",
    ])
}

fn arb_frame() -> impl Strategy<Value = Frame> {
    (
        prop::collection::vec(arb_cell(), usize::from(ROWS * COLS)),
        (0..ROWS, 0..COLS, 0u8..7),
        prop::sample::select(vec![(1, 1), (1, 2), (2, 1), (2, 2), (3, 5), (8, 16)]),
        (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()),
        proptest::option::of((0..ROWS, 0..COLS, 0..ROWS, 0..COLS, any::<bool>())),
        any::<bool>(),
        (
            0u16..2,
            proptest::option::of((0..ROWS, 0..COLS, "[a\u{3042}]{1,3}")),
        ),
    )
        .prop_map(
            |(
                cells,
                (row, col, style),
                (cw, ch),
                (red, translucent, reverse, visible),
                sel,
                browsing,
                (reserved_bottom_rows, preedit),
            )| {
                let mut bytes = Vec::new();
                for (i, cell) in cells.iter().enumerate() {
                    if i % usize::from(COLS) == 0 {
                        bytes.extend_from_slice(
                            format!("\x1b[0m\x1b[{};1H", i / usize::from(COLS) + 1).as_bytes(),
                        );
                    }
                    bytes.extend_from_slice(b"\x1b[0m");
                    bytes.extend_from_slice(cell.as_bytes());
                }
                bytes.extend_from_slice(
                    format!("\x1b[{style} q\x1b[{};{}H", row + 1, col + 1).as_bytes(),
                );
                let theme = if red {
                    theme_with_red_ground(translucent)
                } else if translucent {
                    Theme {
                        bg: [0.1, 0.2, 0.3, 0.5],
                        ..Theme::default()
                    }
                } else {
                    Theme::default()
                };
                Frame {
                    bytes,
                    metrics: CellMetrics::unscaled(cw, ch, ch * 3 / 4),
                    theme,
                    reverse,
                    cursor_visible: visible,
                    selection: sel.map(|(r0, c0, r1, c1, rectangle)| SelectionRange {
                        start: (r0, c0),
                        end: (r1, c1),
                        rectangle,
                    }),
                    browsing,
                    reserved_bottom_rows,
                    preedit,
                }
            },
        )
}

fn paint_frame(f: &Frame, compact: bool) -> RowPaint {
    let g = drive(ROWS, COLS, &f.bytes);
    let theme = ResolvedTheme::new(&f.theme);
    let atlas = MockAtlas::full(slot());
    let shape = ShapeFrame::empty();
    let preedit = f.preedit.as_ref().map(|(row, col, text)| PreeditOverlay {
        anchor: (*row, *col),
        text: text.clone(),
        cursor: None,
    });
    let span = preedit.as_ref().and_then(|o| preedit_covered_cols(o, COLS));
    let painter = CellPainter::new(g.screen(), f.metrics, &theme, &atlas, &shape)
        .with_reserved_bottom_rows(f.reserved_bottom_rows)
        .with_preedit(span)
        .with_cursor_visible(f.cursor_visible)
        .with_selection(f.selection)
        .with_reverse_video(f.reverse);
    let painter = if f.browsing {
        painter.with_viewport(1, u32::from(ROWS) * 2)
    } else {
        painter.with_viewport(0, u32::from(ROWS))
    };
    let painter = if compact {
        painter
    } else {
        painter.without_compaction()
    };
    let mut out = RowPaint::default();
    painter.extend_screen(&mut out);
    if let Some(overlay) = &preedit {
        extend_preedit(overlay, f.metrics, ROWS, COLS, &atlas, &mut out);
    }
    out
}

proptest::proptest! {
    #[test]
    fn compacted_bg_rects_leave_the_same_pixels_as_one_rect_per_cell(f in arb_frame()) {
        let clear = ResolvedTheme::new(&f.theme).clear_color(f.reverse);
        let size = [
            usize::from(COLS) * f.metrics.width as usize + 3,
            usize::from(ROWS) * f.metrics.height as usize + 3,
        ];
        let compact = paint_frame(&f, true);
        let reference = paint_frame(&f, false);
        proptest::prop_assert_eq!(&compact.glyphs, &reference.glyphs);
        proptest::prop_assert_eq!(&compact.decos, &reference.decos);
        proptest::prop_assert!(
            raster_rects(&compact.rects, clear, size)
                == raster_rects(&reference.rects, clear, size)
        );
    }
}

fn bg_rects(bytes: &[u8], metrics: CellMetrics, theme: &Theme, reverse: bool) -> Vec<Rect> {
    let g = drive(1, 4, bytes);
    let theme = ResolvedTheme::new(theme);
    let atlas = MockAtlas::full(slot());
    let mut out = RowPaint::default();
    CellPainter::new(g.screen(), metrics, &theme, &atlas, &ShapeFrame::empty())
        .with_cursor_visible(true)
        .with_viewport(0, 1)
        .with_reverse_video(reverse)
        .extend_screen(&mut out);
    out.rects
}

#[test]
fn a_row_on_the_default_background_paints_no_bg_quad() {
    let rects = bg_rects(b"\x1b[?25lab", metrics(), &Theme::default(), false);
    assert_eq!(rects, []);
}

#[test]
fn a_run_of_one_background_paints_one_quad() {
    let rects = bg_rects(
        b"\x1b[?25l\x1b[44mabcd",
        metrics(),
        &Theme::default(),
        false,
    );
    assert_eq!(rects.len(), 1);
    assert_eq!(rects[0].origin_px, [0.0, 0.0]);
    assert_eq!(rects[0].size_px, [32.0, 16.0]);
}

#[test]
fn a_block_cursor_on_a_default_row_paints_only_the_cursor_quad() {
    let rects = bg_rects(b"ab\x1b[1;2H", metrics(), &Theme::default(), false);
    assert_eq!(rects.len(), 1);
    assert_eq!(rects[0].origin_px, [8.0, 0.0]);
}

#[test]
fn an_explicit_background_equal_to_the_theme_background_still_paints() {
    let rects = bg_rects(
        b"\x1b[?25l\x1b[41mab",
        metrics(),
        &theme_with_red_ground(false),
        false,
    );
    assert_eq!(rects.len(), 1, "covers an image under the backgrounds");
    assert_eq!(rects[0].size_px, [16.0, 16.0]);
}

#[test]
fn decscnm_paints_every_cell_background() {
    let rects = bg_rects(b"\x1b[?25l", metrics(), &Theme::default(), true);
    assert_eq!(rects.len(), 1, "one merged run of the reversed ground");
    assert_eq!(rects[0].size_px, [32.0, 16.0]);
}

#[test]
fn a_cell_narrower_than_a_cursor_marker_keeps_its_background_quad() {
    let narrow = CellMetrics::unscaled(1, 16, 12);
    let rects = bg_rects(b"\x1b[?25l", narrow, &Theme::default(), false);
    assert_eq!(rects.len(), 1);
    assert_eq!(rects[0].size_px, [4.0, 16.0]);
}

#[test]
fn cursor_rect_spans_both_columns_of_a_wide_character() {
    let metrics = CellMetrics::unscaled(10, 20, 16);
    let span = (3, 4);
    assert_eq!(
        cursor_rect_px(metrics, [5.0, 0.0], 1, span, CursorStyle::Block),
        [35.0, 20.0, 20.0, 20.0]
    );
    assert_eq!(
        cursor_rect_px(metrics, [5.0, 0.0], 1, span, CursorStyle::Underline),
        [35.0, 40.0 - CURSOR_MARKER_PX, 20.0, CURSOR_MARKER_PX]
    );
    assert_eq!(
        cursor_rect_px(metrics, [5.0, 0.0], 1, span, CursorStyle::Bar),
        [35.0, 20.0, CURSOR_MARKER_PX, 20.0]
    );
}

#[test]
fn cursor_cell_renders_as_reverse_video_block() {
    let g = drive(1, 3, b"A");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let theme = Theme::default();
    assert_eq!(g.cursor().row, 0);
    assert_eq!(g.cursor().col, 1);
    assert_eq!(
        buffers.rects[1].color, theme.fg,
        "cursor cell bg must be the theme fg (reverse video)"
    );
    assert_eq!(buffers.rects[0].color, theme.bg);
    assert_eq!(buffers.glyphs[0].color, theme.fg);
}

#[test]
fn hidden_cursor_skips_reverse_video_swap() {
    // `?25l`: the cursor cell renders like any other empty cell.
    let g = drive(1, 3, b"\x1b[?25lA");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let theme = Theme::default();
    assert!(!g.cursor().visible);
    assert_eq!(buffers.rects[1].color, theme.bg);
}

#[test]
fn underline_cursor_appends_a_thin_bar_at_the_bottom_of_the_cell() {
    let g = drive(1, 3, b"\x1b[4 qA");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let theme = Theme::default();
    assert_eq!(g.cursor().col, 1);
    assert_eq!(buffers.rects[0].color, theme.bg, "cell 0 normal");
    assert_eq!(buffers.rects[1].color, theme.bg, "cursor cell bg unchanged");
    let marker = buffers
        .rects
        .iter()
        .find(|b| b.size_px[1] == CURSOR_MARKER_PX)
        .expect("underline marker present");
    assert_eq!(marker.size_px, [metrics().width as f32, CURSOR_MARKER_PX]);
    let expected_y = metrics().height as f32 - CURSOR_MARKER_PX;
    assert_eq!(marker.origin_px[1], expected_y);
}

#[test]
fn bar_cursor_appends_a_thin_vertical_strip_at_the_left_of_the_cell() {
    let g = drive(1, 3, b"\x1b[6 qA");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let marker = buffers
        .rects
        .iter()
        .find(|b| b.size_px[0] == CURSOR_MARKER_PX)
        .expect("bar marker present");
    assert_eq!(marker.size_px, [CURSOR_MARKER_PX, metrics().height as f32]);
    let cell_origin = [metrics().width as f32 * 1.0, 0.0];
    assert_eq!(marker.origin_px, cell_origin);
}

#[test]
fn block_cursor_emits_no_marker_quad() {
    let g = drive(1, 3, b"AB"); // default style stays Block
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.rects.len(), 3);
}

#[test]
fn underline_cursor_uses_theme_cursor_color_when_configured() {
    let theme = Theme {
        cursor: Some([0.5, 0.7, 0.9, 1.0]),
        ..Theme::default()
    };
    let g = drive(1, 3, b"\x1b[4 qA");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
    let marker = buffers
        .rects
        .iter()
        .find(|b| b.size_px[1] == CURSOR_MARKER_PX)
        .expect("underline marker present");
    assert_eq!(marker.color, [0.5, 0.7, 0.9, 1.0]);
}

#[test]
fn bar_cursor_uses_theme_cursor_color_when_configured() {
    let theme = Theme {
        cursor: Some([0.5, 0.7, 0.9, 1.0]),
        ..Theme::default()
    };
    let g = drive(1, 3, b"\x1b[6 qA");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
    let marker = buffers
        .rects
        .iter()
        .find(|b| b.size_px[0] == CURSOR_MARKER_PX)
        .expect("bar marker present");
    assert_eq!(marker.color, [0.5, 0.7, 0.9, 1.0]);
}

#[test]
fn unfocused_window_suppresses_the_cursor_block() {
    let g = drive(1, 3, b"AB");
    let atlas = MockAtlas::full(slot());
    let theme = Theme::default();
    let buffers = build_instances_with(g.screen(), metrics(), &theme, &atlas, |painter| {
        painter.with_cursor_visible(false)
    });
    assert_eq!(buffers.rects.len(), 3);
    for inst in &buffers.rects {
        assert_eq!(inst.color, theme.bg, "no cell painted with cursor swap");
    }
}

#[test]
fn unfocused_window_suppresses_underline_marker_too() {
    let g = drive(1, 3, b"\x1b[4 qA");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances_with(
        g.screen(),
        metrics(),
        &Theme::default(),
        &atlas,
        |painter| painter.with_cursor_visible(false),
    );
    let marker = buffers
        .rects
        .iter()
        .find(|b| b.size_px[1] == CURSOR_MARKER_PX);
    assert!(
        marker.is_none(),
        "underline marker must drop when unfocused"
    );
}

#[test]
fn unfocused_window_suppresses_bar_marker_too() {
    let g = drive(1, 3, b"\x1b[6 qA");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances_with(
        g.screen(),
        metrics(),
        &Theme::default(),
        &atlas,
        |painter| painter.with_cursor_visible(false),
    );
    let marker = buffers
        .rects
        .iter()
        .find(|b| b.size_px[0] == CURSOR_MARKER_PX);
    assert!(marker.is_none(), "bar marker must drop when unfocused");
}

#[test]
fn cursor_uses_theme_cursor_color_when_configured() {
    let theme = Theme {
        cursor: Some([0.5, 0.7, 0.9, 1.0]),
        ..Theme::default()
    };
    let g = drive(1, 3, b"AB");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &theme, &atlas);
    assert_eq!(g.cursor().col, 2);
    assert_eq!(buffers.rects[2].color, [0.5, 0.7, 0.9, 1.0]);
}

#[test]
fn cursor_glyph_renders_in_reversed_fg_color() {
    let g = drive(1, 3, b"AB\x1b[1;1H");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let theme = Theme::default();
    assert_eq!(g.cursor().row, 0);
    assert_eq!(g.cursor().col, 0);
    assert_eq!(buffers.rects[0].color, theme.fg);
    assert_eq!(
        buffers.glyphs[0].color, theme.bg,
        "cursor cell glyph must paint in reversed fg color"
    );
}

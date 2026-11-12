use super::*;

fn visible_snapshot(g: &Grid) -> (Vec<Vec<Grapheme>>, Vec<bool>) {
    let mut cells = Vec::new();
    let mut wraps = Vec::new();
    for r in 0..g.rows() {
        let row: Vec<Grapheme> = (0..g.cols())
            .map(|c| g.cell(r, c).unwrap().grapheme)
            .collect();
        cells.push(row);
        wraps.push(g.row_soft_wrap_continued(r));
    }
    (cells, wraps)
}

/// REQ-604 round-trip below the scrollback cap.
#[test]
fn reflow_soft_wrap_narrow_then_widen_round_trips() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 10);
    drive(&mut p, &mut g, b"abcdefghijklmnopqr");
    let before = visible_snapshot(&g);
    assert!(g.row_soft_wrap_continued(1));

    g.reflow(6, 4);
    g.reflow(6, 10);

    let after = visible_snapshot(&g);
    assert_eq!(before, after, "cells + soft-wrap bits must survive W→W'→W");
}

/// The remap points at the row where the same cell landed, not the
/// start of its logical line (REQ-604).
#[test]
fn reflow_remap_translates_live_anchor_across_rewrap() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 10);
    drive(&mut p, &mut g, b"abcdefghijklmnopqr");

    let remap = g
        .reflow(6, 4)
        .expect("primary-screen reflow returns a remap");

    assert_eq!(remap.remap_row(2), Some(3));
    assert_eq!(remap.remap_row(1), Some(1));
}

/// Anchors pushed past the live window remap into scrollback
/// coordinates (row ≤ 0), as `shift_up` keeps scrolled-out placements
/// addressable.
#[test]
fn reflow_remap_moves_anchor_into_scrollback() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    // Wrapped line (rows 1-2) plus two hard lines fill the screen.
    drive(&mut p, &mut g, b"abcdefghijklmnopqr\r\nsecond\r\nthird");

    let remap = g
        .reflow(4, 4)
        .expect("primary-screen reflow returns a remap");

    // 9 physical rows now exist; the last 4 are live, 5 spill into
    // scrollback. Row 1 ("abcd…") becomes the oldest retained line.
    assert_eq!(remap.remap_row(1), Some(-4));
    assert_eq!(remap.remap_row(2), Some(-2));
    // "third" (old row 4) opens at new physical row 7 → live row 3.
    assert_eq!(remap.remap_row(4), Some(3));
}

/// Blank rows below the content are screen padding the reflow
/// regenerates, so an anchor there keeps its distance rather than
/// reading as evicted.
#[test]
fn reflow_remap_preserves_padding_distance_below_content() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 10);
    drive(&mut p, &mut g, b"hello");

    let remap = g
        .reflow(6, 4)
        .expect("primary-screen reflow returns a remap");

    assert_eq!(remap.remap_row(3), Some(4));
}

#[test]
fn reflow_keeps_hard_wrapped_lines_separate() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 8);
    drive(&mut p, &mut g, b"aaaa\r\nbbbb");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(!g.row_soft_wrap_continued(1));

    g.reflow(6, 3);

    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert!(g.row_soft_wrap_continued(1), "wrap of the first line");
    assert_eq!(g.cell(2, 0).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert!(
        !g.row_soft_wrap_continued(2),
        "the LF boundary is not a soft wrap"
    );
}

/// D1: the cursor keeps pointing at the same grapheme.
#[test]
fn reflow_preserves_cursor_logical_position() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 5);
    drive(&mut p, &mut g, b"0123456789");
    drive(&mut p, &mut g, b"\x1b[2;3H");
    assert_eq!(
        g.cell(g.cursor().row, g.cursor().col).unwrap().grapheme,
        Grapheme::Ascii(b'7')
    );

    g.reflow(6, 3);
    assert_eq!(
        g.cell(g.cursor().row, g.cursor().col).unwrap().grapheme,
        Grapheme::Ascii(b'7'),
        "cursor tracks '7' at the narrow width"
    );

    g.reflow(6, 5);
    assert_eq!(
        g.cell(g.cursor().row, g.cursor().col).unwrap().grapheme,
        Grapheme::Ascii(b'7'),
        "cursor returns to '7' at the original width"
    );
}

/// D1: the mark lands on the first physical row of its logical line.
#[test]
fn reflow_remaps_prompt_mark_to_its_logical_line_start() {
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(6, 5);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b]133;A\x07");
    drive(&mut p, &mut g, b"promptline");
    assert_eq!(g.prompt_marks().len(), 1);
    assert_eq!(g.prompt_marks()[0].kind, PromptKind::PromptStart);

    g.reflow(6, 3);

    let mark_line = g.prompt_marks()[0].line;
    assert_eq!(g.locate_line(mark_line), MarkLocation::Screen(0));
}

#[test]
fn reflow_never_splits_a_wide_glyph() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 2);
    drive(&mut p, &mut g, "aあ".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Char('あ'));
    assert_eq!(g.cell(1, 1).unwrap().grapheme, Grapheme::Spacer);

    g.reflow(6, 3);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Char('あ'));
    assert_eq!(
        g.cell(0, 2).unwrap().grapheme,
        Grapheme::Spacer,
        "the wide glyph keeps its spacer partner"
    );
}

/// D3: the alt buffer trim/pads and no scrollback is disturbed.
#[test]
fn reflow_on_alternate_screen_delegates_to_resize() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"\x1b[?1049h");
    drive(&mut p, &mut g, b"alt-text");
    assert!(g.on_alternate_screen());

    g.reflow(4, 4);
    assert_eq!(g.cols(), 4);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert!(g.scrollback().is_empty());
}

/// The preserved `?47` snapshot is trimmed at its capture-time column
/// count, not the post-reflow one: the alt buffer trim/pads (D3), it
/// does not reflow.
#[test]
fn reflow_trims_the_preserved_alt_snapshot_at_capture_dimensions() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 16);
    drive(&mut p, &mut g, b"\x1b[?47h");
    drive(&mut p, &mut g, b"\x1b[1;1HA\x1b[2;1HB");
    drive(&mut p, &mut g, b"\x1b[?47l");
    g.reflow(4, 8);
    drive(&mut p, &mut g, b"\x1b[?47h");
    assert_eq!(g.rows(), 4);
    assert_eq!(g.cols(), 8);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'A'));
    assert_eq!(
        g.cell(1, 0).unwrap().grapheme,
        Grapheme::Ascii(b'B'),
        "the alt snapshot must trim from 8x16, not a stale stride"
    );
}

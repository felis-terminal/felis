use super::*;

#[test]
fn autowrap_sets_soft_wrap_on_the_landing_row() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    drive(&mut p, &mut g, b"abcdefg");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(g.row_soft_wrap_continued(1));
    assert!(!g.row_soft_wrap_continued(2));
}

#[test]
fn explicit_newline_does_not_set_soft_wrap() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    drive(&mut p, &mut g, b"ab\r\ncd");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(!g.row_soft_wrap_continued(1));
}

/// The unfillable last cell stays `Empty`, the trailing-Empty shape
/// search's stitcher must trim.
#[test]
fn wide_char_wrap_sets_soft_wrap_and_leaves_trailing_empty() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    drive(&mut p, &mut g, "abc中".as_bytes());
    assert!(g.row_soft_wrap_continued(1));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Char('中'));
}

#[test]
fn autowrap_off_does_not_set_soft_wrap() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    drive(&mut p, &mut g, b"\x1b[?7labcdefg");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(!g.row_soft_wrap_continued(1));
}

/// A later search stitches the logical line back across the seam.
#[test]
fn scroll_pushes_soft_wrap_bit_into_scrollback() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 3);
    drive(&mut p, &mut g, b"abcdef\r\n\r\n");
    assert_eq!(g.scrollback().soft_wrap_continued(0), Some(false));
    assert_eq!(g.scrollback().soft_wrap_continued(1), Some(true));
}

#[test]
fn full_row_erase_clears_soft_wrap_partial_keeps_it() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 3);
    drive(&mut p, &mut g, b"abcdef");
    assert!(g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[1;2H");
    drive(&mut p, &mut g, b"\x1b[2;2H\x1b[K");
    assert!(g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[2K");
    assert!(!g.row_soft_wrap_continued(1));
}

#[test]
fn ed2_clears_all_soft_wrap_bits() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 3);
    drive(&mut p, &mut g, b"abcdef\x1b[2J");
    assert!(!g.row_soft_wrap_continued(0));
    assert!(!g.row_soft_wrap_continued(1));
}

/// Already-blank rows change nothing, so shipping them as `RowDelta`s
/// would be pure wire noise (the vim / lazygit CUP + ED 2 repaint loop
/// clears a mostly-blank screen every frame).
#[test]
fn ed2_default_pen_marks_only_rows_with_content() {
    let mut p = Parser::new();
    let mut g = Grid::new(5, 10);
    drive(&mut p, &mut g, b"top\x1b[3;1Hmid");
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"\x1b[2J");
    for r in 0..5 {
        for c in 0..10 {
            assert!(g.cell(r, c).unwrap().is_blank(), "cell ({r},{c})");
        }
    }
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![0, 2]);
}

#[test]
fn ed2_on_a_blank_screen_marks_nothing() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"\x1b[2J");
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert!(dirty.is_empty(), "unexpected dirty rows: {dirty:?}");
}

/// A BCE pen paints every cell, so the prefix shortcut must not apply.
#[test]
fn ed2_bce_pen_marks_every_row_and_colors_every_cell() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"\x1b[44m\x1b[2J");
    for r in 0..3 {
        for c in 0..4 {
            let cell = *g.cell(r, c).unwrap();
            assert_eq!(cell.grapheme, Grapheme::Empty);
            assert_eq!(g.style(cell.style).bg, Color::Indexed(4), "cell ({r},{c})");
        }
    }
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![0, 1, 2]);
}

#[test]
fn alt_screen_isolates_soft_wrap_from_the_primary() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 3);
    drive(&mut p, &mut g, b"abcdef");
    assert!(g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert!(!g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[HXYZuvw");
    assert!(g.row_soft_wrap_continued(1));
    drive(&mut p, &mut g, b"\x1b[?1049l");
    assert!(g.row_soft_wrap_continued(1));
    assert!(!g.row_soft_wrap_continued(0));
}

/// The bit must stay attached to its content row, not its physical
/// slot.
#[test]
fn resize_keeps_soft_wrap_attached_to_content_after_rotation() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 3);
    drive(&mut p, &mut g, b"abcdefgh\r\n");
    assert!(g.row_soft_wrap_continued(0), "def row still a continuation");
    assert!(g.row_soft_wrap_continued(1), "gh row still a continuation");
    g.resize(4, 3);
    assert!(g.row_soft_wrap_continued(0));
    assert!(g.row_soft_wrap_continued(1));
    assert!(!g.row_soft_wrap_continued(2));
    assert!(!g.row_soft_wrap_continued(3));
}

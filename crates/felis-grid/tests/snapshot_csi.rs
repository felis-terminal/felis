//! Insta snapshots for fixed CSI scripts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use felis_grid::{AttrFlags, Grapheme, Grid, ScrollDirection};
use felis_vt::Parser;

mod common;
use common::{
    cell_char, drive, scroll_ops, styled_cell_lines, write_cells, write_cursor, write_dirty_rows,
    write_pen,
};

fn render(grid: &Grid) -> String {
    let mut out = String::new();
    write_cursor(&mut out, grid);
    write_pen(&mut out, grid);
    write_cells(&mut out, grid);
    let attr_lines = styled_cell_lines(grid);
    if attr_lines.is_empty() {
        writeln!(out, "attrs: (all default)").unwrap();
    } else {
        writeln!(out, "attrs:").unwrap();
        for line in attr_lines {
            writeln!(out, "{line}").unwrap();
        }
    }
    write_dirty_rows(&mut out, grid);
    out
}

#[test]
fn colored_prompt_then_command() {
    let g = drive(
        2,
        20,
        b"\x1b[1;32muser\x1b[0m@\x1b[1;34mhost\x1b[0m$ ls\r\nfoo bar",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=7 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 2x20:
    |user@host$ ls.......|
    |foo bar.............|
    attrs:
      (0,0): fg=idx2 bg=default flags=B
      (0,1): fg=idx2 bg=default flags=B
      (0,2): fg=idx2 bg=default flags=B
      (0,3): fg=idx2 bg=default flags=B
      (0,5): fg=idx4 bg=default flags=B
      (0,6): fg=idx4 bg=default flags=B
      (0,7): fg=idx4 bg=default flags=B
      (0,8): fg=idx4 bg=default flags=B
    dirty_rows: [0, 1]
    ");
}

#[test]
fn cursor_moves_and_overwrite() {
    let g = drive(1, 10, b"abcdef\x1b[3D\x1b[7mX\x1b[0m");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=4 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 1x10:
    |abcXef....|
    attrs:
      (0,3): fg=default bg=default flags=R
    dirty_rows: [0]
    ");
}

#[test]
fn ed_clears_screen_and_cup_homes_cursor() {
    let g = drive(3, 6, b"AAAAAABBBBBBCCCCCC\x1b[2J\x1b[Hhi");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=2 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 3x6:
    |hi....|
    |......|
    |......|
    attrs: (all default)
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn ed_0_clears_from_cursor_to_end_of_screen() {
    let g = drive(3, 6, b"AAAAAABBBBBBCCCCCC\x1b[2;3H\x1b[J");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=2 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 3x6:
    |AAAAAA|
    |BB....|
    |......|
    attrs: (all default)
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn ed_1_clears_from_start_of_screen_to_cursor() {
    let g = drive(3, 6, b"AAAAAABBBBBBCCCCCC\x1b[2;3H\x1b[1J");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=2 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 3x6:
    |......|
    |...BBB|
    |CCCCCC|
    attrs: (all default)
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn ed_3_clears_scrollback_only_and_leaves_screen_intact() {
    // esctest `test_ED_3`: the visible screen stays untouched, only
    // history drops.
    let after_ed3 = drive(3, 6, b"AAAAAABBBBBBCCCCCC\x1b[3J");
    insta::assert_snapshot!(render(&after_ed3), @r"
    cursor: row=2 col=5 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    grid 3x6:
    |AAAAAA|
    |BBBBBB|
    |CCCCCC|
    attrs: (all default)
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn ed_3_drops_scrollback_rows() {
    let g = drive(2, 3, b"AAA\r\nBBB\r\nCCC\r\nDDD\r\nEEE\x1b[3J");
    assert_eq!(g.scrollback().len(), 0);
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=2 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    grid 2x3:
    |DDD|
    |EEE|
    attrs: (all default)
    dirty_rows: [0, 1]
    ");
}

#[test]
fn pending_wrap_holds_until_next_print() {
    let g = drive(2, 5, b"abcde");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=4 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    grid 2x5:
    |abcde|
    |.....|
    attrs: (all default)
    dirty_rows: [0, 1]
    ");
}

#[test]
fn line_feed_at_bottom_pushes_into_scrollback() {
    let g = drive(2, 4, b"row1\r\nrow2\r\nrow3");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=3 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    grid 2x4:
    |row2|
    |row3|
    attrs: (all default)
    dirty_rows: [0, 1]
    ");
}

#[test]
fn alternate_screen_enter_blanks_and_homes_cursor() {
    let g = drive(3, 6, b"primary\r\n\x1b[?1049h\x1b[1;31malt-x");
    assert!(g.on_alternate_screen());
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    pen: fg=idx1 bg=default flags=B
    grid 3x6:
    |alt-x.|
    |......|
    |......|
    attrs:
      (0,0): fg=idx1 bg=default flags=B
      (0,1): fg=idx1 bg=default flags=B
      (0,2): fg=idx1 bg=default flags=B
      (0,3): fg=idx1 bg=default flags=B
      (0,4): fg=idx1 bg=default flags=B
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn alternate_screen_leave_restores_primary_exactly() {
    let g = drive(2, 6, b"\x1b[1;32mhello\x1b[?1049hGARBAGE\x1b[?1049l");
    assert!(!g.on_alternate_screen());
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=5 visible=1 pending_wrap=0
    pen: fg=idx2 bg=default flags=B
    grid 2x6:
    |hello.|
    |......|
    attrs:
      (0,0): fg=idx2 bg=default flags=B
      (0,1): fg=idx2 bg=default flags=B
      (0,2): fg=idx2 bg=default flags=B
      (0,3): fg=idx2 bg=default flags=B
      (0,4): fg=idx2 bg=default flags=B
    dirty_rows: [0, 1]
    ");
}

fn row_text(g: &Grid, r: u16) -> String {
    (0..g.cols()).map(|c| cell_char(g, r, c)).collect()
}

#[test]
fn alternate_screen_resize_trims_the_alt_and_rewraps_the_primary() {
    use felis_vt::Parser;
    let mut g = Grid::new(3, 8);
    let mut p = Parser::new();
    p.advance(&mut g, b"primary-row");
    p.advance(&mut g, b"\x1b[?1049h");
    p.advance(&mut g, b"alt-row");
    g.resize(3, 5);
    assert!(g.on_alternate_screen());
    assert_eq!(row_text(&g, 0), "alt-r");
    p.advance(&mut g, b"\x1b[?1049l");
    assert_eq!(row_text(&g, 0), "prima");
    assert_eq!(row_text(&g, 1), "ry-ro");
    assert_eq!(row_text(&g, 2), "w....");
}

#[test]
fn underline_subparam_form_lands_a_curly_style_via_the_parser() {
    use felis_grid::UnderlineStyle;
    let g = drive(1, 4, b"\x1b[4:3mxyz\x1b[0m");
    let cell = g.cell(0, 0).unwrap();
    let attrs = g.style(cell.style);
    assert!(attrs.flags.contains(AttrFlags::UNDERLINE));
    assert!(!attrs.flags.contains(AttrFlags::ITALIC));
    assert_eq!(attrs.underline_style, UnderlineStyle::Curly);
}

#[test]
fn underline_semicolon_form_keeps_italic_separate() {
    use felis_grid::UnderlineStyle;
    let g = drive(1, 4, b"\x1b[4;3mxyz\x1b[0m");
    let cell = g.cell(0, 0).unwrap();
    let attrs = g.style(cell.style);
    assert!(attrs.flags.contains(AttrFlags::UNDERLINE));
    assert!(attrs.flags.contains(AttrFlags::ITALIC));
    assert_eq!(attrs.underline_style, UnderlineStyle::Single);
}

#[test]
fn overline_and_underline_color_through_the_parser() {
    use felis_grid::{Color, UnderlineStyle};
    let g = drive(1, 4, b"\x1b[53;58;2;10;20;30;4:2mab\x1b[0m");
    let cell = g.cell(0, 0).unwrap();
    let attrs = g.style(cell.style);
    assert!(attrs.flags.contains(AttrFlags::OVERLINE));
    assert!(attrs.flags.contains(AttrFlags::UNDERLINE));
    assert_eq!(attrs.underline_style, UnderlineStyle::Double);
    assert_eq!(attrs.underline_color, Color::Rgb(10, 20, 30));
}

#[test]
fn curses_sliding_object_leaves_no_trailing_glyph() {
    // Regression for the `sl` / `asciiquarium` frame-trail bug.
    let mut p = Parser::new();
    let mut g = Grid::new(3, 12);
    p.advance(&mut g, b"\x1b[2;5HXXXXXX");
    p.advance(&mut g, b"\x1b[2;4HXXXXXX ");
    let trail = g.cell(1, 9).unwrap();
    let visually_blank = matches!(trail.grapheme, Grapheme::Empty | Grapheme::Ascii(b' '));
    assert!(
        visually_blank,
        "trailing cell carried frame-1 glyph: {:?}",
        trail.grapheme,
    );
    for c in 3..=8u16 {
        assert_eq!(
            g.cell(1, c).unwrap().grapheme,
            Grapheme::Ascii(b'X'),
            "train body at col {c}",
        );
    }
}

#[test]
fn curses_sliding_object_via_dch_leaves_no_trailing_glyph() {
    // `sl` emits `\x1b[<row>;1H\x1b[1P` per frame.
    let mut p = Parser::new();
    let mut g = Grid::new(3, 12);
    p.advance(&mut g, b"\x1b[2;5HXXXXXX");
    p.advance(&mut g, b"\x1b[2;1H\x1b[1P");
    let trail = g.cell(1, 9).unwrap();
    assert!(
        matches!(trail.grapheme, Grapheme::Empty),
        "DCH-vacated rightmost cell should be Empty, got {:?}",
        trail.grapheme,
    );
    for c in 3..=8u16 {
        assert_eq!(
            g.cell(1, c).unwrap().grapheme,
            Grapheme::Ascii(b'X'),
            "train body at col {c}",
        );
    }
    for c in 0..=2u16 {
        let cell = g.cell(1, c).unwrap();
        assert!(
            matches!(cell.grapheme, Grapheme::Empty),
            "col {c} should be empty, got {:?}",
            cell.grapheme,
        );
    }
}

#[test]
fn curses_sliding_object_via_ech_leaves_no_trailing_glyph() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 12);
    p.advance(&mut g, b"\x1b[2;5HXXXXXX");
    p.advance(&mut g, b"\x1b[2;10H\x1b[X\x1b[2;4HXXXXXX");
    let trail = g.cell(1, 9).unwrap();
    assert!(
        matches!(trail.grapheme, Grapheme::Empty),
        "ECH'd trailing cell should be Empty, got {:?}",
        trail.grapheme,
    );
    for c in 3..=8u16 {
        assert_eq!(
            g.cell(1, c).unwrap().grapheme,
            Grapheme::Ascii(b'X'),
            "train body at col {c}",
        );
    }
}

#[test]
fn pure_pager_scroll_emits_one_scroll_op_no_dirty_rows_in_region() {
    // docs/reference/ipc.md: a scroll over a clean region shrinks to one
    // `Scrolled` directive.
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC");
    g.damage_mut().clear();

    p.advance(&mut g, b"\x1b[S");

    let ops = scroll_ops(&mut g);
    assert_eq!(ops.len(), 1, "expected exactly one Scrolled effect");
    assert_eq!(ops[0].region_top, 0);
    assert_eq!(ops[0].region_bottom, 2);
    assert_eq!(ops[0].n_rows, 1);
    assert_eq!(ops[0].direction, ScrollDirection::Up);
    let dirty: Vec<usize> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![2usize]);
}

#[test]
fn a_scroll_over_a_written_row_ships_the_directive_and_only_the_written_rows() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCC");
    g.damage_mut().clear();
    drop(scroll_ops(&mut g));

    p.advance(&mut g, b"CC\r\nDDDD");

    let ops = scroll_ops(&mut g);
    assert_eq!(ops.len(), 1, "expected exactly one Scrolled effect");
    assert_eq!((ops[0].region_top, ops[0].region_bottom), (0, 2));
    assert_eq!(ops[0].n_rows, 1);
    let dirty: Vec<usize> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![1, 2], "the row written before the LF, moved up");
}

#[test]
fn ind_outside_left_right_at_scroll_bottom_is_no_op() {
    // esctest `IND_MovesDoesNotScrollOutsideLeftRight`.
    let g = drive(6, 8, b"\x1b[2;5r\x1b[?69h\x1b[2;5s\x1b[5;3Hx\x1b[5;6H\x1bD");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=4 col=5 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 6x8:
    |........|
    |........|
    |........|
    |........|
    |..x.....|
    |........|
    attrs: (all default)
    dirty_rows: [0, 1, 2, 3, 4, 5]
    ");
}

#[test]
fn ri_outside_left_right_at_scroll_top_is_no_op() {
    let g = drive(6, 8, b"\x1b[2;5r\x1b[?69h\x1b[2;5s\x1b[5;3Hx\x1b[2;6H\x1bM");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=5 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 6x8:
    |........|
    |........|
    |........|
    |........|
    |..x.....|
    |........|
    attrs: (all default)
    dirty_rows: [0, 1, 2, 3, 4, 5]
    ");
}

#[test]
fn nel_outside_left_right_snaps_column_but_does_not_scroll() {
    // xterm gates NEL's LF scroll-arm on the pre-CR column.
    let g = drive(6, 8, b"\x1b[2;5r\x1b[?69h\x1b[2;5s\x1b[5;3Hx\x1b[5;6H\x1bE");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=4 col=1 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 6x8:
    |........|
    |........|
    |........|
    |........|
    |..x.....|
    |........|
    attrs: (all default)
    dirty_rows: [0, 1, 2, 3, 4, 5]
    ");
}

#[test]
fn bs_reverse_wrap_at_scroll_top_lands_on_scroll_bottom() {
    // esctest `test_BS_ReverseWrapGoesToBottom`: xterm (383+) bounds
    // reverse-wrap by the TB band under ?1045; ?45 does not take this arm.
    let g = drive(6, 8, b"\x1b[?7h\x1b[?1045h\x1b[2;5r\x1b[2;1H\x08");
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=4 col=7 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 6x8:
    |........|
    |........|
    |........|
    |........|
    |........|
    |........|
    attrs: (all default)
    dirty_rows: [0, 1, 2, 3, 4, 5]
    ");
}

#[test]
fn lnm_makes_lf_perform_implicit_cr() {
    // esctest `test_SM_LNM`.
    let off = drive(3, 8, b"\x1b[20l\x1b[2;5H\n");
    insta::assert_snapshot!(render(&off), @r"
    cursor: row=2 col=4 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 3x8:
    |........|
    |........|
    |........|
    attrs: (all default)
    dirty_rows: [0, 1, 2]
    ");

    let on = drive(3, 8, b"\x1b[20h\x1b[2;5H\n");
    insta::assert_snapshot!(render(&on), @r"
    cursor: row=2 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 3x8:
    |........|
    |........|
    |........|
    attrs: (all default)
    dirty_rows: [0, 1, 2]
    ");
}

#[test]
fn irm_under_declrmm_truncates_at_right_margin() {
    // esctest `test_SM_IRM_TruncatesAtRightMargin`.
    let g = drive(
        2,
        12,
        b"\x1b[1;5Habcdef\x1b[?69h\x1b[5;10s\x1b[1;7H\x1b[4hX",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=7 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 2x12:
    |....abXcde..|
    |............|
    attrs: (all default)
    dirty_rows: [0, 1]
    ");
}

#[test]
fn bs_reverse_wrap_outside_left_margin_lands_at_right_margin() {
    // esctest `test_BS_ReversewrapFromLeftEdgeToRightMargin`: xterm
    // bounds the reverse wrap by DECSLRM.
    let g = drive(
        6,
        12,
        b"\x1b[?7h\x1b[?1045h\x1b[?69h\x1b[5;10s\x1b[3;1H\x08",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=9 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 6x12:
    |............|
    |............|
    |............|
    |............|
    |............|
    |............|
    attrs: (all default)
    dirty_rows: [0, 1, 2, 3, 4, 5]
    ");
}

/// DECCARA under DECSACE 2 sets the listed attributes on every cell of
/// the rectangle and leaves the surrounding cells alone.
#[test]
fn deccara_sets_listed_attributes_in_rectangle() {
    let g = drive(
        3,
        8,
        b"abcdefgh\r\nijklmnop\r\nqrstuvwx\x1b[2*x\x1b[2;3;3;5;1;7$r",
    );
    insta::assert_snapshot!(render(&g), @"
    cursor: row=2 col=7 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    grid 3x8:
    |abcdefgh|
    |ijklmnop|
    |qrstuvwx|
    attrs:
      (1,2): fg=default bg=default flags=BR
      (1,3): fg=default bg=default flags=BR
      (1,4): fg=default bg=default flags=BR
      (2,2): fg=default bg=default flags=BR
      (2,3): fg=default bg=default flags=BR
      (2,4): fg=default bg=default flags=BR
    dirty_rows: [0, 1, 2]
    ");
}

/// DECRARA toggles rather than assigns: the pre-underlined run inside
/// the rectangle comes back plain while its neighbors gain underline.
#[test]
fn decrara_reverses_listed_attributes_in_rectangle() {
    let g = drive(
        3,
        8,
        b"abcdefgh\r\nij\x1b[4mklm\x1b[0mnop\r\nqrstuvwx\x1b[2*x\x1b[2;3;3;5;4$t",
    );
    insta::assert_snapshot!(render(&g), @"
    cursor: row=2 col=7 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    grid 3x8:
    |abcdefgh|
    |ijklmnop|
    |qrstuvwx|
    attrs:
      (2,2): fg=default bg=default flags=U
      (2,3): fg=default bg=default flags=U
      (2,4): fg=default bg=default flags=U
    dirty_rows: [0, 1, 2]
    ");
}

/// With DECSACE at its default (stream), the same parameters cover the
/// text from the start point to the end point in reading order, so the
/// first row runs to the right edge and the last starts at column 1.
#[test]
fn deccara_stream_extent_spans_full_rows_between_endpoints() {
    let g = drive(3, 8, b"abcdefgh\r\nijklmnop\r\nqrstuvwx\x1b[1;3;3;5;1$r");
    insta::assert_snapshot!(render(&g), @"
    cursor: row=2 col=7 visible=1 pending_wrap=1
    pen: fg=default bg=default flags=-
    grid 3x8:
    |abcdefgh|
    |ijklmnop|
    |qrstuvwx|
    attrs:
      (0,2): fg=default bg=default flags=B
      (0,3): fg=default bg=default flags=B
      (0,4): fg=default bg=default flags=B
      (0,5): fg=default bg=default flags=B
      (0,6): fg=default bg=default flags=B
      (0,7): fg=default bg=default flags=B
      (1,0): fg=default bg=default flags=B
      (1,1): fg=default bg=default flags=B
      (1,2): fg=default bg=default flags=B
      (1,3): fg=default bg=default flags=B
      (1,4): fg=default bg=default flags=B
      (1,5): fg=default bg=default flags=B
      (1,6): fg=default bg=default flags=B
      (1,7): fg=default bg=default flags=B
      (2,0): fg=default bg=default flags=B
      (2,1): fg=default bg=default flags=B
      (2,2): fg=default bg=default flags=B
      (2,3): fg=default bg=default flags=B
      (2,4): fg=default bg=default flags=B
    dirty_rows: [0, 1, 2]
    ");
}

/// Origin mode shifts the rectangle by the top and left margins
/// (DECLRMM on), and the grid edge still clamps it; margins never clip
/// the area itself.
#[test]
fn deccara_rectangle_offset_by_margins_under_origin_mode() {
    let g = drive(
        4,
        8,
        b"abcdefgh\r\nijklmnop\r\nqrstuvwx\r\nyz012345\x1b[2*x\x1b[?69h\x1b[3;6s\x1b[2;4r\x1b[?6h\x1b[1;1;4;7;7$r",
    );
    insta::assert_snapshot!(render(&g), @"
    cursor: row=1 col=0 visible=1 pending_wrap=0
    pen: fg=default bg=default flags=-
    grid 4x8:
    |abcdefgh|
    |ijklmnop|
    |qrstuvwx|
    |yz012345|
    attrs:
      (1,2): fg=default bg=default flags=R
      (1,3): fg=default bg=default flags=R
      (1,4): fg=default bg=default flags=R
      (1,5): fg=default bg=default flags=R
      (1,6): fg=default bg=default flags=R
      (1,7): fg=default bg=default flags=R
      (2,2): fg=default bg=default flags=R
      (2,3): fg=default bg=default flags=R
      (2,4): fg=default bg=default flags=R
      (2,5): fg=default bg=default flags=R
      (2,6): fg=default bg=default flags=R
      (2,7): fg=default bg=default flags=R
      (3,2): fg=default bg=default flags=R
      (3,3): fg=default bg=default flags=R
      (3,4): fg=default bg=default flags=R
      (3,5): fg=default bg=default flags=R
      (3,6): fg=default bg=default flags=R
      (3,7): fg=default bg=default flags=R
    dirty_rows: [0, 1, 2, 3]
    ");
}

/// DECCARA merges into the interned style: the colors, the underline
/// color, the hyperlink and both protection bits of a cell inside the
/// rectangle survive the attribute change.
#[test]
fn deccara_preserves_colors_link_and_protection() {
    let g = drive(
        2,
        4,
        b"\x1b[31;44;58:5:9m\x1b[1\"q\x1bV\x1b]8;;https://example.com/\x1b\\ab\
          \x1b]8;;\x1b\\\x1b[2*x\x1b[1;1;1;2;1$r",
    );

    let cell = g.cell(0, 0).expect("cell inside the rectangle");
    let attrs = g.style(cell.style);
    assert_eq!(attrs.fg, felis_grid::Color::Indexed(1));
    assert_eq!(attrs.bg, felis_grid::Color::Indexed(4));
    assert_eq!(attrs.underline_color, felis_grid::Color::Indexed(9));
    assert!(attrs.flags.contains(AttrFlags::BOLD));
    assert!(attrs.flags.contains(AttrFlags::PROTECTED));
    assert!(attrs.flags.contains(AttrFlags::ISO_PROTECTED));
    let uri = cell
        .link
        .and_then(|id| g.hyperlink(id))
        .map(|e| e.uri.as_str().to_owned());
    assert_eq!(uri.as_deref(), Some("https://example.com/"));
}

/// DECCARA's "off" selectors clear just what they name and leave the
/// rest of the cell's flags standing.
#[test]
fn deccara_clear_selectors_turn_off_only_the_named_attributes() {
    let g = drive(1, 4, b"\x1b[1;3;4;7mab\x1b[0m\x1b[2*x\x1b[1;1;1;2;22;24$r");

    let attrs = g.style(g.cell(0, 0).expect("cell inside the rectangle").style);
    assert_eq!(
        attrs.flags,
        AttrFlags::ITALIC | AttrFlags::REVERSE,
        "22 and 24 clear bold and underline only"
    );
}

/// DECRARA reads `4:3` as one selector, as DECCARA does: the shape
/// argument is consumed, not toggled as a separate `3` (italic).
#[test]
fn decrara_colon_underline_selector_leaves_italic_alone() {
    let g = drive(1, 4, b"ab\x1b[2*x\x1b[1;1;1;2;4:3$t");

    let attrs = g.style(g.cell(0, 0).expect("cell inside the rectangle").style);
    assert_eq!(attrs.flags, AttrFlags::UNDERLINE);
}

/// Each DECRARA selector is its own XOR, so naming one twice cancels
/// out.
#[test]
fn decrara_repeated_selector_cancels_out() {
    let g = drive(1, 4, b"\x1b[1mab\x1b[0m\x1b[2*x\x1b[1;1;1;2;1;1$t");

    let attrs = g.style(g.cell(0, 0).expect("cell inside the rectangle").style);
    assert_eq!(attrs.flags, AttrFlags::BOLD);
}

const RECT_ATTRS: AttrFlags = AttrFlags::BOLD
    .union(AttrFlags::FAINT)
    .union(AttrFlags::ITALIC)
    .union(AttrFlags::UNDERLINE)
    .union(AttrFlags::BLINK)
    .union(AttrFlags::REVERSE)
    .union(AttrFlags::CONCEAL)
    .union(AttrFlags::STRIKETHROUGH);

/// The "on" selectors DECCARA accepts, each with the one attribute it
/// maps to.
const RECT_ON_SELECTORS: [(&str, AttrFlags); 10] = [
    ("1", AttrFlags::BOLD),
    ("2", AttrFlags::FAINT),
    ("3", AttrFlags::ITALIC),
    ("4", AttrFlags::UNDERLINE),
    ("5", AttrFlags::BLINK),
    ("6", AttrFlags::BLINK),
    ("7", AttrFlags::REVERSE),
    ("8", AttrFlags::CONCEAL),
    ("9", AttrFlags::STRIKETHROUGH),
    ("21", AttrFlags::UNDERLINE),
];

/// The "off" selectors, each with the attribute set it clears.
const RECT_OFF_SELECTORS: [(&str, AttrFlags); 7] = [
    ("22", AttrFlags::BOLD.union(AttrFlags::FAINT)),
    ("23", AttrFlags::ITALIC),
    ("24", AttrFlags::UNDERLINE),
    ("25", AttrFlags::BLINK),
    ("27", AttrFlags::REVERSE),
    ("28", AttrFlags::CONCEAL),
    ("29", AttrFlags::STRIKETHROUGH),
];

fn rect_flags_after(prelude: &[u8], op: &str) -> AttrFlags {
    let mut bytes = prelude.to_vec();
    bytes.extend_from_slice(format!("ab\x1b[0m\x1b[2*x\x1b[1;1;1;2;{op}").as_bytes());
    let g = drive(1, 4, &bytes);
    g.style(g.cell(0, 0).expect("cell inside the rectangle").style)
        .flags
}

/// Every DECCARA "on" selector sets exactly its own attribute on a
/// plain cell, so a swapped or dropped arm in the selector table fails
/// on the selector it breaks.
#[test]
fn deccara_each_on_selector_sets_exactly_its_attribute() {
    for (selector, expected) in RECT_ON_SELECTORS {
        assert_eq!(
            rect_flags_after(b"", &format!("{selector}$r")),
            expected,
            "DECCARA selector {selector}"
        );
    }
}

/// Every DECCARA "off" selector clears exactly its own attribute from a
/// cell that carries the whole reachable set.
#[test]
fn deccara_each_off_selector_clears_exactly_its_attribute() {
    for (selector, cleared) in RECT_OFF_SELECTORS {
        assert_eq!(
            rect_flags_after(b"\x1b[1;2;3;4;5;7;8;9m", &format!("{selector}$r")),
            RECT_ATTRS.difference(cleared),
            "DECCARA selector {selector}"
        );
    }
}

/// DECCARA reads the `4:n` colon form as one selector, as SGR does:
/// the shape lands on the cell and `4:0` turns the underline off.
#[test]
fn deccara_colon_underline_selector_sets_the_shape() {
    use felis_grid::UnderlineStyle;
    let g = drive(1, 4, b"ab\x1b[0m\x1b[2*x\x1b[1;1;1;2;4:3$r");
    let attrs = g.style(g.cell(0, 0).expect("cell inside the rectangle").style);
    assert_eq!(attrs.flags, AttrFlags::UNDERLINE);
    assert_eq!(attrs.underline_style, UnderlineStyle::Curly);

    assert_eq!(
        rect_flags_after(b"\x1b[3;4:3m", "4:0$r"),
        AttrFlags::ITALIC,
        "4:0 clears the underline and consumes its shape argument"
    );
}

/// DECRARA maps every selector, "on" and "off" alike, to the same mask
/// and toggles it: from a plain cell each one turns exactly its
/// attribute on, and from a fully attributed cell turns it off.
#[test]
fn decrara_each_selector_toggles_exactly_its_attribute() {
    let all = RECT_ON_SELECTORS.iter().chain(RECT_OFF_SELECTORS.iter());
    for (selector, mask) in all {
        assert_eq!(
            rect_flags_after(b"", &format!("{selector}$t")),
            *mask,
            "DECRARA selector {selector} from plain"
        );
        assert_eq!(
            rect_flags_after(b"\x1b[1;2;3;4;5;7;8;9m", &format!("{selector}$t")),
            RECT_ATTRS.difference(*mask),
            "DECRARA selector {selector} from all set"
        );
    }
}

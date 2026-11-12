use super::*;

#[test]
fn advance_cursor_after_image_placement_lands_right_of_the_last_row() {
    let mut g = Grid::new(8, 16);
    g.screen.cursor.row = 1;
    g.screen.cursor.col = 2;
    g.advance_cursor_after_image_placement(nz(2), nz(3), false);
    assert_eq!((g.screen.cursor.row, g.screen.cursor.col), (2, 5));
    assert!(!g.screen.cursor.pending_wrap);
}

#[test]
fn advance_cursor_after_image_placement_respects_no_cursor_move() {
    // `C=1` leaves the cursor untouched; the producer keeps writing
    // where it expected.
    let mut g = Grid::new(8, 16);
    g.screen.cursor.row = 1;
    g.screen.cursor.col = 2;
    g.advance_cursor_after_image_placement(nz(2), nz(3), true);
    assert_eq!(g.screen.cursor.row, 1);
    assert_eq!(g.screen.cursor.col, 2);
}

#[test]
fn advance_cursor_after_image_placement_wraps_at_the_right_edge() {
    let mut g = Grid::new(8, 16);
    g.screen.cursor.row = 1;
    g.screen.cursor.col = 14;
    g.advance_cursor_after_image_placement(nz(1), nz(2), false);
    assert_eq!((g.screen.cursor.row, g.screen.cursor.col), (2, 0));
}

fn top_text(g: &Grid) -> String {
    row_text_trim(g.row_content(0).unwrap(), g.cluster_table())
}

#[test]
fn advance_cursor_after_image_placement_scrolls_past_the_bottom() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"one\r\ntwo\r\nthree");
    g.screen.cursor.col = 0;
    g.advance_cursor_after_image_placement(nz(4), nz(2), false);
    assert_eq!((g.screen.cursor.row, g.screen.cursor.col), (3, 2));
    assert_eq!(top_text(&g), "three", "two rows scrolled off the top");
}

#[test]
fn advance_cursor_after_image_placement_wrap_on_the_bottom_row_scrolls_one() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"one\r\ntwo\r\n\r\n");
    g.screen.cursor.col = 4;
    g.advance_cursor_after_image_placement(nz(1), nz(4), false);
    assert_eq!((g.screen.cursor.row, g.screen.cursor.col), (3, 0));
    assert_eq!(top_text(&g), "two");
}

#[test]
fn advance_cursor_after_image_placement_under_decom_stays_in_the_region() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 8);
    // Region rows 1..=3; the image's last row (3) is inside it, the wrap
    // pushes one past, the region scrolls and the cursor stays on row 3.
    drive(&mut p, &mut g, b"\x1b[2;4r\x1b[?6h\x1b[2;5H");
    g.advance_cursor_after_image_placement(nz(2), nz(4), false);
    assert_eq!((g.screen.cursor.row, g.screen.cursor.col), (3, 0));
}

#[test]
fn advance_cursor_after_image_placement_without_decom_may_leave_the_region() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 8);
    // As above with origin mode off: the region still scrolls, but the
    // wrapped cursor keeps row 4, below the region.
    drive(&mut p, &mut g, b"\x1b[2;4r\x1b[3;5H");
    g.advance_cursor_after_image_placement(nz(2), nz(4), false);
    assert_eq!((g.screen.cursor.row, g.screen.cursor.col), (4, 0));
}

#[test]
fn advance_cursor_after_image_placement_leaving_the_region_clamps_to_the_screen() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 8);
    // Region rows 1..=3; the image's last row (5) is below it: the region
    // scrolls by two, the cursor keeps row 5.
    drive(&mut p, &mut g, b"\x1b[2;4r\x1b[3;1H");
    g.advance_cursor_after_image_placement(nz(4), nz(2), false);
    assert_eq!((g.screen.cursor.row, g.screen.cursor.col), (5, 2));
}

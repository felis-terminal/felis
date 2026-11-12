use super::*;

//
// A 3-row grid with 3 rows pushed into scrollback: live screen occupies
// absolute lines [3, 6), retained scrollback [0, 3), anything below 0
// is evicted.

fn grid_three_pushed() -> Grid {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 4);
    drive(&mut p, &mut g, b"\r\n\r\n\r\n\r\n\r\n");
    assert_eq!(g.scrollback_total_pushed(), 3);
    g
}

/// `line == base` is the first live row, not the last scrollback row.
#[test]
fn locate_line_maps_screen_lines_to_rows() {
    let g = grid_three_pushed();
    assert_eq!(g.locate_line(3), MarkLocation::Screen(0));
    assert_eq!(g.locate_line(4), MarkLocation::Screen(1));
    assert_eq!(g.locate_line(5), MarkLocation::Screen(2));
}

/// `base - sb_len` is the oldest retained row; `base - 1` the youngest.
#[test]
fn locate_line_maps_scrollback_lines_to_indices() {
    let g = grid_three_pushed();
    assert_eq!(g.locate_line(0), MarkLocation::Scrollback(0));
    assert_eq!(g.locate_line(1), MarkLocation::Scrollback(1));
    assert_eq!(g.locate_line(2), MarkLocation::Scrollback(2));
}

/// `base - sb_len` is still retained, one below it is gone.
#[test]
fn locate_line_reports_evicted_below_the_retained_ring() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    for _ in 0..DEFAULT_SCROLLBACK_ROWS + 5 {
        drive(&mut p, &mut g, b"\r\n");
    }
    let base = g.scrollback_total_pushed();
    let sb_len = g.scrollback().len() as u64;
    assert_eq!(g.locate_line(base - sb_len), MarkLocation::Scrollback(0));
    assert_eq!(g.locate_line(base - sb_len - 1), MarkLocation::Evicted);
}

/// The pruned counter records the drop so the daemon's ordinal cursor
/// can translate past it.
#[test]
fn evicted_prompt_marks_are_pruned_from_the_front() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b]133;A\x07");
    assert_eq!(g.prompt_marks().len(), 1);
    assert_eq!(g.prompt_marks_pruned(), 0);
    for _ in 0..DEFAULT_SCROLLBACK_ROWS + 5 {
        drive(&mut p, &mut g, b"\r\n");
    }
    assert!(g.prompt_marks().is_empty(), "evicted mark must be dropped");
    assert_eq!(g.prompt_marks_pruned(), 1);
}

/// ED 3 prunes only the marks pointing into history; live-screen marks
/// keep their absolute lines.
#[test]
fn ed_3_prunes_marks_pointing_into_scrollback() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b]133;A\x07"); // line 0
    drive(&mut p, &mut g, b"\r\n\r\n"); // push rows into scrollback
    drive(&mut p, &mut g, b"\x1b]133;A\x07"); // a live-screen mark
    let live_line = g.scrollback_total_pushed() + u64::from(g.cursor().row);
    assert!(g.prompt_marks().len() >= 2);
    assert!(!g.scrollback().is_empty());
    drive(&mut p, &mut g, b"\x1b[3J");
    let base = g.scrollback_total_pushed();
    assert!(
        g.prompt_marks().iter().all(|m| m.line >= base),
        "no surviving mark may point into the cleared scrollback"
    );
    assert!(g.prompt_marks().iter().any(|m| m.line == live_line));
    assert!(g.prompt_marks_pruned() >= 1);
}

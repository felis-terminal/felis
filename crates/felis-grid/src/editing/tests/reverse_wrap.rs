use super::*;

/// BS without reverse-wraparound stops at col 0
/// (`test_BS_StopsAtOrigin`).
#[test]
fn backspace_stops_at_column_zero_without_reverse_wrap() {
    let g = drive_new(2, 4, b"\x1b[1;1H\x08");
    assert_eq!(g.cursor().col, 0);
    assert_eq!(g.cursor().row, 0);
}

/// `?1045` + DECAWM: BS at col 0 wraps to the previous row's last
/// column (`test_BS_WrapsInWraparoundMode`).
#[test]
fn backspace_with_reverse_wrap_extend_moves_to_previous_row_last_col() {
    let g = drive_new(4, 4, b"\x1b[?7h\x1b[?1045h\x1b[3;1H\x08");
    assert_eq!(g.cursor().col, 3);
    assert_eq!(g.cursor().row, 1);
}

/// `?1045` + DECAWM: BS at top-left wraps to bottom-right
/// (`test_BS_ReverseWrapGoesToBottom`).
#[test]
fn backspace_with_reverse_wrap_extend_wraps_from_top_to_bottom() {
    let g = drive_new(4, 4, b"\x1b[?7h\x1b[?1045h\x1b[1;1H\x08");
    assert_eq!(g.cursor().col, 3);
    assert_eq!(g.cursor().row, 3);
}

/// `?45`: BS at col 0 of a continuation row retraces onto the
/// previous row (xterm 380+ arm of `test_BS_AfterOneWrappedInline`).
#[test]
fn backspace_with_reverse_wrap_inline_retraces_a_wrapped_line() {
    let g = drive_new(4, 4, b"\x1b[?7h\x1b[?45habcde\r\x08");
    assert_eq!(g.cursor().col, 3);
    assert_eq!(g.cursor().row, 0);
}

/// `?45`: BS does not wrap from a row that never autowrapped
/// (`test_BS_InitialReverseWraparound`, xterm 380+).
#[test]
fn backspace_with_reverse_wrap_inline_stays_on_unwrapped_row() {
    let g = drive_new(4, 4, b"\x1b[?7h\x1b[?45h\x1b[1;1H\x1bE\x08");
    assert_eq!(g.cursor().col, 0);
    assert_eq!(g.cursor().row, 1);
}

/// With `pending_wrap` set, BS under `?45` only clears the flag
/// (`test_DECSET_ReverseWraparoundLastCol_BS`).
#[test]
fn backspace_with_pending_wrap_and_reverse_wrap_is_a_noop() {
    let g = drive_new(2, 4, b"\x1b[?7h\x1b[?45h\x1b[1;3Hab\x08");
    assert_eq!(g.cursor().col, 3);
    assert_eq!(g.cursor().row, 0);
}

/// CUB under `?1045` + DECAWM wraps through the previous row
/// (`test_DECSET_ReverseWraparound_Multi`).
#[test]
fn cub_with_reverse_wrap_extend_walks_back_through_previous_row() {
    let g = drive_new(3, 4, b"\x1b[?7h\x1b[?1045h\x1b[2;1H\x1b[2D");
    assert_eq!(g.cursor().col, 2);
    assert_eq!(g.cursor().row, 0);
}

/// CUB under `?45` stops at the logical line's head (xterm 380+ arm
/// of `test_CUB_AfterOneWrappedInline`).
#[test]
fn cub_with_reverse_wrap_inline_stops_at_the_logical_line_head() {
    let g = drive_new(3, 4, b"\x1b[?7h\x1b[?45habcdef\r\x1b[1C\x1b[20D");
    assert_eq!(g.cursor().col, 0);
    assert_eq!(g.cursor().row, 0);
}

/// CUB without reverse-wrap clamps at col 0 even with DECAWM
/// (`test_CUB_StopsAtLeftEdge`).
#[test]
fn cub_without_reverse_wrap_clamps_at_column_zero() {
    let g = drive_new(3, 4, b"\x1b[?7h\x1b[2;2H\x1b[5D");
    assert_eq!(g.cursor().col, 0);
    assert_eq!(g.cursor().row, 1);
}

/// DECRQM `?45` reads the functional `reverse_wrap_inline` field.
#[test]
fn decrqm_mode_45_reports_reverse_wraparound_state() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?45$p");
    let initial = reply_text(&mut g);
    assert_eq!(initial[0], "\x1b[?45;2$y");
    drive(&mut p, &mut g, b"\x1b[?45h\x1b[?45$p");
    let set = reply_text(&mut g);
    assert_eq!(set[0], "\x1b[?45;1$y");
}

/// ?45 and ?1045 carry distinct state since the xterm-2023 split:
/// setting one must not flip the other's DECRQM report.
#[test]
fn decrqm_modes_45_and_1045_are_independent() {
    let mut g = Grid::new(2, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?45h\x1b[?1045$p");
    let extend = reply_text(&mut g);
    assert_eq!(extend[0], "\x1b[?1045;2$y");
    drive(
        &mut p,
        &mut g,
        b"\x1b[?1045h\x1b[?45l\x1b[?45$p\x1b[?1045$p",
    );
    let both = reply_text(&mut g);
    assert_eq!(both[0], "\x1b[?45;2$y");
    assert_eq!(both[1], "\x1b[?1045;1$y");
}

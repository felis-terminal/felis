use super::*;

/// The daemon's `compose_diffs` / rehydrate paths borrow the row to skip
/// the per-row clone; the contents must match a per-cell walk.
#[test]
fn row_cells_returns_the_same_contents_as_per_cell_walk() {
    let mut g = Grid::new(3, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdefgh\r\nABCDEFGH\r\n12345678");
    for r in 0..g.rows() {
        let walked: Vec<_> = (0..g.cols())
            .map(|c| g.cell(r, c).copied().unwrap_or_default())
            .collect();
        let slice = g.row_cells(r).expect("in-range row");
        assert_eq!(slice.len(), walked.len());
        for (a, b) in slice.iter().zip(walked.iter()) {
            assert_eq!(a, b, "row {r}");
        }
    }
}

#[test]
fn row_cells_returns_none_for_out_of_range_rows() {
    let g = Grid::new(2, 4);
    assert!(g.row_cells(2).is_none());
    assert!(g.row_cells(u16::MAX).is_none());
}

#[test]
fn fresh_grid_has_default_cells_and_damage_marked() {
    let g = Grid::new(3, 5);
    assert_eq!(g.rows(), 3);
    assert_eq!(g.cols(), 5);
    for r in 0..3 {
        for c in 0..5 {
            assert!(g.cell(r, c).unwrap().is_blank());
        }
    }
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![0, 1, 2]);
}

#[test]
fn print_ascii_advances_cursor_and_marks_row() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 5);
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"hi");
    assert_eq!(
        g.cursor(),
        Cursor {
            row: 0,
            col: 2,
            visible: true,
            pending_wrap: false,
        }
    );
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'h'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'i'));
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, vec![0]);
}

#[test]
fn cr_lf_moves_cursor_to_next_line() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 5);
    drive(&mut p, &mut g, b"a\r\nb");
    assert_eq!(
        g.cursor(),
        Cursor {
            row: 1,
            col: 1,
            visible: true,
            pending_wrap: false,
        }
    );
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'b'));
}

#[test]
fn backspace_moves_cursor_left() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 5);
    drive(&mut p, &mut g, b"abc\x08X");
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'X'));
    assert_eq!(g.cursor().col, 3);
}

#[test]
fn cup_jumps_cursor() {
    let mut p = Parser::new();
    let mut g = Grid::new(5, 10);
    drive(&mut p, &mut g, b"\x1b[3;7HX");
    assert_eq!(g.cell(2, 6).unwrap().grapheme, Grapheme::Ascii(b'X'));
    assert_eq!(g.cursor().col, 7);
    assert_eq!(g.cursor().row, 2);
}

#[test]
fn el_clears_to_end_of_line() {
    let mut g = Grid::new(2, 6);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"abcdef\r\x1b[3C");
    drive(&mut p, &mut g, b"\x1b[K");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'c'));
    assert!(g.cell(0, 3).unwrap().is_blank());
    assert!(g.cell(0, 5).unwrap().is_blank());
}

#[test]
fn bel_byte_sets_pending_flag_and_take_clears_it() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x07");
    assert!(g.take_bell_pending(), "BEL should set the flag");
    assert!(
        !g.take_bell_pending(),
        "second take must return false (drained)"
    );
}

#[test]
fn multiple_bels_in_one_chunk_coalesce_to_one_pending_event() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x07\x07\x07");
    assert!(g.take_bell_pending());
    assert!(!g.take_bell_pending());
}

#[test]
fn bel_does_not_alter_cells_or_cursor() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"a\x07b");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'b'));
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn decscusr_maps_each_param_to_the_right_cursor_style() {
    // xterm: 0/1/2 → Block, 3/4 → Underline, 5/6 → Bar; anything else is
    // ignored.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    assert_eq!(g.cursor_style(), CursorStyle::Block, "default is Block");

    for ps in [3u8, 4] {
        drive(&mut p, &mut g, format!("\x1b[{ps} q").as_bytes());
        assert_eq!(g.cursor_style(), CursorStyle::Underline, "ps={ps}");
    }
    for ps in [5u8, 6] {
        drive(&mut p, &mut g, format!("\x1b[{ps} q").as_bytes());
        assert_eq!(g.cursor_style(), CursorStyle::Bar, "ps={ps}");
    }
    for ps in [0u8, 1, 2] {
        drive(&mut p, &mut g, b"\x1b[3 q"); // first set to Underline
        drive(&mut p, &mut g, format!("\x1b[{ps} q").as_bytes());
        assert_eq!(g.cursor_style(), CursorStyle::Block, "ps={ps}");
    }
    drive(&mut p, &mut g, b"\x1b[5 q");
    drive(&mut p, &mut g, b"\x1b[99 q");
    assert_eq!(g.cursor_style(), CursorStyle::Bar, "unknown ps ignored");
}

#[test]
fn decstr_resets_cursor_style_to_block() {
    // VT510 / xterm: soft reset restores the cursor shape, or a vim crash
    // + `tput reset` would leave vim's bar cursor permanently.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[5 q\x1b[!p");
    assert_eq!(g.cursor_style(), CursorStyle::Block);
}

// docs/explanation/data-model/grid-and-cells.md pins `Cell` at 16 bytes
// (four per cache line); residency and the ascii store floor ride it, so
// a field that regrows it must justify the cost here.
const _: () = {
    const fn assert_copy<T: Copy>() {}
    assert_copy::<Cell>();
    assert!(size_of::<Cell>() == 16);
};

// These pin cursor / response effects the coverage-matrix snapshot
// probes cannot see: an arm whose only effect is a cursor move on an
// otherwise-empty grid leaves the rendered screen identical.

#[test]
fn cud_moves_the_cursor_down_n_rows() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 8);
    drive(&mut p, &mut g, b"\x1b[3;4H\x1b[2B");
    assert_eq!((g.cursor().row, g.cursor().col), (4, 3));
}

#[test]
fn dl_deletes_lines_pulling_lower_rows_up() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 4);
    drive(&mut p, &mut g, b"AAAA\r\nBBBB\r\nCCCC\x1b[1;1H\x1b[1M");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'B'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'C'));
}

#[test]
fn esc_ind_advances_one_row() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"\x1b[1;3H\x1bD");
    assert_eq!((g.cursor().row, g.cursor().col), (1, 2));
}

#[test]
fn esc_hts_sets_a_tab_stop_at_the_cursor_column() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"\x1b[3g\x1b[1;4H\x1bH\x1b[1;1H\t");
    assert_eq!(g.cursor().col, 3, "HT should land on the HTS-set stop");
}

/// SCS is consumed so the selector bytes never reach the screen, and
/// the selected set stays inert: `ESC ( 0` does not translate `q` into
/// a line-drawing glyph (`docs/reference/protocols/vt-compliance.md`
/// "Consumed without effect").
#[test]
fn scs_charset_selection_leaves_printed_text_unchanged() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 6);
    drive(&mut p, &mut g, b"\x1b(0q\x1b)Bx");

    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'q'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

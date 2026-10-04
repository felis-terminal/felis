use crate::test_support::drive;
use crate::*;
use felis_vt::Parser;
use felis_vt::kitty_text_sizing::{HAlign, VAlign};

/// `Sizing`'s private fields disallow a functional-update literal
/// outside its module.
fn sized(scale: u8, cell_width: u8) -> Sizing {
    Sizing::new(scale, cell_width, 0, 0, VAlign::Top, HAlign::Left).unwrap()
}

#[test]
fn char_cell_width_matches_unicode_width_table() {
    // The renderer's IME pre-edit path relies on the public helper
    // agreeing with the grid's internal widths.
    assert_eq!(char_cell_width('A'), 1);
    assert_eq!(char_cell_width('あ'), 2);
    assert_eq!(char_cell_width('中'), 2);
    assert_eq!(char_cell_width('\u{0301}'), 0);
}

#[test]
fn sizing_registry_round_trips_through_a_handle() {
    let mut g = Grid::new(8, 16);
    let s = sized(3, 2);
    let h = g.install_sizing(s).expect("registry has room");
    assert_eq!(g.sizing_count(), 1);
    assert_eq!(g.sizing_by_handle(h), Some(&s));
    let bogus = SizingHandle::new(99).unwrap();
    assert_eq!(g.sizing_by_handle(bogus), None);
}

/// The spec models OSC 66 as a run, so the registry must support
/// sharing.
#[test]
fn one_handle_can_stamp_many_cells() {
    let mut g = Grid::new(8, 16);
    let s = sized(2, 0);
    let h = g.install_sizing(s).unwrap();
    for col in 0..5 {
        g.set_cell_sizing(0, col, Some(h));
    }
    assert_eq!(g.sized_cell_count(), 5);
    for col in 0..5 {
        assert_eq!(g.cell_sizing_handle(0, col), Some(h));
        assert_eq!(g.cell_sizing(0, col), Some(&s));
    }
    assert_eq!(g.cell_sizing_handle(0, 5), None);
    assert_eq!(g.cell_sizing(0, 5), None);
}

#[test]
fn clearing_a_cell_removes_it_from_the_side_table_only() {
    let mut g = Grid::new(8, 16);
    let h = g.install_sizing(Sizing::default()).unwrap();
    g.set_cell_sizing(2, 3, Some(h));
    assert_eq!(g.sized_cell_count(), 1);
    g.set_cell_sizing(2, 3, None);
    assert_eq!(g.sized_cell_count(), 0);
    assert_eq!(g.sizing_count(), 1);
    assert_eq!(g.sizing_by_handle(h), Some(&Sizing::default()));
}

/// Matches the `Damage::mark` tolerance so a resize-window race needs
/// no separate sweep before stamping a fresh row.
#[test]
fn out_of_range_stamp_is_dropped_silently() {
    let mut g = Grid::new(4, 4);
    let h = g.install_sizing(Sizing::default()).unwrap();
    g.set_cell_sizing(99, 0, Some(h));
    g.set_cell_sizing(0, 99, Some(h));
    assert_eq!(g.sized_cell_count(), 0);
}

#[test]
fn resize_trims_sizing_side_table_but_preserves_registry() {
    let mut g = Grid::new(8, 16);
    let h = g.install_sizing(sized(2, 0)).unwrap();
    g.set_cell_sizing(0, 0, Some(h)); // survives
    g.set_cell_sizing(7, 15, Some(h)); // dropped (off bottom-right)
    g.set_cell_sizing(3, 3, Some(h)); // survives
    assert_eq!(g.sized_cell_count(), 3);

    g.resize(4, 8);
    assert_eq!(g.sized_cell_count(), 2);
    assert_eq!(g.cell_sizing_handle(0, 0), Some(h));
    assert_eq!(g.cell_sizing_handle(3, 3), Some(h));
    assert_eq!(g.sizing_count(), 1);
    assert!(g.sizing_by_handle(h).is_some());
}

/// `docs/explanation/protocols/kitty-text-sizing.md` "Interaction with
/// reflow": the
/// primary keeps its char but spanned cells revert to `Empty`, so the
/// renderer paints no half-drawn block from the prior geometry.
#[test]
fn resize_discards_sized_run_whose_block_extent_overflows() {
    let mut g = Grid::new(4, 16);
    let mut p = Parser::new();
    // `s=2:w=3` "AB" → two 6×2 blocks, A at (0,0), B at (0,6).
    drive(&mut p, &mut g, b"\x1b]66;s=2:w=3;AB\x07");
    assert_eq!(g.sized_cell_count(), 24);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'A'));
    assert_eq!(g.cell(0, 6).unwrap().grapheme, Grapheme::Ascii(b'B'));

    // Shrink to 2 rows, 9 cols: B's block (0..2, 6..12) overflows.
    g.resize(2, 9);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'A'));
    assert!(g.cell_sizing_handle(0, 0).is_some(), "A survives");
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::SizedSpacer);
    assert!(g.cell_sizing_handle(1, 5).is_some());

    assert_eq!(g.cell(0, 6).unwrap().grapheme, Grapheme::Ascii(b'B'));
    assert_eq!(g.cell_sizing_handle(0, 6), None, "B's sizing dropped");
    assert_eq!(g.cell(0, 7).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(0, 8).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(1, 6).unwrap().grapheme, Grapheme::Empty);
}

/// Full-screen TUIs (vim, less) use the alt screen; primary sizings
/// must survive the round trip.
#[test]
fn alternate_screen_round_trip_preserves_primary_sizings() {
    let mut g = Grid::new(4, 8);
    let h = g.install_sizing(sized(2, 0)).unwrap();
    g.set_cell_sizing(0, 0, Some(h));
    assert_eq!(g.cell_sizing_handle(0, 0), Some(h));

    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert_eq!(g.cell_sizing_handle(0, 0), None);

    // An alt-screen stamp must not leak back to the primary.
    g.set_cell_sizing(2, 4, Some(h));

    drive(&mut p, &mut g, b"\x1b[?1049l");
    assert_eq!(g.cell_sizing_handle(0, 0), Some(h));
    assert_eq!(g.cell_sizing_handle(2, 4), None);
    // `sizing_table` lives at screen scope, so the handle stays valid.
    assert_eq!(g.sizing_count(), 1);
}

/// A handle left at its pre-resize coordinates would point past the
/// rebuilt cell array.
#[test]
fn leaving_alt_after_a_resize_rewraps_saved_primary_sizings() {
    let mut g = Grid::new(8, 16);
    let h = g.install_sizing(Sizing::default()).unwrap();
    g.set_cell_sizing(7, 15, Some(h));
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?1049h");

    g.resize(4, 8);

    // The primary re-wraps at 4×8, so the stamped cell's logical row 7
    // splits across two physical rows.
    drive(&mut p, &mut g, b"\x1b[?1049l");
    assert_eq!(g.cell_sizing_handle(3, 7), Some(h));
    assert_eq!(g.sized_cell_count(), 1);
    assert_eq!(g.cell_sizing_handle(7, 15), None);
}

/// xterm's "alt screen has no scrollback": `enter_alternate` clears the
/// sizings and the cells, so no prior alt state leaks in.
#[test]
fn successive_alternate_screen_entries_start_fresh() {
    let mut g = Grid::new(4, 8);
    let primary_handle = g.install_sizing(Sizing::default()).unwrap();
    let alt_handle = g.install_sizing(sized(2, 0)).unwrap();
    g.set_cell_sizing(0, 0, Some(primary_handle));

    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert!(
        g.cell_sizing_handle(0, 0).is_none(),
        "alt should start empty"
    );
    g.set_cell_sizing(2, 4, Some(alt_handle));
    assert_eq!(g.cell_sizing_handle(2, 4), Some(alt_handle));

    drive(&mut p, &mut g, b"\x1b[?1049l");
    assert_eq!(g.cell_sizing_handle(0, 0), Some(primary_handle));

    // The previous alt's stamp at (2, 4) must not have leaked.
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert!(
        g.cell_sizing_handle(2, 4).is_none(),
        "second alt entry must not carry the first alt's stamps"
    );
    assert_eq!(g.sized_cell_count(), 0);
}

/// The wire layer walks `0..sizing_count()` to ship the registry on
/// rehydrate and depends on contiguous 1-based numbering.
#[test]
fn install_sizing_returns_dense_one_based_handles() {
    let mut g = Grid::new(2, 2);
    let h1 = g.install_sizing(Sizing::default()).unwrap();
    let h2 = g.install_sizing(sized(2, 0)).unwrap();
    let h3 = g.install_sizing(sized(3, 0)).unwrap();
    assert_eq!(h1.get(), 1);
    assert_eq!(h2.get(), 2);
    assert_eq!(h3.get(), 3);
    assert_eq!(g.sizing_count(), 3);
}

/// Pins the rehydrate side-band the daemon ships per row.
#[test]
fn row_sized_cells_lists_columns_in_order_with_resolved_sizing() {
    let mut g = Grid::new(4, 8);
    let small = sized(2, 0);
    let big = sized(3, 2);
    let h_small = g.install_sizing(small).unwrap();
    let h_big = g.install_sizing(big).unwrap();
    // Stamped out of column order to prove the collector sorts.
    g.set_cell_sizing(1, 5, Some(h_big));
    g.set_cell_sizing(1, 1, Some(h_small));

    assert_eq!(g.row_sized_cells(1), vec![(1, small), (5, big)]);
    assert_eq!(g.row_sized_cells(0), Vec::new());
    assert_eq!(g.row_sized_cells(99), Vec::new());
}

/// Mirrors `cell_at_viewport`; scrollback-band rows read as unsized
/// (sizing is not carried into scrollback yet).
#[test]
fn cell_sizing_at_viewport_maps_live_and_scrollback_band() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"row1\r\nrow2\r\nrow3\r\nrow4");
    assert_eq!(g.scrollback().len(), 2);
    let s = sized(2, 0);
    let h = g.install_sizing(s).unwrap();
    g.set_cell_sizing(0, 0, Some(h));

    assert_eq!(g.cell_sizing_at_viewport(0, 0, 0), Some(s));
    assert_eq!(g.cell_sizing_at_viewport(0, 0, 1), None);
    assert_eq!(g.cell_sizing_at_viewport(0, 99, 0), None);
    assert_eq!(g.cell_sizing_at_viewport(0, 0, 99), None);
    assert_eq!(g.cell_sizing_at_viewport(1, 0, 0), None);
    assert_eq!(g.cell_sizing_at_viewport(1, 1, 0), Some(s));
}

/// Same composed-view mapping as `cell_sizing_at_viewport`.
#[test]
fn viewport_row_sized_cells_map_live_and_scrollback_band() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"row1\r\nrow2\r\nrow3\r\nrow4");
    let s = sized(2, 0);
    let h = g.install_sizing(s).unwrap();
    g.set_cell_sizing(0, 2, Some(h));

    assert_eq!(g.viewport_row(0, 0).unwrap().sized_cells, vec![(2, s)]);
    assert!(g.viewport_row(0, 99).is_none());
    assert_eq!(g.viewport_row(1, 0).unwrap().sized_cells, Vec::new());
    assert_eq!(g.viewport_row(1, 1).unwrap().sized_cells, vec![(2, s)]);
}

/// Kitty spec: any write into a multi-cell character erases the whole
/// character. The struck cell is a spanned cell off the primary, so
/// `sized_block_at` must walk both up and left.
#[test]
fn printing_over_a_foreign_sized_run_erases_the_whole_block() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 16);
    // One 6×2 block ('A' with s=2:w=3) anchored at (0,0).
    drive(&mut p, &mut g, b"\x1b]66;s=2:w=3;A\x07");
    assert_eq!(g.sized_cell_count(), 12);

    // The dispatch is over (`current_sizing_handle == None`), so the
    // whole block is foreign.
    drive(&mut p, &mut g, b"\x1b[2;5HX");
    assert_eq!(g.sized_cell_count(), 0, "the whole block is erased");
    assert_eq!(g.cell_sizing_handle(0, 0), None, "primary cleared");
    assert_eq!(g.cell(1, 4).unwrap().grapheme, Grapheme::Ascii(b'X'));
}

/// `sized_block_at` must walk to the real top-left, not assume
/// (0,0); a wrong primary leaves the block partly sized.
#[test]
fn sized_run_not_anchored_at_origin_is_fully_located_and_erased() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 16);
    // A 4×2 block ('A' with s=2:w=2) at rows 1-2, cols 2-5.
    drive(&mut p, &mut g, b"\x1b[2;3H\x1b]66;s=2:w=2;A\x07");
    assert_eq!(g.sized_cell_count(), 8);
    assert!(g.cell_sizing_handle(1, 2).is_some(), "primary at (1,2)");

    // Strike the block's bottom-right spanned cell.
    drive(&mut p, &mut g, b"\x1b[3;6HX");
    assert_eq!(
        g.sized_cell_count(),
        0,
        "the whole off-origin block is erased"
    );
    assert_eq!(g.cell(2, 5).unwrap().grapheme, Grapheme::Ascii(b'X'));
}

/// `cell_sizing_handle` indexes the cell array with no inner guard, so
/// weakening the `||` to `&&` would let a single off-axis coord index
/// past the array.
#[test]
fn cell_sizing_handle_guards_each_axis_independently() {
    let mut g = Grid::new(8, 16);
    let h = g.install_sizing(Sizing::default()).unwrap();
    g.set_cell_sizing(0, 0, Some(h));
    assert_eq!(g.cell_sizing_handle(0, 0), Some(h));
    // `(7, 99)` is the discriminating case: `7*16 + 99` overruns the
    // 128-cell array; `(0, 99)` stays in bounds and only pins the
    // contract.
    assert_eq!(g.cell_sizing_handle(99, 0), None);
    assert_eq!(g.cell_sizing_handle(7, 99), None);
    assert_eq!(g.cell_sizing_handle(0, 99), None);
}

/// A cell carrying a handle with no registry entry (fuzz-found) must
/// clear just that cell instead of panicking; reached by stamping a
/// bogus handle directly, since the append-only install path cannot
/// produce one.
#[test]
fn clear_foreign_sized_run_with_orphan_handle_clears_only_that_cell() {
    let mut g = Grid::new(4, 4);
    g.install_sizing(Sizing::default()).unwrap(); // registry len 1
    let orphan = SizingHandle::new(99).unwrap(); // far past the registry
    g.set_cell_sizing(0, 0, Some(orphan));
    assert_eq!(g.sized_cell_count(), 1);

    g.clear_foreign_sized_run(0, 0);
    assert_eq!(g.cell_sizing_handle(0, 0), None);
    assert_eq!(g.sized_cell_count(), 0);
}

#[test]
fn sized_bidi_override_with_no_owner_takes_no_column() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, "\x1b]66;;\u{202E}\x1b\\A".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the A with the override folded in");
    };
    assert_eq!(g.cluster_str(id), Some("A\u{202E}"));
    assert_eq!(g.cursor().col, 1);
}

#[test]
fn combining_mark_in_a_scaled_run_does_not_move_the_cursor() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, "\x1b]66;s=2;e\u{0301}\x07".as_bytes());
    assert_eq!(g.cursor().col, 2);
}

#[test]
fn sized_run_fits_without_counting_a_bidi_override() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 2);
    drive(&mut p, &mut g, "\x1b]66;;\u{202E}AB\x07".as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("cell 0,0 should hold the A with the override folded in");
    };
    assert_eq!(g.cluster_str(id), Some("A\u{202E}"));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'B'));
}

#[test]
fn sized_run_widened_by_a_selector_past_the_grid_is_discarded() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 2);
    drive(&mut p, &mut g, "\x1b]66;;\u{2764}\u{FE0F}A\x07".as_bytes());
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cursor().col, 0);
    assert_eq!(g.scrollback().len(), 0);
}

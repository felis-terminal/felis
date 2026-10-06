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

/// A printed run that lands on any cell of a sized block erases the
/// whole block, whichever print loop carries it.
#[test]
fn printing_a_run_over_a_sized_block_erases_it_whole() {
    for (case, bytes) in [
        (
            "ASCII over its second column",
            &b"\x1b]66;s=2;A\x07\x1b[1;2Hxy"[..],
        ),
        ("ASCII over its lower row", b"\x1b]66;s=2;A\x07\x1b[2;1Hxy"),
        (
            "ASCII wrapping onto its row",
            b"\x1b[2;1H\x1b]66;s=2;A\x07\x1b[1;9Hxyz",
        ),
        (
            "CJK over its lower row",
            "\x1b]66;s=2;A\x07\x1b[2;1H字字".as_bytes(),
        ),
    ] {
        let mut p = Parser::new();
        let mut g = Grid::new(4, 10);
        drive(&mut p, &mut g, bytes);
        assert!(
            (0..4).all(|r| g.row_sized_cells(r).is_empty()),
            "{case}: no cell keeps the block's handle"
        );
    }
}

/// After one OSC 66 run, text printed past it still lands normally.
#[test]
fn text_after_a_sized_block_prints_beside_it() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    drive(&mut p, &mut g, "\x1b]66;s=2;A\x07xy字".as_bytes());
    assert_eq!(g.row_sized_cells(0).len(), 2);
    assert_eq!(g.row_sized_cells(1).len(), 2);
    assert_eq!(g.screen.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'x'));
    assert_eq!(g.screen.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'y'));
    assert_eq!(g.screen.cell(0, 4).unwrap().grapheme, Grapheme::Char('字'));
    assert_eq!(g.screen.cell(0, 5).unwrap().grapheme, Grapheme::Spacer);
}

/// Text that starts right after a sized block does not touch it: the
/// block keeps its cells and the text lands beside it.
#[test]
fn text_right_after_a_sized_block_leaves_it_alone() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    drive(&mut p, &mut g, b"\x1b]66;s=2;A\x07xyz");
    assert_eq!(g.row_sized_cells(0).len(), 2);
    assert_eq!(g.screen.cell(0, 2).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

/// Every live cell carrying a sizing handle belongs to a block that is
/// on the grid in full: each footprint cell is live and carries it.
fn assert_blocks_whole(g: &Grid, case: &str) {
    for r in 0..g.screen.rows {
        let occ = g.screen.occupancy[g.screen.phys_row(r)];
        for c in 0..occ {
            if g.screen.cell(r, c).and_then(|cell| cell.sizing).is_none() {
                continue;
            }
            let block = g
                .sized_block_at(r, c)
                .unwrap_or_else(|| panic!("{case}: ({r},{c}) carries a handle with no primary"));
            // A block written within its height of the bottom is clipped
            // there on write; a move must not tear what remains.
            for br in block.top..(block.top + block.rows).min(g.screen.rows) {
                let live = g.screen.occupancy[g.screen.phys_row(br)];
                for bc in block.left..(block.left + block.cols).min(g.screen.cols) {
                    assert!(
                        bc < live
                            && g.screen.cell(br, bc).and_then(|cell| cell.sizing)
                                == Some(block.handle),
                        "{case}: block at ({},{}) lost ({br},{bc})",
                        block.top,
                        block.left
                    );
                }
            }
        }
    }
}

/// The primary of the first block, or `None` once it is gone.
fn primary(g: &Grid) -> Option<(u16, u16)> {
    (0..g.screen.rows).find_map(|r| {
        let occ = g.screen.occupancy[g.screen.phys_row(r)];
        (0..occ).find_map(|c| g.sized_block_at(r, c).map(|b| (b.top, b.left)))
    })
}

/// `(case, cursor before the write, op, primary afterwards)`.
type MoveCase = (
    &'static str,
    &'static [u8],
    &'static [u8],
    Option<(u16, u16)>,
);

/// A 2×2 `A` at `at` on a 4×10 grid, then `op`: a move whose seam
/// crosses the block erases it whole, one that carries all of it moves
/// it whole, and either way no half survives.
#[test]
fn a_cell_move_keeps_or_erases_a_multi_row_block_whole() {
    let origin: &[u8] = b"\x1b[1;1H";
    let row1: &[u8] = b"\x1b[2;1H";
    let cases: [MoveCase; 18] = [
        ("ICH on its top row", origin, b"\x1b[1;1H\x1b[@", None),
        ("ICH on its bottom row", origin, b"\x1b[2;1H\x1b[@", None),
        (
            "IRM print on its top row",
            origin,
            b"\x1b[4h\x1b[1;1Hx",
            None,
        ),
        ("DCH right of it", origin, b"\x1b[1;5H\x1b[P", Some((0, 0))),
        ("DCH left of it", b"\x1b[1;4H", b"\x1b[1;1H\x1b[P", None),
        (
            "SL, its left edge on the seam",
            b"\x1b[1;2H",
            b"\x1b[ @",
            Some((0, 0)),
        ),
        (
            "DECIC, region top cuts it",
            origin,
            b"\x1b[2;3r\x1b[2;1H\x1b['}",
            None,
        ),
        (
            "DECIC, region holds it",
            row1,
            b"\x1b[2;3r\x1b[2;1H\x1b['}",
            Some((1, 1)),
        ),
        ("SR, region top cuts it", origin, b"\x1b[2;3r\x1b[ A", None),
        (
            "SR, region holds it",
            row1,
            b"\x1b[2;3r\x1b[ A",
            Some((1, 1)),
        ),
        (
            "DECBI, region top cuts it",
            origin,
            b"\x1b[?69h\x1b[2;3r\x1b[2;1H\x1b6",
            None,
        ),
        ("SU, region top cuts it", origin, b"\x1b[2;3r\x1b[S", None),
        ("SU, inner seam cuts it", row1, b"\x1b[2;4r\x1b[S", None),
        ("SD, region top cuts it", origin, b"\x1b[2;3r\x1b[T", None),
        (
            "RI at the region top",
            origin,
            b"\x1b[2;3r\x1b[2;1H\x1bM",
            None,
        ),
        ("IL below its top row", origin, b"\x1b[2;1H\x1b[L", None),
        ("DL below its top row", origin, b"\x1b[2;1H\x1b[M", None),
        (
            "subrect SU, region top cuts it",
            origin,
            b"\x1b[?69h\x1b[1;5s\x1b[2;3r\x1b[S",
            None,
        ),
    ];
    for (case, at, op, expected) in cases {
        let mut p = Parser::new();
        let mut g = Grid::new(4, 10);
        drive(&mut p, &mut g, at);
        drive(&mut p, &mut g, b"\x1b]66;s=2;A\x07");
        drive(&mut p, &mut g, op);
        assert_blocks_whole(&g, case);
        assert_eq!(primary(&g), expected, "{case}");
    }
}

#[test]
fn a_full_screen_scroll_moves_a_multi_row_block_whole() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    drive(&mut p, &mut g, b"\x1b]66;s=2;A\x07\x1b[T");
    assert_blocks_whole(&g, "SD");
    assert_eq!(primary(&g), Some((1, 0)));
}

/// The rule is for blocks taller than a row: a single-row `w=2` run
/// shifts like any wide character.
#[test]
fn ich_still_shifts_a_single_row_sized_run() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    drive(&mut p, &mut g, b"\x1b]66;w=2;A\x07\x1b[1;1H\x1b[@");
    assert_blocks_whole(&g, "ICH");
    assert_eq!(primary(&g), Some((0, 1)));
}

/// Text either side of an erased block, on both of its rows, stays.
#[test]
fn erasing_a_block_spares_its_neighbors() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    drive(
        &mut p,
        &mut g,
        b"\x1b]66;s=2;A\x07xy\x1b[2;3Hz\x1b[1;1H\x1b[@",
    );
    assert_eq!(primary(&g), None);
    let ch = |r, c| g.screen.cell(r, c).unwrap().grapheme;
    assert_eq!(ch(0, 3), Grapheme::Ascii(b'x'));
    assert_eq!(ch(0, 4), Grapheme::Ascii(b'y'));
    assert_eq!(ch(1, 2), Grapheme::Ascii(b'z'));
}

/// Rows pushed into history keep their part of a block, so its text
/// survives there; the rows left live lose theirs.
#[test]
fn a_scroll_into_history_keeps_the_departing_part_of_a_block() {
    let row1: &[u8] = b"\x1b[2;1H";
    let origin: &[u8] = b"\x1b[1;1H";
    let cases: [(&str, &[u8], &[u8]); 5] = [
        ("LF at the bottom", origin, b"\x1b[4;1H\n"),
        ("SU 1", origin, b"\x1b[S"),
        (
            "top-anchored partial region",
            origin,
            b"\x1b[1;3r\x1b[3;1H\n",
        ),
        ("SU 2 with the block lower", row1, b"\x1b[2S"),
        (
            "history and bottom seams at once",
            row1,
            b"\x1b[1;3r\x1b[2S",
        ),
    ];
    for (case, at, op) in cases {
        let mut p = Parser::new();
        let mut g = Grid::new(4, 10);
        drive(&mut p, &mut g, at);
        let scale: &[u8] = if case.starts_with("history and bottom") {
            b"\x1b]66;s=3;A\x07"
        } else {
            b"\x1b]66;s=2;A\x07"
        };
        drive(&mut p, &mut g, scale);
        drive(&mut p, &mut g, b"\x1b[4;10Hz");
        drive(&mut p, &mut g, op);
        assert_blocks_whole(&g, case);
        assert_eq!(primary(&g), None, "{case}");
        assert_eq!(
            g.cell_at_viewport(1, 0, 0).unwrap().grapheme,
            Grapheme::Ascii(b'A'),
            "{case}: the youngest history row holds the primary"
        );
    }
}

#[test]
fn a_scroll_on_the_alternate_screen_erases_a_cut_block() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    drive(&mut p, &mut g, b"\x1b[?1049h\x1b]66;s=2;A\x07\x1b[S");
    assert_blocks_whole(&g, "alternate SU");
    assert_eq!(primary(&g), None);
}

fn arb_move() -> impl proptest::strategy::Strategy<Value = Vec<u8>> {
    use proptest::prelude::*;
    let n = 1u8..4;
    prop_oneof![
        (1u8..7, 1u8..13).prop_map(|(r, c)| format!("\x1b[{r};{c}H").into_bytes()),
        Just(b"x".to_vec()),
        n.clone().prop_map(|n| format!("\x1b[{n}@").into_bytes()),
        n.clone().prop_map(|n| format!("\x1b[{n}P").into_bytes()),
        prop_oneof![Just(b"\x1b[4h".to_vec()), Just(b"\x1b[4l".to_vec())],
        n.clone().prop_map(|n| format!("\x1b[{n}'}}").into_bytes()),
        n.clone().prop_map(|n| format!("\x1b[{n}'~").into_bytes()),
        n.clone().prop_map(|n| format!("\x1b[{n} @").into_bytes()),
        n.clone().prop_map(|n| format!("\x1b[{n} A").into_bytes()),
        n.clone().prop_map(|n| format!("\x1b[{n}S").into_bytes()),
        n.clone().prop_map(|n| format!("\x1b[{n}T").into_bytes()),
        n.clone().prop_map(|n| format!("\x1b[{n}L").into_bytes()),
        n.prop_map(|n| format!("\x1b[{n}M").into_bytes()),
        prop_oneof![Just(b"\n".to_vec()), Just(b"\x1bM".to_vec())],
        prop_oneof![Just(b"\x1b6".to_vec()), Just(b"\x1b9".to_vec())],
        (1u8..7, 1u8..7).prop_map(|(t, b)| format!("\x1b[{t};{}r", t + b).into_bytes()),
        (1u8..7, 1u8..7).prop_map(|(l, w)| format!("\x1b[?69h\x1b[{l};{}s", l + w).into_bytes()),
        prop_oneof![Just(b"\x1b[?69l".to_vec()), Just(b"\x1b[r".to_vec())],
        prop_oneof![Just(b"\x1b[?1049h".to_vec()), Just(b"\x1b[?1049l".to_vec())],
    ]
}

#[derive(Clone, Debug)]
enum Op {
    /// An OSC 66 `A` at `s` scale and `w` width.
    Sized(u16, u16),
    Move(Vec<u8>),
}

fn arb_op() -> impl proptest::strategy::Strategy<Value = Op> {
    use proptest::prelude::*;
    prop_oneof![
        (1u16..4, 0u16..3).prop_map(|(s, w)| Op::Sized(s, w)),
        arb_move().prop_map(Op::Move),
    ]
}

proptest::proptest! {
    /// No sequence of cell moves leaves part of a block behind. A write
    /// that does not fit at the cursor is skipped: it is clipped at the
    /// grid edge before any move runs.
    #[test]
    fn no_cell_move_leaves_part_of_a_block(ops in proptest::collection::vec(arb_op(), 1..40)) {
        let mut p = Parser::new();
        let mut g = Grid::new(6, 12);
        for op in &ops {
            let bytes = match op {
                Op::Sized(s, w) => {
                    let cursor = g.screen.cursor;
                    if cursor.row + s > g.screen.rows || cursor.col + s * (*w).max(1) > g.screen.cols {
                        continue;
                    }
                    format!("\x1b]66;s={s}:w={w};A\x07").into_bytes()
                }
                Op::Move(bytes) => bytes.clone(),
            };
            drive(&mut p, &mut g, &bytes);
            assert_blocks_whole(&g, &format!("{:?}", String::from_utf8_lossy(&bytes)));
        }
    }
}

/// A write whose lower rows land on another block erases that block
/// whole, not only the cells it covers.
#[test]
fn a_write_over_a_blocks_lower_row_erases_it_whole() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 12);
    drive(
        &mut p,
        &mut g,
        b"\x1b[2;1H\x1b]66;s=2;A\x07\x1b[1;1H\x1b]66;s=2;B\x07",
    );
    assert_blocks_whole(&g, "B over A's top row");
    assert_eq!(primary(&g), Some((0, 0)));
    assert_eq!(g.screen.cell(2, 0).and_then(|c| c.sizing), None);
}

/// IRM leaves no copy of the shifted cell under the cursor for the
/// print to mistake for a sized character.
#[test]
fn an_irm_print_before_a_sized_run_shifts_it_whole() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 10);
    drive(&mut p, &mut g, b"\x1b]66;w=2;A\x07\x1b[4h\x1b[1;1Hx");
    assert_blocks_whole(&g, "IRM print");
    assert_eq!(primary(&g), Some((0, 1)));
    assert_eq!(g.screen.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'x'));
}

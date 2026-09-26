//! `Grid::gc_sizings` must reclaim the entries no cell references and leave
//! every live sized cell resolving to the sizing it was drawn with
//! (`docs/explanation/data-model/grid-and-cells.md` "Sizing handle").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::{Grid, Sizing};
use felis_vt::Parser;

fn sized_cells(grid: &Grid) -> Vec<(u16, u16, Sizing)> {
    let mut out = Vec::new();
    for r in 0..grid.rows() {
        for c in 0..grid.cols() {
            let cell = grid.cell(r, c).unwrap();
            if let Some(handle) = cell.sizing {
                out.push((r, c, *grid.sizing_by_handle(handle).unwrap()));
            }
        }
    }
    out
}

fn repaint_sized_run(parser: &mut Parser, grid: &mut Grid, count: u32) {
    for _ in 0..count {
        parser.advance(grid, b"\x1b[H\x1b]66;s=2;A\x1b\\");
    }
}

#[test]
fn gc_reclaims_repainted_sizing_entries_and_preserves_live_cells() {
    let mut parser = Parser::default();
    let mut grid = Grid::new(4, 20);

    repaint_sized_run(&mut parser, &mut grid, 3000);
    let grown = grid.sizing_count();
    assert!(
        grown >= 3000,
        "a repainted run must mint an entry per repaint, got {grown}"
    );

    let before = sized_cells(&grid);
    assert!(!before.is_empty(), "the last repaint leaves a sized cell");

    grid.gc_sizings();

    assert!(
        grid.sizing_count() < 8,
        "GC should reclaim the {grown} unreferenced entries down to the live \
         run, got {}",
        grid.sizing_count()
    );
    assert_eq!(
        before,
        sized_cells(&grid),
        "gc_sizings changed an observable cell — a handle was mis-remapped"
    );
}

/// `find_sized_primary` tells adjacent multi-cell characters apart by
/// handle, so equal sizings must stay on distinct handles.
#[test]
fn gc_keeps_adjacent_equal_sizings_on_distinct_handles() {
    let mut parser = Parser::default();
    let mut grid = Grid::new(4, 20);
    parser.advance(&mut grid, b"\x1b]66;s=2;A\x1b\\\x1b]66;s=2;B\x1b\\");

    grid.gc_sizings();

    let first = grid.cell(0, 0).unwrap().sizing.expect("A is sized");
    let second_col = (0..grid.cols())
        .find(|c| {
            grid.cell(0, *c)
                .unwrap()
                .sizing
                .is_some_and(|handle| handle != first)
        })
        .expect("B must carry its own handle");
    assert_eq!(
        grid.sizing_by_handle(first),
        grid.sizing_by_handle(grid.cell(0, second_col).unwrap().sizing.unwrap()),
        "the two runs share a sizing value",
    );
    assert_eq!(grid.sizing_count(), 2, "and not the entry behind it");
}

#[test]
fn gc_is_idempotent_on_a_settled_table() {
    let mut parser = Parser::default();
    let mut grid = Grid::new(4, 20);
    parser.advance(&mut grid, b"\x1b]66;s=2;A\x1b\\plain");

    let before = sized_cells(&grid);
    grid.gc_sizings();
    let first = grid.sizing_count();
    grid.gc_sizings();

    assert_eq!(
        first,
        grid.sizing_count(),
        "a second sweep with no churn must not shrink further"
    );
    assert_eq!(
        before,
        sized_cells(&grid),
        "idempotent GC must not alter cells"
    );
}

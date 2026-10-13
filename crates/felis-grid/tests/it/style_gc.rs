//! `Grid::gc_styles` must reclaim the unreferenced ids and leave every live
//! cell resolving to the pen it was written with
//! (`docs/explanation/data-model/grid-and-cells.md` "Style interning").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use felis_grid::Grid;
use felis_vt::Parser;

fn fingerprint(grid: &Grid) -> String {
    let mut out = String::new();
    for r in 0..grid.rows() {
        for c in 0..grid.cols() {
            let cell = grid.cell(r, c).unwrap();
            let a = grid.style(cell.style);
            write!(
                out,
                "{r},{c}:{:?}/{:?}/{:?}/{:?}/{:?};",
                cell.grapheme, a.fg, a.bg, a.flags, a.underline_style
            )
            .unwrap();
        }
    }
    out
}

fn churn_pens(parser: &mut Parser, grid: &mut Grid, count: u32) {
    for i in 0..count {
        let (r, g, b) = (
            (i & 0xFF) as u8,
            ((i >> 8) & 0xFF) as u8,
            ((i >> 16) & 0xFF) as u8,
        );
        parser.advance(grid, format!("\x1b[38;2;{r};{g};{b}m").as_bytes());
    }
}

#[test]
fn gc_reclaims_unreferenced_pens_and_preserves_live_cells() {
    let mut parser = Parser::default();
    let mut grid = Grid::new(6, 20);

    parser.advance(&mut grid, b"\x1b[38;2;200;30;40m");
    parser.advance(&mut grid, b"SURVIVOR");
    parser.advance(&mut grid, b"\x1b[0m\r\n");

    let before = fingerprint(&grid);

    churn_pens(&mut parser, &mut grid, 3000);
    let grown = grid.style_table_len();
    assert!(
        grown > 3000,
        "expected the truecolor flood to grow the table past 3000, got {grown}"
    );

    parser.advance(&mut grid, b"\x1b[0m");

    grid.gc_styles();

    let after = grid.style_table_len();
    assert!(
        after < 8,
        "GC should reclaim the {grown} unreferenced pens down to the few live \
         (default + survivor), got {after}"
    );

    assert_eq!(
        before,
        fingerprint(&grid),
        "gc_styles changed an observable cell — a handle was mis-remapped"
    );

    // `is_blank` relies on the default pen keeping its reserved id across
    // a sweep.
    parser.advance(&mut grid, b"\x1b[2J\x1b[H");
    for r in 0..grid.rows() {
        for c in 0..grid.cols() {
            assert!(
                grid.cell(r, c).unwrap().is_blank(),
                "cell ({r},{c}) not blank after clear post-GC"
            );
        }
    }
}

#[test]
fn gc_is_idempotent_on_a_settled_table() {
    let mut parser = Parser::default();
    let mut grid = Grid::new(4, 10);
    parser.advance(&mut grid, b"\x1b[1;31mred\x1b[0m plain");

    let fp = fingerprint(&grid);
    grid.gc_styles();
    let len1 = grid.style_table_len();
    grid.gc_styles();
    let len2 = grid.style_table_len();

    assert_eq!(
        len1, len2,
        "a second GC with no churn must not shrink further"
    );
    assert_eq!(fp, fingerprint(&grid), "idempotent GC must not alter cells");
}

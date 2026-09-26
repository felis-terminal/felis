//! `RefTests`: replay captured raw PTY byte streams into a fresh `Parser` +
//! `Grid` and pin the screen with insta (`docs/explanation/testing.md`
//! "Peer conformance practices"). Each scenario under `tests/ref/<name>/`
//! provides `recording.bin` and `size.json`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use felis_grid::Grid;
use felis_vt::Parser;

/// Rows sit between `|` fences so the trailing-blank trim cannot hide a
/// trailing-content regression.
fn render(grid: &Grid) -> String {
    let clusters = grid.cluster_table();
    let cursor = grid.cursor();
    let mut out = format!(
        "size: {} rows x {} cols\ncursor: row {} col {} visible {}\n",
        grid.rows(),
        grid.cols(),
        cursor.row,
        cursor.col,
        cursor.visible,
    );
    for r in 0..grid.rows() {
        let cells = grid.row_cells(r).expect("row index in range");
        out.push('|');
        out.push_str(&felis_grid::row_text_trim(cells, clusters));
        out.push_str("|\n");
    }
    out
}

#[test]
fn recordings_replay_to_their_pinned_screens() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ref");
    let mut scenarios: Vec<_> = std::fs::read_dir(&root)
        .expect("tests/ref exists")
        .filter_map(|e| {
            let e = e.unwrap();
            e.file_type().unwrap().is_dir().then(|| e.path())
        })
        .collect();
    scenarios.sort();
    assert!(
        !scenarios.is_empty(),
        "an empty tests/ref/ means the suite silently pins nothing"
    );
    for dir in scenarios {
        let name = dir.file_name().unwrap().to_str().unwrap().to_owned();
        let size: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("size.json")).unwrap()).unwrap();
        let rows = u16::try_from(size["rows"].as_u64().expect("size.json rows")).unwrap();
        let cols = u16::try_from(size["cols"].as_u64().expect("size.json cols")).unwrap();
        let bytes = std::fs::read(dir.join("recording.bin")).unwrap();
        let mut grid = Grid::new(rows, cols);
        let mut parser = Parser::new();
        parser.advance(&mut grid, &bytes);
        insta::assert_snapshot!(name, render(&grid));
    }
}

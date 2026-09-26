//! Property tests for the grapheme-cluster interner
//! (`docs/explanation/data-model/grid-and-cells.md` "Cluster interning").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::{Grapheme, Grid};
use felis_vt::Parser;
use proptest::prelude::*;

const MARKS: &[char] = &['\u{0301}', '\u{0308}', '\u{0327}', '\u{0323}'];

fn base_char() -> impl Strategy<Value = char> {
    prop_oneof![prop::char::range('a', 'z'), prop::char::range('A', 'Z')]
}

fn clustered_cell() -> impl Strategy<Value = (char, Vec<char>)> {
    (
        base_char(),
        proptest::collection::vec(prop::sample::select(MARKS), 1..=3),
    )
}

proptest! {
    #[test]
    fn cluster_str_round_trips_the_composed_text(
        cells in proptest::collection::vec(clustered_cell(), 1..=20),
    ) {
        let cols = u16::try_from(cells.len()).unwrap();
        let mut grid = Grid::new(1, cols);
        let mut parser = Parser::new();

        let mut input = String::new();
        let mut expected: Vec<String> = Vec::with_capacity(cells.len());
        for (base, marks) in &cells {
            let mut s = String::new();
            s.push(*base);
            for m in marks {
                s.push(*m);
            }
            input.push_str(&s);
            expected.push(s);
        }
        parser.advance(&mut grid, input.as_bytes());

        for (col, want) in expected.iter().enumerate() {
            let cell = grid.cell(0, u16::try_from(col).unwrap()).unwrap();
            match cell.grapheme {
                Grapheme::Cluster(id) => {
                    prop_assert_eq!(
                        grid.cluster_str(id),
                        Some(want.as_str()),
                        "col {} cluster text mismatch",
                        col
                    );
                }
                other => prop_assert!(
                    false,
                    "col {} expected a Cluster, got {:?}",
                    col,
                    other
                ),
            }
        }
    }

    #[test]
    fn identical_clusters_share_one_table_entry(
        base in base_char(),
        mark in prop::sample::select(MARKS),
        repeats in 2u16..=30,
    ) {
        let mut grid = Grid::new(1, repeats);
        let mut parser = Parser::new();
        let mut input = String::new();
        for _ in 0..repeats {
            input.push(base);
            input.push(mark);
        }
        parser.advance(&mut grid, input.as_bytes());

        let first_id = match grid.cell(0, 0).unwrap().grapheme {
            Grapheme::Cluster(id) => id,
            other => return Err(TestCaseError::fail(format!("expected cluster, got {other:?}"))),
        };
        for col in 0..repeats {
            prop_assert_eq!(
                grid.cell(0, col).unwrap().grapheme,
                Grapheme::Cluster(first_id),
                "col {} should share the first cell's interned handle",
                col
            );
        }
        prop_assert_eq!(grid.cluster_count(), 1);
    }
}

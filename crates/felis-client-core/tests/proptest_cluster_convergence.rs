//! Verifies daemon/shadow cluster convergence (`docs/explanation/data-model/grid-and-cells.md`).
//!
//! Asserts the shadow mirrors daemon cell handles and resolved cluster text
//! even when entries arrive permuted or after referencing row deltas.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_client_core::ShadowScreen;
use felis_grid::{Grapheme, Grid, RowEncode, encode_row};
use felis_protocol::{RowPayload, messages::GridMsg};
use felis_vt::Parser;
use proptest::prelude::*;

const MARKS: &[char] = &['\u{0301}', '\u{0308}', '\u{0327}', '\u{0323}'];

fn cluster_entries(grid: &Grid) -> Vec<(u32, String)> {
    (1..=grid.cluster_count())
        .filter_map(|i| {
            let id = u32::try_from(i).ok()?;
            let text = grid.cluster_str(std::num::NonZeroU32::new(id)?)?;
            Some((id, text.to_owned()))
        })
        .collect()
}

fn base_char() -> impl Strategy<Value = char> {
    prop_oneof![prop::char::range('a', 'z'), prop::char::range('A', 'Z')]
}

fn cell_input() -> impl Strategy<Value = (char, Vec<char>)> {
    (
        base_char(),
        proptest::collection::vec(prop::sample::select(MARKS), 0..=3),
    )
}

proptest! {
    /// Pins: a shadow driven in the daemon's emission order (Cluster
    /// messages, then `RowDeltas`) ends cell-for-cell identical to the
    /// daemon grid, with every cluster handle resolving to the same text.
    #[test]
    fn shadow_mirrors_daemon_clusters(
        rows_input in proptest::collection::vec(
            proptest::collection::vec(cell_input(), 1..=12),
            1..=6,
        ),
    ) {
        let rows = u16::try_from(rows_input.len()).unwrap();
        let cols = u16::try_from(rows_input.iter().map(Vec::len).max().unwrap()).unwrap();

        let mut grid = Grid::new(rows, cols);
        let mut parser = Parser::new();
        let mut input = String::new();
        for (i, row) in rows_input.iter().enumerate() {
            if i > 0 {
                input.push_str("\r\n");
            }
            for (base, marks) in row {
                input.push(*base);
                for m in marks {
                    input.push(*m);
                }
            }
        }
        parser.advance(&mut grid, input.as_bytes());

        let mut shadow = ShadowScreen::new(rows, cols);

        // The daemon's `compose_diffs` order: entries before the rows that reference them.
        for (id, text) in cluster_entries(&grid) {
            shadow
                .apply(&GridMsg::Cluster { id, text })
                .expect("cluster apply");
        }
        let dirty: Vec<usize> = grid.damage().dirty_rows().collect();
        for row_idx in dirty {
            let r = u16::try_from(row_idx).unwrap();
            let cells = grid.row_cells(r).unwrap_or(&[]);
            let sized = grid.row_sized_cells(r);
            let body = encode_row(
                RowEncode {
                    cells,
                    pad_to: cells.len(),
                    sized_cells: &sized,
                    soft_wrap_continued: grid.row_soft_wrap_continued(r),
                },
                grid.style_table(),
            )
            .unwrap();
            shadow
                .apply(&GridMsg::RowDelta { rows: vec![(r, RowPayload(body))] })
                .expect("row delta apply");
        }

        let sg = shadow.screen();
        for r in 0..rows {
            for c in 0..cols {
                let d = grid.cell(r, c).unwrap();
                let s = sg.cell(r, c).unwrap();
                prop_assert_eq!(d, s, "cell {},{} diverged (handle)", r, c);
                if let Grapheme::Cluster(id) = d.grapheme {
                    prop_assert_eq!(
                        grid.cluster_str(id),
                        sg.cluster_str(id),
                        "cell {},{} cluster text diverged",
                        r,
                        c
                    );
                    prop_assert!(grid.cluster_str(id).is_some(), "daemon cluster {} unresolved", id);
                }
            }
        }
    }

    /// Pins: a permuted entry order (the rehydrate ships entry 100
    /// before 1-99) still converges on the daemon grid, and a row
    /// delivered before its entries is refused instead of drawing a
    /// hole.
    #[test]
    fn a_permuted_entry_order_converges_and_a_row_before_its_entries_is_refused(
        rows_input in proptest::collection::vec(
            proptest::collection::vec(cell_input(), 1..=12),
            1..=6,
        ),
        rotation in 0_usize..16,
    ) {
        let rows = u16::try_from(rows_input.len()).unwrap();
        let cols = u16::try_from(rows_input.iter().map(Vec::len).max().unwrap()).unwrap();

        let mut grid = Grid::new(rows, cols);
        let mut parser = Parser::new();
        let mut input = String::new();
        for (i, row) in rows_input.iter().enumerate() {
            if i > 0 {
                input.push_str("\r\n");
            }
            for (base, marks) in row {
                input.push(*base);
                for m in marks {
                    input.push(*m);
                }
            }
        }
        parser.advance(&mut grid, input.as_bytes());

        let entries = cluster_entries(&grid);
        let dirty: Vec<u16> = grid
            .damage()
            .dirty_rows()
            .map(|r| u16::try_from(r).unwrap())
            .collect();
        let encoded: Vec<(u16, Vec<u8>)> = dirty
            .iter()
            .map(|r| {
                let cells = grid.row_cells(*r).unwrap_or(&[]);
                let sized = grid.row_sized_cells(*r);
                let body = encode_row(
                    RowEncode {
                        cells,
                        pad_to: cells.len(),
                        sized_cells: &sized,
                        soft_wrap_continued: grid.row_soft_wrap_continued(*r),
                    },
                    grid.style_table(),
                )
                .unwrap();
                (*r, body)
            })
            .collect();

        // A row whose entries have not landed ends the attachment.
        let mut early = ShadowScreen::new(rows, cols);
        let names_a_cluster = (0..rows).any(|r| {
            (0..cols).any(|c| matches!(grid.cell(r, c).unwrap().grapheme, Grapheme::Cluster(_)))
        });
        let first_row = encoded.first().cloned();
        if names_a_cluster && let Some((r, body)) = first_row {
            let mut refused = false;
            for (row, body) in std::iter::once((r, body)).chain(
                encoded.iter().skip(1).map(|(r, b)| (*r, b.clone())),
            ) {
                if early
                    .apply(&GridMsg::RowDelta { rows: vec![(row, RowPayload(body))] })
                    .is_err()
                {
                    refused = true;
                    break;
                }
            }
            prop_assert!(refused, "a row naming an unsent cluster must be refused");
        }

        // The entries themselves may arrive in any order, holes and all.
        let mut shadow = ShadowScreen::new(rows, cols);
        let split = if entries.is_empty() { 0 } else { rotation % entries.len() };
        for (id, text) in entries.iter().skip(split).chain(entries.iter().take(split)) {
            shadow
                .apply(&GridMsg::Cluster { id: *id, text: text.clone() })
                .expect("cluster apply");
        }
        for (row, body) in encoded {
            shadow
                .apply(&GridMsg::RowDelta { rows: vec![(row, RowPayload(body))] })
                .expect("row delta apply");
        }

        let sg = shadow.screen();
        for r in 0..rows {
            for c in 0..cols {
                let d = grid.cell(r, c).unwrap();
                prop_assert_eq!(d, sg.cell(r, c).unwrap(), "cell {},{} diverged (handle)", r, c);
                if let Grapheme::Cluster(id) = d.grapheme {
                    prop_assert_eq!(
                        grid.cluster_str(id),
                        sg.cluster_str(id),
                        "cell {},{} cluster text diverged after the permuted fill",
                        r,
                        c
                    );
                }
            }
        }
    }
}

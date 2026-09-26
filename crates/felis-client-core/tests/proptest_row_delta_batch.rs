//! docs/reference/ipc.md `GridMsg::RowDelta` shadow-equivalence property:
//! one frame carrying N `(row, packed_cells)` entries must leave the
//! shadow identical to N one-entry frames.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_client_core::ShadowScreen;
use felis_grid::{Grapheme, Grid, RowEncode, encode_row};
use felis_protocol::{RowPayload, messages::GridMsg};
use felis_vt::Parser;
use proptest::prelude::*;

fn build_dirty_grid(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
    let mut grid = Grid::new(rows, cols);
    Parser::new().advance(&mut grid, bytes);
    grid
}

fn encode_dirty_rows(grid: &Grid) -> Vec<(u16, Vec<u8>)> {
    grid.damage()
        .dirty_rows()
        .map(|row_idx| {
            let r = u16::try_from(row_idx).unwrap();
            let cells: Vec<_> = (0..grid.cols())
                .map(|c| grid.cell(r, c).copied().unwrap_or_default())
                .collect();
            let sized = grid.row_sized_cells(r);
            (
                r,
                encode_row(
                    RowEncode {
                        cells: &cells,
                        pad_to: cells.len(),
                        sized_cells: &sized,
                        soft_wrap_continued: grid.row_soft_wrap_continued(r),
                    },
                    grid.style_table(),
                )
                .unwrap(),
            )
        })
        .collect()
}

/// The daemon ships every entry a row names before that row, and the
/// shadow refuses a row that names an absent one, so the property must
/// drive it in that order too.
fn registry_msgs(grid: &Grid) -> Vec<GridMsg> {
    let mut msgs = Vec::new();
    let mut links: Vec<u16> = Vec::new();
    let mut clusters: Vec<u32> = Vec::new();
    for r in 0..grid.rows() {
        for c in 0..grid.cols() {
            let Some(cell) = grid.cell(r, c) else {
                continue;
            };
            if let Some(id) = cell.link
                && !links.contains(&id.get())
                && let Some(entry) = grid.hyperlink(id)
            {
                links.push(id.get());
                msgs.push(GridMsg::Hyperlink {
                    id: id.get(),
                    anchor: entry.id.as_ref().map(|a| a.as_str().to_owned()),
                    uri: entry.uri.as_str().to_owned(),
                });
            }
            if let Grapheme::Cluster(id) = cell.grapheme
                && !clusters.contains(&id.get())
                && let Some(text) = grid.cluster_str(id)
            {
                clusters.push(id.get());
                msgs.push(GridMsg::Cluster {
                    id: id.get(),
                    text: text.to_owned(),
                });
            }
        }
    }
    msgs
}

proptest! {
    /// Pins: for any byte stream producing at least one dirty row, one
    /// batched `RowDelta` reaches the same cell state as one frame per row.
    #[test]
    fn batch_apply_matches_individual_apply(
        rows in 1u16..=24,
        cols in 1u16..=80,
        bytes in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let grid = build_dirty_grid(rows, cols, &bytes);
        let payloads = encode_dirty_rows(&grid);
        prop_assume!(!payloads.is_empty());

        let registry = registry_msgs(&grid);
        let mut shadow_a = ShadowScreen::new(rows, cols);
        for msg in &registry {
            shadow_a.apply(msg).expect("registry apply");
        }
        for (row, packed_cells) in &payloads {
            shadow_a
                .apply(&GridMsg::RowDelta {
                    rows: vec![(*row, RowPayload(packed_cells.clone()))],
                })
                .expect("individual apply");
        }

        let mut shadow_b = ShadowScreen::new(rows, cols);
        for msg in &registry {
            shadow_b.apply(msg).expect("registry apply");
        }
        shadow_b
            .apply(&GridMsg::RowDelta {
                rows: payloads
                    .into_iter()
                    .map(|(row, body)| (row, RowPayload(body)))
                    .collect(),
            })
            .expect("batch apply");

        let g_a = shadow_a.screen();
        let g_b = shadow_b.screen();
        prop_assert_eq!(g_a.rows(), g_b.rows());
        prop_assert_eq!(g_a.cols(), g_b.cols());
        for r in 0..g_a.rows() {
            for c in 0..g_a.cols() {
                prop_assert_eq!(
                    g_a.cell(r, c),
                    g_b.cell(r, c),
                    "row {} col {} diverged across batch vs individual apply",
                    r,
                    c
                );
            }
        }
    }

    /// Pins: an empty `RowDelta` leaves the shadow unchanged. An
    /// off-by-one in the ranged apply loop would still pass the previous
    /// property, which only sees inputs with at least one row.
    #[test]
    fn empty_batch_is_a_noop(
        rows in 1u16..=24,
        cols in 1u16..=80,
    ) {
        let mut shadow = ShadowScreen::new(rows, cols);
        let before: Vec<_> = (0..rows)
            .flat_map(|r| (0..cols).map(move |c| (r, c)))
            .map(|(r, c)| shadow.screen().cell(r, c).copied().unwrap_or_default())
            .collect();

        shadow
            .apply(&GridMsg::RowDelta { rows: vec![] })
            .expect("empty batch apply");

        let after: Vec<_> = (0..rows)
            .flat_map(|r| (0..cols).map(move |c| (r, c)))
            .map(|(r, c)| shadow.screen().cell(r, c).copied().unwrap_or_default())
            .collect();
        prop_assert_eq!(before, after);
    }
}

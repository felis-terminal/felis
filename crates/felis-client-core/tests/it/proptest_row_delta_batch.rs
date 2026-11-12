//! docs/reference/ipc.md `GridMsg::RowDelta` shadow-equivalence property:
//! one frame carrying N `(row, packed_cells)` entries must leave the
//! shadow identical to N one-entry frames.

use felis_client_core::ShadowScreen;
use felis_grid::Grid;
use felis_protocol::messages::GridMsg;
use felis_vt::Parser;
use proptest::prelude::*;

use crate::support::{encode_dirty_rows, visible_registry_msgs};

fn build_dirty_grid(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
    let mut grid = Grid::new(rows, cols);
    Parser::new().advance(&mut grid, bytes);
    grid
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

        let registry = visible_registry_msgs(&grid);
        let mut shadow_a = ShadowScreen::new(rows, cols);
        for msg in &registry {
            shadow_a.apply(msg).expect("registry apply");
        }
        for row in &payloads {
            shadow_a
                .apply(&GridMsg::RowDelta {
                    rows: vec![row.clone()],
                })
                .expect("individual apply");
        }

        let mut shadow_b = ShadowScreen::new(rows, cols);
        for msg in &registry {
            shadow_b.apply(msg).expect("registry apply");
        }
        shadow_b
            .apply(&GridMsg::RowDelta { rows: payloads })
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

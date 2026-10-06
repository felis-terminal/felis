//! Properties for the `Placements::remove_where` filter primitive and
//! the `Placement::contains_*` geometric helpers. Spec source:
//! `docs/reference/protocols/kitty-graphics.md` "Lifecycle" and
//! `docs/reference/protocols/support-matrix.md`
//! "Display / lifecycle (`a=`)" (the extended `a=d` modes).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::images::{CellPos, ImageId, Placement, PlacementId, Placements};
use proptest::prelude::*;

/// Bounds keep `anchor + extent` inside `u16` so the helpers' saturating
/// math never engages.
fn placement_strategy() -> impl Strategy<Value = Placement> {
    (
        1u32..16,
        prop::option::of(0u32..8),
        1u16..200,
        1u16..200,
        1u16..20,
        1u16..20,
        -8i32..=8,
        any::<bool>(),
    )
        .prop_map(
            |(image, placement, row, col, rows, cols, z, no_cursor_move)| Placement {
                image_id: ImageId(image),
                placement_id: placement.map(PlacementId),
                anchor: CellPos {
                    row: i32::from(row),
                    col,
                },
                cols,
                rows,
                requested_cols: cols,
                requested_rows: rows,
                source: None,
                z_index: z,
                no_cursor_move,
                quiet: 0,
            },
        )
}

fn populate(table: &mut Placements, items: &[Placement]) {
    for p in items {
        table.upsert(p.clone());
    }
}

proptest! {
    #[test]
    fn remove_where_partitions_the_table(
        items in proptest::collection::vec(placement_strategy(), 0..16),
        threshold in -8i32..=8,
    ) {
        let mut table = Placements::new();
        populate(&mut table, &items);
        let original_len = table.len();
        let original_keys: Vec<_> = table
            .iter()
            .map(|p| (p.image_id, p.placement_id))
            .collect();

        let removed = table.remove_where(|p| p.z_index >= threshold);

        prop_assert!(removed.iter().all(|p| p.z_index >= threshold));
        prop_assert!(table.iter().all(|p| p.z_index < threshold));
        prop_assert_eq!(removed.len() + table.len(), original_len);

        let mut got: Vec<_> = removed
            .iter()
            .chain(table.iter())
            .map(|p| (p.image_id, p.placement_id))
            .collect();
        got.sort_by_key(|(i, p)| (i.0, p.map(|p| p.0)));
        let mut want = original_keys;
        want.sort_by_key(|(i, p)| (i.0, p.map(|p| p.0)));
        // De-dup `want` because `upsert` collapses repeated keys.
        want.dedup();
        prop_assert_eq!(got, want);
    }

    #[test]
    fn remove_where_match_all_drains(
        items in proptest::collection::vec(placement_strategy(), 0..16),
    ) {
        let mut table = Placements::new();
        populate(&mut table, &items);
        let expected_len = table.len();
        let removed = table.remove_where(|_| true);
        prop_assert_eq!(removed.len(), expected_len);
        prop_assert!(table.is_empty());
    }

    #[test]
    fn contains_cell_agrees_with_row_and_col_helpers(
        p in placement_strategy(),
        query_row in 0u16..220,
        query_col in 0u16..220,
    ) {
        let actual = p.contains_cell(query_row, query_col);
        let composed = p.contains_row(query_row) && p.contains_col(query_col);
        prop_assert_eq!(actual, composed);
    }

    #[test]
    fn every_cell_inside_the_box_matches(
        p in placement_strategy(),
        row_offset in 0u16..20,
        col_offset in 0u16..20,
    ) {
        prop_assume!(row_offset < p.rows && col_offset < p.cols);
        let row = u16::try_from(p.anchor.row).unwrap() + row_offset;
        let col = p.anchor.col + col_offset;
        prop_assert!(p.contains_cell(row, col));
    }

    #[test]
    fn cells_outside_the_box_never_match(
        p in placement_strategy(),
        delta in 1u16..50,
    ) {
        let below = p.anchor.row + i32::from(p.rows) + i32::from(delta);
        prop_assert!(!p.contains_row(u16::try_from(below).unwrap()));
        if p.anchor.row > i32::from(delta) {
            let above = p.anchor.row - i32::from(delta);
            prop_assert!(!p.contains_row(u16::try_from(above).unwrap()));
        }
        let right = p.anchor.col.saturating_add(p.cols).saturating_add(delta);
        prop_assert!(!p.contains_col(right));
        if p.anchor.col > delta {
            let left = p.anchor.col - delta;
            prop_assert!(!p.contains_col(left));
        }
    }
}

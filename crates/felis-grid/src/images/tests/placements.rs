use super::*;

#[test]
fn fresh_placements_table_is_empty() {
    let t = Placements::new();
    assert!(t.is_empty());
    assert_eq!(t.len(), 0);
    assert!(t.iter().next().is_none());
}

#[test]
fn upsert_adds_then_iter_in_insertion_order() {
    let mut t = Placements::new();
    t.upsert(placement(1, Some(10), 0));
    t.upsert(placement(2, None, 0));
    t.upsert(placement(1, Some(20), 0));
    let ids: Vec<_> = t
        .iter()
        .map(|p| (p.image_id.0, p.placement_id.map(|p| p.0)))
        .collect();
    assert_eq!(ids, vec![(1, Some(10)), (2, None), (1, Some(20))]);
}

#[test]
fn upsert_replaces_existing_key() {
    let mut t = Placements::new();
    t.upsert(placement(1, Some(10), 0));
    t.upsert(placement(1, Some(10), 5)); // same key, new z
    assert_eq!(t.len(), 1);
    assert_eq!(t.iter().next().unwrap().z_index, 5);
}

#[test]
fn remove_returns_the_removed_entry() {
    let mut t = Placements::new();
    t.upsert(placement(1, Some(10), 0));
    let removed = t.remove(ImageId(1), Some(PlacementId(10))).unwrap();
    assert_eq!(removed.image_id, ImageId(1));
    assert!(t.is_empty());
    assert!(t.remove(ImageId(1), Some(PlacementId(10))).is_none());
}

#[test]
fn remove_distinguishes_default_placement_from_keyed() {
    // `(image=1, placement=None)` and `(image=1, placement=Some(0))`
    // are different keys per the spec.
    let mut t = Placements::new();
    t.upsert(placement(1, None, 0));
    t.upsert(placement(1, Some(0), 0));
    assert_eq!(t.len(), 2);
    t.remove(ImageId(1), None).unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t.iter().next().unwrap().placement_id, Some(PlacementId(0)));
}

#[test]
fn delete_image_removes_every_placement_of_that_image() {
    let mut t = Placements::new();
    t.upsert(placement(1, Some(1), 0));
    t.upsert(placement(2, Some(1), 0));
    t.upsert(placement(1, Some(2), 0));
    t.upsert(placement(1, None, 0));
    let removed = t.delete_image(ImageId(1));
    assert_eq!(removed.len(), 3);
    let kept: Vec<_> = t.iter().map(|p| p.image_id).collect();
    assert_eq!(kept, vec![ImageId(2)]);
}

fn virtual_placement(image: u32, placement: u32, cols: u16) -> VirtualPlacement {
    VirtualPlacement {
        image_id: ImageId(image),
        placement_id: Some(PlacementId(placement)),
        anonymous: false,
        cols,
        rows: 2,
        z_index: 0,
        serial: 0,
    }
}

#[test]
fn upsert_virtual_keeps_one_extent_per_placement_id() {
    let mut t = Placements::new();
    t.upsert_virtual(virtual_placement(1, 1, 4));
    t.upsert_virtual(virtual_placement(2, 1, 6));
    t.upsert_virtual(virtual_placement(1, 2, 5));
    t.upsert_virtual(virtual_placement(1, 1, 8));
    let extents: Vec<_> = t.iter_virtual().map(|v| (v.image_id, v.cols)).collect();
    assert_eq!(
        extents,
        vec![(ImageId(1), 8), (ImageId(2), 6), (ImageId(1), 5)]
    );
}

#[test]
fn a_virtual_placement_without_an_id_replaces_every_extent_of_its_image() {
    let mut t = Placements::new();
    t.upsert_virtual(virtual_placement(1, 1, 4));
    t.upsert_virtual(VirtualPlacement {
        placement_id: None,
        ..virtual_placement(1, 0, 8)
    });
    let extents: Vec<_> = t.iter_virtual().map(|v| v.cols).collect();
    assert_eq!(extents, vec![8]);
}

#[test]
fn a_cell_picks_its_placement_or_the_image_s_first() {
    let mut t = Placements::new();
    t.upsert_virtual(virtual_placement(1, 1, 4));
    t.upsert_virtual(virtual_placement(1, 2, 6));
    assert_eq!(t.virtual_for_cell(ImageId(1), 2).map(|v| v.cols), Some(6));
    assert_eq!(t.virtual_for_cell(ImageId(1), 0).map(|v| v.cols), Some(4));
    assert_eq!(t.virtual_for_cell(ImageId(1), 3), None);
}

#[test]
fn an_anonymous_id_skips_the_ids_virtual_placements_hold() {
    let mut t = Placements::new();
    t.upsert_virtual(virtual_placement(1, u32::MAX, 4));
    assert_eq!(
        t.allocate_anonymous_id(ImageId(1)),
        PlacementId(u32::MAX - 1)
    );
}

#[test]
fn virtual_placements_live_and_die_apart_from_anchored_entries() {
    let mut t = Placements::new();
    t.upsert(placement(1, Some(1), 0));
    t.upsert_virtual(virtual_placement(1, 1, 4));
    t.delete_image(ImageId(1));
    assert_eq!(t.iter_virtual().count(), 1);
    assert!(!t.is_empty(), "a virtual extent alone keeps the table live");
    t.upsert(placement(1, Some(1), 0));
    t.remove_virtual(ImageId(1));
    assert_eq!(t.iter_virtual().count(), 0);
    assert_eq!(t.len(), 1);
}

#[test]
fn a_put_past_the_cap_evicts_the_oldest_placement_of_either_kind() {
    let mut t = Placements::with_cap(3);
    t.upsert_virtual(virtual_placement(1, 1, 4));
    assert_eq!(t.upsert(placement(2, Some(1), 0)), None);
    assert_eq!(t.upsert(placement(3, Some(1), 0)), None);
    assert_eq!(
        t.upsert(placement(4, Some(1), 0)),
        Some(Displaced::Evicted(AnyPlacement::Virtual(
            VirtualPlacement {
                serial: 1,
                ..virtual_placement(1, 1, 4)
            }
        )))
    );
    assert_eq!(
        t.upsert_virtual(virtual_placement(5, 1, 4)),
        Some(AnyPlacement::Anchored(Placement {
            serial: 2,
            ..placement(2, Some(1), 0)
        }))
    );
    let anchored: Vec<_> = t.iter().map(|p| p.image_id.0).collect();
    assert_eq!(anchored, vec![3, 4]);
    assert_eq!(t.iter_virtual().count(), 1);
}

#[test]
fn a_replacing_put_at_the_cap_keeps_its_place_and_evicts_nothing() {
    let mut t = Placements::with_cap(2);
    t.upsert(placement(1, Some(1), 0));
    t.upsert(placement(2, Some(1), 0));
    assert_eq!(
        t.upsert(placement(1, Some(1), 5)),
        Some(Displaced::Replaced(Placement {
            serial: 1,
            ..placement(1, Some(1), 0)
        }))
    );
    t.upsert_virtual(virtual_placement(3, 1, 4));
    t.upsert_virtual(virtual_placement(3, 1, 6));
    let anchored: Vec<_> = t.iter().map(|p| (p.image_id.0, p.z_index)).collect();
    assert_eq!(
        anchored,
        vec![(2, 0)],
        "the replaced placement stayed oldest"
    );
}

#[test]
fn a_rekeyed_anonymous_placement_becomes_the_newest() {
    let mut t = Placements::with_cap(2);
    let id = t.allocate_anonymous_id(ImageId(1));
    t.upsert(Placement {
        anonymous: true,
        ..placement(1, Some(id.0), 0)
    });
    t.upsert(placement(2, Some(1), 0));
    t.rekey_anonymous(ImageId(1), id);
    assert_eq!(
        t.upsert(placement(3, Some(1), 0)),
        Some(Displaced::Evicted(AnyPlacement::Anchored(Placement {
            serial: 2,
            ..placement(2, Some(1), 0)
        })))
    );
}

#[test]
fn for_image_filters_placements_by_image() {
    let mut t = Placements::new();
    t.upsert(placement(1, Some(1), 0));
    t.upsert(placement(2, Some(1), 0));
    t.upsert(placement(1, Some(2), 0));
    let mine: Vec<_> = t.for_image(ImageId(1)).collect();
    assert_eq!(mine.len(), 2);
    assert!(mine.iter().all(|p| p.image_id == ImageId(1)));
}

fn placement_at_row(image: u32, row: i32) -> Placement {
    let mut p = placement(image, None, 0);
    p.anchor = CellPos { row, col: 1 };
    p
}

#[test]
fn shift_up_zero_lines_is_a_noop() {
    let mut t = Placements::new();
    t.upsert(placement_at_row(1, 10));
    let removed = t.shift_up(0, 0);
    assert_eq!(removed, Vec::<Placement>::new());
    assert_eq!(t.iter().next().unwrap().anchor.row, 10);
}

#[test]
fn shift_up_decrements_anchor_row() {
    let mut t = Placements::new();
    t.upsert(placement_at_row(1, 10));
    let removed = t.shift_up(3, 0);
    assert_eq!(removed, Vec::<Placement>::new());
    assert_eq!(t.iter().next().unwrap().anchor.row, 7);
}

#[test]
fn shift_up_with_zero_retention_evicts_placements_leaving_the_live_area() {
    let mut t = Placements::new();
    t.upsert(placement_at_row(1, 5));
    t.upsert(placement_at_row(2, 20));
    let removed = t.shift_up(10, 0);
    // Image 1 leaves the live area with no retention granted; image 2
    // survives at row 10.
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].image_id, ImageId(1));
    let kept: Vec<_> = t.iter().collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].image_id, ImageId(2));
    assert_eq!(kept[0].anchor.row, 10);
}

#[test]
fn remap_rows_rewrites_each_anchor_and_evicts_on_none() {
    let mut t = Placements::new();
    t.upsert(placement_at_row(1, 3));
    t.upsert(placement_at_row(2, 7));
    // row 3 → -1, row 7 → evicted.
    let removed = t.remap_rows(|row| if row == 3 { Some(-1) } else { None });
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].image_id, ImageId(2));
    let kept: Vec<_> = t.iter().collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].image_id, ImageId(1));
    assert_eq!(kept[0].anchor.row, -1);
}

#[test]
fn shift_up_equal_to_anchor_evicts_it_without_retention() {
    // The anchor lands at row 0, above the live area; the alt screen
    // passes retain = 0, so anchors scrolling off it drop immediately.
    let mut t = Placements::new();
    t.upsert(placement_at_row(1, 7));
    let removed = t.shift_up(7, 0);
    assert_eq!(removed.len(), 1);
    assert!(t.is_empty());
}

/// `docs/reference/protocols/kitty-graphics.md` "Scrollback-anchored
/// placements": a user scrolling back re-sees the image exactly where
/// its text went.
#[test]
fn shift_up_retains_scrolled_out_placements_within_scrollback() {
    let mut t = Placements::new();
    t.upsert(placement_at_row(1, 5));
    let removed = t.shift_up(10, /* retain */ 100);
    assert!(removed.is_empty(), "anchor at -5 is well within retention");
    assert_eq!(t.iter().next().unwrap().anchor.row, -5);
}

/// Retention is bounded by the scrollback, not infinite.
#[test]
fn shift_up_evicts_anchor_scrolled_past_retention() {
    let mut t = Placements::new();
    // Retention 3: a shift of 4 lands the anchor at -3, past the -2
    // horizon; a shift of 3 stays.
    t.upsert(placement_at_row(1, 1));
    let removed = t.shift_up(3, 3);
    assert!(removed.is_empty(), "row -2 is the oldest retained line");
    assert_eq!(t.iter().next().unwrap().anchor.row, -2);
    let removed = t.shift_up(1, 3);
    assert_eq!(removed.len(), 1, "row -3 is past the 3-line retention");
    assert!(t.is_empty());
}

/// `d=y` deletes and ED erasures address live coordinates, so a
/// straddling placement must keep matching the live rows its body
/// covers.
#[test]
fn contains_row_matches_live_rows_for_straddling_placement() {
    // Anchor at -2, 6 rows tall: covers rows -2..=3.
    let mut p = placement_at_row(1, -2);
    p.rows = cells(6);
    assert!(p.contains_row(1));
    assert!(p.contains_row(3));
    assert!(!p.contains_row(4));
}

fn placement_at(image: u32, row_1based: i32, rows_high: u16, no_cursor: bool) -> Placement {
    let mut p = placement(image, None, 0);
    p.anchor = CellPos {
        row: row_1based,
        col: 1,
    };
    p.rows = cells(rows_high);
    p.cols = cells(8);
    p.no_cursor_move = no_cursor;
    p
}

#[test]
fn a_screen_clear_drops_every_placement_with_a_row_on_screen() {
    let mut t = Placements::new();
    t.upsert(placement_at(1, 3, 4, false));
    t.upsert(placement_at(2, 20, 1, /* no_cursor_move */ true));
    t.upsert(placement_at(3, -1, 3, false)); // rows -1..=1: the last is the top live row
    t.upsert(placement_at(4, -9, 4, false)); // rows -9..=-6, all in scrollback
    let removed: Vec<u32> = t
        .clear(ImageClear::Screen)
        .iter()
        .map(|p| p.image_id.0)
        .collect();
    assert_eq!(removed, vec![1, 2, 3]);
    assert_eq!(t.iter().map(|p| p.image_id.0).collect::<Vec<_>>(), vec![4]);
}

#[test]
fn a_full_clear_also_drops_scrollback_placements() {
    let mut t = Placements::new();
    t.upsert(placement_at(1, 3, 4, false));
    t.upsert(placement_at(4, -9, 4, false));
    assert_eq!(t.clear(ImageClear::All).len(), 2);
    assert!(t.is_empty());
}

fn placement_box(
    image: u32,
    placement: Option<u32>,
    row_1based: i32,
    col_1based: u16,
    rows: u16,
    cols: u16,
    z: i32,
) -> Placement {
    Placement {
        image_id: ImageId(image),
        placement_id: placement.map(PlacementId),
        anchor: CellPos {
            row: row_1based,
            col: col_1based,
        },
        cols: cells(cols),
        rows: cells(rows),
        source: None,
        z_index: z,
        no_cursor_move: false,
        quiet: 0,
        anonymous: false,
        serial: 0,
    }
}

/// `remove_where` is the filter primitive the extended `a=d` modes ride
/// on: matches in insertion order, the rest keep their relative order.
#[test]
fn remove_where_returns_matches_in_insertion_order_and_preserves_kept_order() {
    let mut t = Placements::new();
    t.upsert(placement_box(1, None, 1, 1, 2, 2, 0)); // keep
    t.upsert(placement_box(2, None, 5, 5, 2, 2, 0)); // match
    t.upsert(placement_box(3, None, 1, 1, 2, 2, 0)); // keep
    t.upsert(placement_box(4, None, 5, 5, 2, 2, 0)); // match
    let removed = t.remove_where(|p| p.image_id == ImageId(2) || p.image_id == ImageId(4));
    let removed_ids: Vec<_> = removed.iter().map(|p| p.image_id.0).collect();
    assert_eq!(removed_ids, vec![2, 4]);
    let kept_ids: Vec<_> = t.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept_ids, vec![1, 3]);
}

#[test]
fn remove_where_match_all_drains_the_table() {
    let mut t = Placements::new();
    t.upsert(placement_box(1, None, 1, 1, 1, 1, 0));
    t.upsert(placement_box(2, None, 2, 2, 1, 1, 0));
    let removed = t.remove_where(|_| true);
    assert_eq!(removed.len(), 2);
    assert!(t.is_empty());
}

#[test]
fn remove_where_match_none_is_a_noop() {
    let mut t = Placements::new();
    t.upsert(placement_box(1, None, 1, 1, 1, 1, 0));
    let removed = t.remove_where(|_| false);
    assert_eq!(removed, Vec::<Placement>::new());
    assert_eq!(t.len(), 1);
}

/// The geometric primitives for `d=x` / `d=y`.
#[test]
fn contains_col_and_contains_row_match_the_full_extent() {
    let p = placement_box(1, None, 5, 10, 3, 5, 0);
    assert!(p.contains_col(10));
    assert!(p.contains_col(14));
    assert!(!p.contains_col(9));
    assert!(!p.contains_col(15));

    assert!(p.contains_row(5));
    assert!(p.contains_row(7));
    assert!(!p.contains_row(4));
    assert!(!p.contains_row(8));
}

/// The `&&` in the position predicate flipped to `||` would let a
/// mismatched-placement-id removal pull the wrong entry.
#[test]
fn placements_remove_requires_both_image_and_placement_id_to_match() {
    let mut t = Placements::new();
    t.upsert(placement(1, Some(10), 0));
    t.upsert(placement(1, Some(20), 0));
    assert!(t.remove(ImageId(1), Some(PlacementId(99))).is_none());
    assert_eq!(t.len(), 2);
    let removed = t.remove(ImageId(1), Some(PlacementId(20))).unwrap();
    assert_eq!(removed.placement_id, Some(PlacementId(20)));
    assert_eq!(t.len(), 1);
    assert_eq!(t.iter().next().unwrap().placement_id, Some(PlacementId(10)));
}

/// Catches the constant `true` body mutant.
#[test]
fn placements_is_empty_is_false_after_upsert() {
    let mut t = Placements::new();
    t.upsert(placement(1, None, 0));
    assert!(!t.is_empty());
    assert_eq!(t.len(), 1);
}

#[test]
fn a_replaced_placement_keeps_its_serial_and_a_new_key_takes_the_next() {
    let mut t = Placements::new();
    t.upsert(placement(1, Some(1), 0));
    t.upsert(placement(1, Some(2), 0));
    t.upsert(placement(1, Some(1), 4));
    assert_eq!(t.serial(ImageId(1), Some(PlacementId(1))), Some(1));
    assert_eq!(t.serial(ImageId(1), Some(PlacementId(2))), Some(2));
    t.upsert_virtual(virtual_placement(1, 3, 4));
    t.upsert_virtual(virtual_placement(1, 3, 6));
    assert_eq!(t.serial(ImageId(1), Some(PlacementId(3))), Some(3));
}

#[test]
fn a_rekeyed_anonymous_placement_keeps_its_serial() {
    let mut t = Placements::new();
    let id = t.allocate_anonymous_id(ImageId(1));
    t.upsert(Placement {
        anonymous: true,
        ..placement(1, Some(id.0), 0)
    });
    t.upsert(placement(1, Some(1), 0));
    let Some(AnyPlacement::Anchored(moved)) = t.rekey_anonymous(ImageId(1), id) else {
        panic!("the anonymous placement moves");
    };
    assert_eq!(moved.serial, 1);
    assert_eq!(t.serial(ImageId(1), moved.placement_id), Some(1));
}

/// What a put converting a placement to the other kind relies on.
#[test]
fn a_new_placement_keeps_a_serial_it_arrives_with() {
    let mut t = Placements::new();
    t.upsert_virtual(virtual_placement(1, 1, 4));
    t.upsert(placement(1, Some(2), 0));
    let inherited = t.serial(ImageId(1), Some(PlacementId(1))).unwrap();
    t.remove_virtual_placement(ImageId(1), PlacementId(1));
    t.upsert(Placement {
        serial: inherited,
        ..placement(1, Some(1), 0)
    });
    assert_eq!(t.serial(ImageId(1), Some(PlacementId(1))), Some(1));
    assert_eq!(t.serial(ImageId(1), Some(PlacementId(2))), Some(2));
}

#[cfg(feature = "state-dump")]
#[test]
fn a_dump_without_serials_takes_them_from_its_sequence_numbers() {
    let mut t = Placements::new();
    t.upsert(placement(1, Some(1), 0));
    t.upsert_virtual(virtual_placement(1, 2, 4));
    t.upsert(placement(2, Some(1), 0));
    let mut dump = PlacementsDump::from(t);
    for p in &mut dump.entries {
        p.serial = 0;
    }
    for v in &mut dump.virtuals {
        v.serial = 0;
    }
    let restored = Placements::from(dump);
    assert_eq!(restored.serial(ImageId(1), Some(PlacementId(1))), Some(1));
    assert_eq!(restored.serial(ImageId(1), Some(PlacementId(2))), Some(2));
    assert_eq!(restored.serial(ImageId(2), Some(PlacementId(1))), Some(3));
}

/// Entries restored from a dump that predates serials all hold 0; the
/// newest of a number is then the last in store order.
#[test]
fn newest_with_number_breaks_serial_ties_by_store_order() {
    let s = ImageStore::over_cap_for_test(
        vec![
            (ImageId(3), entry(1).with_number(7)),
            (ImageId(1), entry(1).with_number(7)),
            (ImageId(2), entry(1).with_number(8)),
        ],
        charge(1) * 3,
    );
    assert_eq!(s.newest_with_number(7), Some(ImageId(1)));
    assert_eq!(s.newest_with_number(8), Some(ImageId(2)));
    assert_eq!(s.newest_with_number(9), None);
}

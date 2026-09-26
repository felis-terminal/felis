use crate::test_support::{apc_bodies, drive};
use crate::*;
use felis_vt::Parser;

use super::*;

fn entry(size: usize) -> ImageEntry {
    ImageEntry::new(1, 1, ImageFormat::Rgb24, vec![0u8; size])
}

fn frame_of(size: usize) -> Frame {
    Frame {
        pixels: vec![0u8; size].into(),
        gap_ms: 40,
    }
}

/// Caps and expected totals are written through the charge rather than
/// as pixel counts, so they keep meaning what they say if the charge
/// changes.
fn charge(size: usize) -> usize {
    entry(size).byte_len()
}

fn frame_charge(size: usize) -> usize {
    frame_of(size).byte_len()
}

#[test]
fn fresh_store_is_empty() {
    let s = ImageStore::new(1024);
    assert!(s.is_empty());
    assert_eq!(s.len(), 0);
    assert_eq!(s.bytes_used(), 0);
    assert_eq!(s.bytes_cap(), 1024);
    assert!(s.get(ImageId(1)).is_none());
}

#[test]
fn insert_and_get_round_trips() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(7), entry(64)).unwrap();
    let got = s.get(ImageId(7)).unwrap();
    assert_eq!(got.byte_len(), charge(64));
    assert_eq!(got.refcount(), 0);
    assert_eq!(s.bytes_used(), charge(64));
}

#[test]
fn insert_replacing_same_id_overwrites_and_keeps_refcount_reset() {
    // Producers re-transmit under the same id; the dispatcher
    // invalidates existing placements, so the refcount resets.
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(1), entry(100)).unwrap();
    assert!(s.retain(ImageId(1)));
    assert_eq!(s.get(ImageId(1)).unwrap().refcount(), 1);

    s.insert(ImageId(1), entry(50)).unwrap();
    let after = s.get(ImageId(1)).unwrap();
    assert_eq!(after.byte_len(), charge(50));
    assert_eq!(after.refcount(), 0);
    assert_eq!(s.bytes_used(), charge(50));
}

#[test]
fn retain_and_release_track_refcount() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(2), entry(10)).unwrap();
    assert!(s.retain(ImageId(2)));
    assert!(s.retain(ImageId(2)));
    assert_eq!(s.get(ImageId(2)).unwrap().refcount(), 2);
    assert!(s.release(ImageId(2)));
    assert_eq!(s.get(ImageId(2)).unwrap().refcount(), 1);
    assert!(s.release(ImageId(2)));
    assert_eq!(s.get(ImageId(2)).unwrap().refcount(), 0);
    // Releasing past zero is a logic bug: `false` lets the dispatcher
    // log instead of silently underflowing.
    assert!(!s.release(ImageId(2)));
}

#[test]
fn retain_and_release_on_unknown_id_return_false() {
    let mut s = ImageStore::new(1024);
    assert!(!s.retain(ImageId(99)));
    assert!(!s.release(ImageId(99)));
}

#[test]
fn delete_removes_entry_and_frees_bytes() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(3), entry(80)).unwrap();
    assert_eq!(s.bytes_used(), charge(80));
    let removed = s.delete(ImageId(3)).unwrap();
    assert_eq!(removed.byte_len(), charge(80));
    assert!(s.is_empty());
    assert_eq!(s.bytes_used(), 0);
    assert!(s.delete(ImageId(3)).is_none());
}

#[test]
fn delete_removes_pinned_entries_too() {
    // a=d is the producer's explicit "drop this"; pin status does not
    // save it.
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(4), entry(20)).unwrap();
    s.retain(ImageId(4));
    assert!(s.delete(ImageId(4)).is_some());
    assert!(s.is_empty());
}

#[test]
fn insert_larger_than_cap_is_rejected() {
    let mut s = ImageStore::new(64);
    let err = s.insert(ImageId(1), entry(128)).unwrap_err();
    let InsertError::OverCapacity { wanted, cap } = err;
    assert_eq!(wanted, charge(128));
    assert_eq!(cap, 64);
    assert!(s.is_empty());
}

#[test]
fn insert_evicts_oldest_unpinned_entries_to_fit() {
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.insert(ImageId(2), entry(40)).unwrap();
    s.insert(ImageId(3), entry(40)).unwrap();
    assert!(s.get(ImageId(1)).is_none());
    assert!(s.get(ImageId(2)).is_some());
    assert!(s.get(ImageId(3)).is_some());
    assert_eq!(s.bytes_used(), 2 * charge(40));
}

#[test]
fn pinned_entries_are_skipped_during_eviction() {
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.retain(ImageId(1));
    s.insert(ImageId(2), entry(40)).unwrap();
    // id=1 is pinned, so id=2 goes even though it is newer.
    s.insert(ImageId(3), entry(40)).unwrap();
    assert!(s.get(ImageId(1)).is_some());
    assert!(s.get(ImageId(2)).is_none());
    assert!(s.get(ImageId(3)).is_some());
}

#[test]
fn insert_fails_when_all_entries_are_pinned() {
    // The store cannot make room without dropping live data, so the
    // dispatcher gets OverCapacity (ENOTSUP / EBADF) rather than an
    // evicted placed image.
    let mut s = ImageStore::new(2 * charge(50));
    s.insert(ImageId(1), entry(50)).unwrap();
    s.retain(ImageId(1));
    s.insert(ImageId(2), entry(50)).unwrap();
    s.retain(ImageId(2));
    let err = s.insert(ImageId(3), entry(40)).unwrap_err();
    assert!(matches!(err, InsertError::OverCapacity { .. }));
    assert!(s.get(ImageId(1)).is_some());
    assert!(s.get(ImageId(2)).is_some());
    assert!(s.get(ImageId(3)).is_none());
}

#[test]
fn a_refused_replacement_leaves_the_old_entry_in_the_store() {
    // Both entries are pinned, so the replacement cannot be made to
    // fit. The store must come out of the refusal unchanged: id=1 is
    // still there, at its old size, and still counted.
    let mut s = ImageStore::new(charge(60) + charge(40));
    s.insert(ImageId(1), entry(60)).unwrap();
    s.retain(ImageId(1));
    s.insert(ImageId(2), entry(40)).unwrap();
    s.retain(ImageId(2));

    let err = s.insert(ImageId(1), entry(100)).unwrap_err();

    assert!(matches!(err, InsertError::OverCapacity { .. }));
    assert_eq!(s.get(ImageId(1)).unwrap().byte_len(), charge(60));
    assert_eq!(s.bytes_used(), charge(60) + charge(40));
}

/// The frame cap is a count, so the byte cap says nothing about it:
/// this store has room for orders of magnitude more four-byte frames
/// than the cap admits. The refusal must land on the frame that would
/// be the 4097th, and leave the image whole.
#[test]
fn the_frame_cap_admits_its_last_frame_and_refuses_the_next() {
    let mut s = ImageStore::new(usize::MAX);
    s.insert(ImageId(1), entry(4)).unwrap();
    // The root counts toward the cap, so the pushes stop one short.
    for _ in 1..MAX_IMAGE_FRAMES {
        s.push_frame(ImageId(1), frame_of(4)).unwrap();
    }
    assert_eq!(s.get(ImageId(1)).unwrap().frame_count(), MAX_IMAGE_FRAMES);

    let err = s.push_frame(ImageId(1), frame_of(4)).unwrap_err();

    assert_eq!(
        err,
        FrameError::TooManyFrames {
            cap: MAX_IMAGE_FRAMES
        }
    );
    assert_eq!(
        s.get(ImageId(1)).unwrap().frame_count(),
        MAX_IMAGE_FRAMES,
        "a refusal must not have grown the image",
    );
}

#[test]
fn bytes_used_tracks_inserts_replaces_and_deletes() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(1), entry(100)).unwrap();
    s.insert(ImageId(2), entry(50)).unwrap();
    assert_eq!(s.bytes_used(), charge(100) + charge(50));
    s.insert(ImageId(1), entry(80)).unwrap(); // replace
    assert_eq!(s.bytes_used(), charge(80) + charge(50));
    s.delete(ImageId(2));
    assert_eq!(s.bytes_used(), charge(80));
}

#[test]
fn replacement_evicts_other_entries_if_needed() {
    // Existing id=1 is dropped first, but the replacement still does not
    // fit beside id=2, so id=2 gives up its slot.
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.insert(ImageId(2), entry(40)).unwrap();
    s.insert(ImageId(1), entry(70)).unwrap();
    assert!(s.get(ImageId(1)).is_some());
    assert_eq!(s.get(ImageId(1)).unwrap().byte_len(), charge(70));
    assert!(s.get(ImageId(2)).is_none());
}

fn placement(image: u32, placement: Option<u32>, z: i32) -> Placement {
    Placement {
        image_id: ImageId(image),
        placement_id: placement.map(PlacementId),
        anchor: CellPos { row: 1, col: 1 },
        cols: 0,
        rows: 0,
        source: None,
        z_index: z,
        no_cursor_move: false,
        quiet: 0,
    }
}

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

fn virtual_placement(image: u32, cols: u16) -> VirtualPlacement {
    VirtualPlacement {
        image_id: ImageId(image),
        cols,
        rows: 2,
        z_index: 0,
    }
}

#[test]
fn upsert_virtual_keeps_one_extent_per_image_id() {
    // A U=1 re-emission re-describes the same extent; accumulating
    // would let a rehydrate replay a stale tile size beside the live
    // one.
    let mut t = Placements::new();
    t.upsert_virtual(virtual_placement(1, 4));
    t.upsert_virtual(virtual_placement(2, 6));
    t.upsert_virtual(virtual_placement(1, 8));
    let extents: Vec<_> = t.iter_virtual().map(|v| (v.image_id, v.cols)).collect();
    assert_eq!(extents, vec![(ImageId(1), 8), (ImageId(2), 6)]);
}

#[test]
fn virtual_placements_live_and_die_apart_from_anchored_entries() {
    // A virtual placement's lifetime is its image's, not any placement
    // id's.
    let mut t = Placements::new();
    t.upsert(placement(1, Some(1), 0));
    t.upsert_virtual(virtual_placement(1, 4));
    t.delete_image(ImageId(1));
    assert_eq!(t.iter_virtual().count(), 1);
    assert!(!t.is_empty(), "a virtual extent alone keeps the table live");
    t.upsert(placement(1, Some(1), 0));
    t.remove_virtual(ImageId(1));
    assert_eq!(t.iter_virtual().count(), 0);
    assert_eq!(t.len(), 1);
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
    p.rows = 6;
    assert!(p.contains_row(1));
    assert!(p.contains_row(3));
    assert!(!p.contains_row(4));
}

/// The scrolled-out part grants no immunity from a live-region erase.
#[test]
fn remove_intersecting_evicts_straddling_placement() {
    let mut t = Placements::new();
    let mut p = placement_at_row(1, -2);
    p.rows = 6; // covers live rows 1..=3 (0-based 0..=2)
    t.upsert(p);
    let removed = t.remove_intersecting(0, 23, /* force */ false);
    assert_eq!(removed.len(), 1);
    assert!(
        t.is_empty(),
        "the reported eviction must actually leave the table",
    );
}

/// `clear` wipes the screen, not the history.
#[test]
fn remove_intersecting_spares_fully_scrolled_out_placement() {
    let mut t = Placements::new();
    let mut p = placement_at_row(1, -9);
    p.rows = 4; // covers rows -9..=-6, all in scrollback
    t.upsert(p);
    let removed = t.remove_intersecting(0, 23, false);
    assert_eq!(removed, Vec::<Placement>::new());
    assert_eq!(t.len(), 1);
}

fn placement_at(image: u32, row_1based: i32, rows_high: u16, no_cursor: bool) -> Placement {
    let mut p = placement(image, None, 0);
    p.anchor = CellPos {
        row: row_1based,
        col: 1,
    };
    p.rows = rows_high;
    p.cols = 8;
    p.no_cursor_move = no_cursor;
    p
}

/// Kitty graphics: every placement overlapping an `ED 2` range is
/// removed unless flagged `C=1`.
#[test]
fn remove_intersecting_drops_overlapping() {
    let mut t = Placements::new();
    // Anchored at row 3 (1-based), 4 rows high: 0-based rows 2..=5.
    t.upsert(placement_at(1, 3, 4, false));
    let removed = t.remove_intersecting(0, 23, /* force */ false);
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].image_id, ImageId(1));
    assert!(t.is_empty());
}

/// `C=1` is the producer's opt-out from the ED / EL path: programs that
/// pin a status-line image would otherwise lose it on every `clear`.
#[test]
fn remove_intersecting_keeps_c_1_placements_when_not_forced() {
    let mut t = Placements::new();
    t.upsert(placement_at(1, 3, 4, /* no_cursor_move */ true));
    let removed = t.remove_intersecting(0, 23, /* force */ false);
    assert_eq!(removed, Vec::<Placement>::new());
    assert_eq!(t.len(), 1);
}

/// Kitty spec: a hard reset (RIS / DECSTR) has no per-placement
/// opt-out.
#[test]
fn remove_intersecting_drops_c_1_placements_when_forced() {
    let mut t = Placements::new();
    t.upsert(placement_at(1, 3, 4, /* no_cursor_move */ true));
    let removed = t.remove_intersecting(0, 23, /* force */ true);
    assert_eq!(removed.len(), 1);
    assert!(t.is_empty());
}

/// An `EL 0` on a row above the placement must not remove it.
#[test]
fn remove_intersecting_no_overlap_no_op() {
    let mut t = Placements::new();
    t.upsert(placement_at(1, 5, 4, false));
    let removed = t.remove_intersecting(0, 2, false);
    assert_eq!(removed, Vec::<Placement>::new());
    assert_eq!(t.len(), 1);
}

#[test]
fn remove_intersecting_single_row_touch_evicts() {
    let mut t = Placements::new();
    t.upsert(placement_at(1, 5, 1, false));
    let removed = t.remove_intersecting(4, 4, false);
    assert_eq!(removed.len(), 1);
    assert!(
        t.is_empty(),
        "the reported eviction must actually leave the table",
    );
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
        cols,
        rows,
        source: None,
        z_index: z,
        no_cursor_move: false,
        quiet: 0,
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

#[test]
fn apc_dispatch_buffers_kitty_graphics_body_for_daemon_drain() {
    // docs/explanation/protocols/kitty-graphics.md "Dispatcher
    // architecture": the grid is a relay; the daemon runs the Kitty
    // graphics dispatcher off `take_pty_effects`.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b_Gi=1,a=q;\x1b\\");
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), 1, "one APC body");
    assert_eq!(drained[0].body, b"Gi=1,a=q;");
    assert!(
        apc_bodies(&mut g).is_empty(),
        "second drain returns no bodies (queue was emptied)",
    );
}

#[test]
fn apc_dispatch_preserves_burst_order_across_multiple_apcs() {
    // Chunked transmission (`m=1`): the daemon's `Reassembler::feed`
    // depends on chunks arriving in the order the producer emitted them.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b_Ga=t,m=1;chunk1\x1b\\\x1b_Gm=1;chunk2\x1b\\\x1b_Gm=0;chunk3\x1b\\",
    );
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), 3);
    assert_eq!(drained[0].body, b"Ga=t,m=1;chunk1");
    assert_eq!(drained[1].body, b"Gm=1;chunk2");
    assert_eq!(drained[2].body, b"Gm=0;chunk3");
}

#[test]
fn apc_body_captures_the_cursor_after_a_mid_burst_move() {
    // yazi wraps each preview in DECSC / CUP / image / DECRC in one
    // write, and the daemon drains the outbox only after the whole
    // `Parser::advance`, so each body must carry the cursor as it stood
    // at APC time.
    let mut p = Parser::new();
    let mut g = Grid::new(24, 80);
    drive(
        &mut p,
        &mut g,
        b"\x1b7\x1b[4;3H\x1b_Ga=T,f=24,s=1,v=1;AAA\x1b\\\x1b8",
    );
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), 1);
    // The CUP target, captured before DECRC.
    assert_eq!(
        (drained[0].cursor_row, drained[0].cursor_col),
        (3, 2),
        "body must carry the cursor at APC time, not after DECRC",
    );
    assert_eq!((g.cursor().row, g.cursor().col), (0, 0));
}

#[test]
fn apc_outbox_drops_past_cap_and_bells_so_producer_sees_loss() {
    // A producer flooding APCs without a chance to drain: the same
    // drain cycle reports both the surviving bodies and the loss
    // signal.
    const OVERFLOW: usize = 6;
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    let mut bytes = Vec::new();
    for i in 0..APC_OUTBOX_CAP + OVERFLOW {
        let body = format!("Gi={i};{i}");
        bytes.extend_from_slice(b"\x1b_");
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(b"\x1b\\");
    }
    drive(&mut p, &mut g, &bytes);
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), APC_OUTBOX_CAP);
    assert_eq!(drained[0].body, b"Gi=0;0");
    let last = APC_OUTBOX_CAP - 1;
    assert_eq!(drained[last].body, format!("Gi={last};{last}").into_bytes());
    assert!(
        g.take_bell_pending(),
        "drop past cap must signal the producer via BEL",
    );
}

#[test]
fn apc_overflow_signals_bell_so_truncation_is_audible() {
    // The parser calls `apc_overflow` before the truncated body reaches
    // `apc_dispatch`; one loss event per truncated dispatch.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    let mut bytes = Vec::from(*b"\x1b_G");
    // One KiB past the buffer cap, so truncation fires regardless of
    // the cap's exact value.
    bytes.extend(std::iter::repeat_n(b'x', felis_vt::APC_BUFFER_LIMIT + 1024));
    bytes.extend_from_slice(b"\x1b\\");
    drive(&mut p, &mut g, &bytes);
    assert!(g.take_bell_pending(), "apc_overflow must BEL");
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), 1, "truncated body still delivered");
    assert!(
        drained[0].body.len() <= felis_vt::APC_BUFFER_LIMIT,
        "body capped at parser's APC_BUFFER_LIMIT, got {}",
        drained[0].body.len(),
    );
}

#[test]
fn advance_cursor_after_image_placement_moves_by_rows_and_cols() {
    // docs/reference/protocols/kitty-graphics.md "Placement parameters":
    // the cursor lands at (row + rows, col + cols).
    let mut g = Grid::new(8, 16);
    g.screen.cursor.row = 1;
    g.screen.cursor.col = 2;
    g.advance_cursor_after_image_placement(2, 3, false);
    assert_eq!(g.screen.cursor.row, 3);
    assert_eq!(g.screen.cursor.col, 5);
    assert!(!g.screen.cursor.pending_wrap);
}

#[test]
fn advance_cursor_after_image_placement_respects_no_cursor_move() {
    // `C=1` leaves the cursor untouched; the producer keeps writing
    // where it expected.
    let mut g = Grid::new(8, 16);
    g.screen.cursor.row = 1;
    g.screen.cursor.col = 2;
    g.advance_cursor_after_image_placement(2, 3, true);
    assert_eq!(g.screen.cursor.row, 1);
    assert_eq!(g.screen.cursor.col, 2);
}

#[test]
fn advance_cursor_after_image_placement_wraps_past_right_margin() {
    // Col 14 + 5 cols in a 16-wide grid wraps to the next line.
    let mut g = Grid::new(8, 16);
    g.screen.cursor.row = 1;
    g.screen.cursor.col = 14;
    g.advance_cursor_after_image_placement(0, 5, false);
    assert_eq!(g.screen.cursor.col, 0, "past-edge wraps to col 0");
    assert!(g.screen.cursor.row > 1, "next-line wrap moved cursor down");
}

#[test]
fn advance_cursor_after_image_placement_clamps_past_bottom() {
    // Clamps instead of scrolling (see the method's doc).
    let mut g = Grid::new(4, 8);
    g.screen.cursor.row = 2;
    g.screen.cursor.col = 0;
    g.advance_cursor_after_image_placement(10, 1, false);
    assert_eq!(g.screen.cursor.row, 3, "clamped to last row");
}

#[test]
fn advance_cursor_after_image_placement_with_zero_dims_is_noop() {
    // `c=0,r=0` means "natural" size, which the dispatcher resolves; a
    // literal zero must not get a phantom movement.
    let mut g = Grid::new(8, 16);
    g.screen.cursor.row = 1;
    g.screen.cursor.col = 2;
    g.advance_cursor_after_image_placement(0, 0, false);
    assert_eq!((g.screen.cursor.row, g.screen.cursor.col), (1, 2));
}

// (docs/reference/protocols/kitty-graphics.md "Animation")

/// Each frame's pixel buffer is one byte so byte-accounting deltas are
/// easy to assert.
fn animated(gaps: &[u32]) -> ImageEntry {
    let mut e = ImageEntry::new(1, 1, ImageFormat::Rgb24, vec![0u8]);
    e.set_gap(0, gaps[0]);
    for &g in &gaps[1..] {
        e.frames.push(Frame {
            pixels: vec![0u8].into(),
            gap_ms: g,
        });
    }
    e
}

#[test]
fn new_entry_is_a_single_frame_still() {
    let e = ImageEntry::new(2, 2, ImageFormat::Rgba32, vec![0u8; 16]);
    assert_eq!(e.frame_count(), 1);
    assert!(!e.is_animated());
    assert_eq!(e.current_frame(), 0);
    assert_eq!(e.byte_len(), charge(16));
    let mut e = e;
    assert_eq!(e.advance(1_000_000), None);
}

#[test]
fn push_frame_grows_byte_total_and_evicts_others() {
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.insert(ImageId(2), entry(40)).unwrap();
    assert_eq!(s.bytes_used(), 2 * charge(40));
    // Image 2 (refcount 0) is evicted; image 1 is never evicted to fit
    // its own frame.
    let pushed = s.push_frame(ImageId(1), frame_of(40)).unwrap();
    assert_eq!(pushed.index, 1);
    assert!(
        s.get(ImageId(2)).is_none(),
        "image 2 should have been evicted"
    );
    assert_eq!(
        pushed.evicted,
        vec![ImageId(2)],
        "an eviction the caller cannot see is one it cannot announce",
    );
    assert_eq!(s.get(ImageId(1)).unwrap().frame_count(), 2);
    assert_eq!(s.bytes_used(), charge(40) + frame_charge(40));
}

#[test]
fn push_frame_rejects_when_pinned_image_blocks_eviction() {
    let mut s = ImageStore::new(charge(60));
    s.insert(ImageId(1), entry(60)).unwrap();
    s.retain(ImageId(1));
    // No other image to evict and the pinned one cannot shrink.
    let err = s.push_frame(ImageId(1), frame_of(60)).unwrap_err();
    assert!(matches!(err, FrameError::OverCapacity { .. }));
    assert_eq!(s.get(ImageId(1)).unwrap().frame_count(), 1);
    assert_eq!(s.bytes_used(), charge(60));
}

/// Everything reachable through [`ImageStore::animation_control`] is a
/// display knob; the store charges when pixels enter, and nothing here
/// moves pixels.
#[test]
fn animation_control_never_moves_the_stores_byte_total() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    s.push_frame(ImageId(1), frame_of(20)).unwrap();
    let before = s.bytes_used();

    let mut ctl = s.animation_control(ImageId(1)).unwrap();
    ctl.set_gap(0, 50);
    ctl.set_gap(1, 60);
    ctl.set_mode(AnimationMode::Running);
    ctl.set_max_loops(3);
    assert!(ctl.jump_to(1));

    assert_eq!(s.bytes_used(), before);
}

#[test]
fn remove_frame_keeps_current_in_range_and_frees_bytes() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    s.push_frame(ImageId(1), frame_of(20)).unwrap();
    s.push_frame(ImageId(1), frame_of(30)).unwrap();
    // Removing a frame before the displayed one shifts `current` down
    // so it still points at the same logical frame.
    s.animation_control(ImageId(1)).unwrap().jump_to(2);
    let freed = s.remove_frame(ImageId(1), 1).unwrap();
    assert_eq!(freed, frame_charge(20));
    let e = s.get(ImageId(1)).unwrap();
    assert_eq!(e.frame_count(), 2);
    assert_eq!(e.current_frame(), 1);
    assert_eq!(s.bytes_used(), charge(10) + frame_charge(30));
}

#[test]
fn remove_last_frame_is_refused() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    // The store never holds a zero-frame entry; whole-image delete is
    // the caller's job.
    assert_eq!(s.remove_frame(ImageId(1), 0), Err(FrameError::NoSuchFrame));
}

#[test]
fn running_animation_advances_one_frame_per_gap() {
    let mut e = animated(&[100, 100, 100]);
    e.set_mode(AnimationMode::Running);
    assert_eq!(e.advance(0), None);
    assert_eq!(e.current_frame(), 0);
    assert_eq!(e.advance(50), None);
    assert_eq!(e.advance(100), Some(1));
    assert_eq!(e.advance(200), Some(2));
}

#[test]
fn running_animation_loops_to_root_and_counts_loops() {
    let mut e = animated(&[10, 10]);
    e.set_mode(AnimationMode::Running);
    e.set_max_loops(2); // play through twice, then stop
    assert_eq!(e.advance(0), None); // anchor
    assert_eq!(e.advance(10), Some(1)); // -> frame 1
    assert_eq!(e.advance(20), Some(0)); // wrap -> loop 1, back to root
    assert_eq!(e.advance(30), Some(1)); // -> frame 1
    assert_eq!(e.advance(40), None);
    assert_eq!(e.current_frame(), 1);
}

#[test]
fn loading_mode_halts_on_last_frame() {
    let mut e = animated(&[10, 10]);
    e.set_mode(AnimationMode::Loading);
    assert_eq!(e.advance(0), None); // anchor
    assert_eq!(e.advance(10), Some(1)); // -> last frame
    // Loading waits at the last frame for more frames.
    assert_eq!(e.advance(20), None);
    assert_eq!(e.current_frame(), 1);
}

#[test]
fn gapless_frames_are_skipped() {
    // Frame 1 is gapless (a compositing base).
    let mut e = animated(&[10, 0, 10]);
    e.set_mode(AnimationMode::Running);
    assert_eq!(e.advance(0), None); // anchor
    assert_eq!(e.advance(10), Some(2));
}

#[test]
fn stopped_animation_never_advances() {
    let mut e = animated(&[10, 10]);
    e.set_mode(AnimationMode::Stopped);
    assert_eq!(e.advance(0), None);
    assert_eq!(e.advance(1000), None);
    assert_eq!(e.current_frame(), 0);
}

#[test]
fn jump_to_changes_current_and_reanchors() {
    let mut e = animated(&[10, 10, 10]);
    e.set_mode(AnimationMode::Running);
    assert!(e.jump_to(2));
    assert_eq!(e.current_frame(), 2);
    assert!(!e.jump_to(2)); // already there
    assert!(!e.jump_to(9)); // out of range
}

#[test]
fn advance_animations_only_ticks_placed_images() {
    let mut s = ImageStore::new(10_000);
    let mut a = animated(&[10, 10]);
    a.set_mode(AnimationMode::Running);
    let mut b = animated(&[10, 10]);
    b.set_mode(AnimationMode::Running);
    s.insert(ImageId(1), a).unwrap();
    s.insert(ImageId(2), b).unwrap();
    s.retain(ImageId(1)); // placed

    assert_eq!(s.next_animation_due_ms(0), Some(0));
    let changed = s.advance_animations(0); // anchors, no change yet
    assert_eq!(changed, Vec::<(ImageId, usize)>::new());
    assert_eq!(s.next_animation_due_ms(0), Some(10));
    let changed = s.advance_animations(10);
    assert_eq!(changed, vec![(ImageId(1), 1)]);
    assert_eq!(s.get(ImageId(2)).unwrap().current_frame(), 0);
}

#[test]
fn next_animation_due_is_none_when_nothing_placed_or_animating() {
    let mut s = ImageStore::new(10_000);
    let still = ImageEntry::new(1, 1, ImageFormat::Rgb24, vec![0]);
    s.insert(ImageId(1), still).unwrap();
    s.retain(ImageId(1));
    assert_eq!(s.next_animation_due_ms(0), None);
}

/// `is_animatable` requires `frames.len() > 1`; relaxing that to `>= 1`
/// would tick a one-frame "animation".
#[test]
fn single_frame_entry_with_a_gap_is_not_animatable() {
    let mut e = animated(&[100]); // one frame, gap 100
    e.set_mode(AnimationMode::Running);
    assert_eq!(e.frame_count(), 1);
    assert_eq!(e.advance(0), None);
    assert_eq!(e.advance(1_000), None);
    assert_eq!(e.current_frame(), 0);
    assert_eq!(e.next_due_ms(), None);
}

/// The reject test is `needed > cap`; `>=` would spuriously reject an
/// exactly-fitting image.
#[test]
fn insert_exactly_at_cap_is_accepted() {
    let mut s = ImageStore::new(charge(64));
    s.insert(ImageId(1), entry(64)).unwrap();
    assert_eq!(s.bytes_used(), charge(64));
    assert_eq!(s.get(ImageId(1)).unwrap().byte_len(), charge(64));
}

/// Deleting the `!` in the `!contains_key` skip would return a
/// colliding live id or spin.
#[test]
fn allocate_anonymous_id_counts_down_and_skips_live_ids() {
    let mut s = ImageStore::new(1024);
    let first = s.allocate_anonymous_id();
    assert_eq!(first, ImageId(u32::MAX));
    let second = s.allocate_anonymous_id();
    assert_eq!(second, ImageId(u32::MAX - 1));
    // Occupy the next id that would be handed out.
    s.insert(ImageId(u32::MAX - 2), entry(8)).unwrap();
    let third = s.allocate_anonymous_id();
    assert_eq!(third, ImageId(u32::MAX - 3), "must skip the live id");
}

/// Growing then shrinking pins the byte-total delta math and the
/// grow-versus-shrink eviction guard.
#[test]
fn replace_frame_adjusts_byte_accounting_by_the_delta() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    s.push_frame(ImageId(1), frame_of(20)).unwrap();
    assert_eq!(s.bytes_used(), charge(10) + frame_charge(20));
    s.replace_frame(ImageId(1), 1, frame_of(50)).unwrap();
    assert_eq!(s.bytes_used(), charge(10) + frame_charge(50));
    assert_eq!(
        s.get(ImageId(1)).unwrap().frame(1).unwrap().pixels.len(),
        50
    );
    s.replace_frame(ImageId(1), 1, frame_of(5)).unwrap();
    assert_eq!(s.bytes_used(), charge(10) + frame_charge(5));
}

/// Growing a frame near the cap must evict other refcount-0 images by
/// exactly the growth delta: pins the `new_len > old_len` branch and
/// the `new_len - old_len` evict amount. The `>`→`>=` widening is
/// pinned by `equal_size_replace_frame_evicts_nothing_in_an_over_cap_store`.
#[test]
fn replace_frame_grows_near_cap_and_evicts_others() {
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.insert(ImageId(2), entry(40)).unwrap();
    assert_eq!(s.bytes_used(), 2 * charge(40));
    s.replace_frame(ImageId(1), 0, frame_of(70)).unwrap();
    assert!(
        s.get(ImageId(2)).is_none(),
        "image 2 should have been evicted to fit the larger frame"
    );
    assert_eq!(s.get(ImageId(1)).unwrap().byte_len(), charge(70));
    assert_eq!(s.bytes_used(), charge(70));
}

/// A same-size replacement charges nothing and evicts nothing,
/// even when the store starts over capacity.
#[test]
fn equal_size_replace_frame_evicts_nothing_in_an_over_cap_store() {
    let mut entries = IndexMap::new();
    entries.insert(ImageId(1), entry(40));
    // The bystander: refcount 0, first to go in an eviction pass.
    entries.insert(ImageId(2), entry(40));
    let mut s = ImageStore::over_cap_for_test(entries, charge(40));
    assert!(s.bytes_used() > s.bytes_cap(), "store must start over cap");

    s.replace_frame(ImageId(1), 0, frame_of(40)).unwrap();

    assert!(
        s.get(ImageId(2)).is_some(),
        "a zero-byte replacement must not evict a bystander image"
    );
    assert_eq!(s.bytes_used(), 2 * charge(40));
    // `entry` builds its root frame gapless, `frame_of` at 40 ms, so
    // the gap proves the swap landed.
    assert_eq!(s.get(ImageId(1)).unwrap().frame(0).unwrap().gap_ms, 40);
}

/// Pins against the `-> Ok(())` body mutant.
#[test]
fn replace_frame_rejects_unknown_image_and_frame() {
    let mut s = ImageStore::new(1000);
    assert_eq!(
        s.replace_frame(
            ImageId(9),
            0,
            Frame {
                pixels: vec![0].into(),
                gap_ms: 0
            }
        ),
        Err(FrameError::NoSuchImage),
    );
    s.insert(ImageId(1), entry(10)).unwrap();
    assert_eq!(
        s.replace_frame(
            ImageId(1),
            5,
            Frame {
                pixels: vec![0].into(),
                gap_ms: 0
            }
        ),
        Err(FrameError::NoSuchFrame),
    );
}

/// Pins the `frame_idx < current` branch and its `current -= 1`.
#[test]
fn remove_frame_below_current_decrements_current() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    for _ in 0..3 {
        s.push_frame(
            ImageId(1),
            Frame {
                pixels: vec![0u8; 10].into(),
                gap_ms: 40,
            },
        )
        .unwrap();
    }
    s.animation_control(ImageId(1)).unwrap().jump_to(2);
    assert_eq!(s.get(ImageId(1)).unwrap().current_frame(), 2);
    s.remove_frame(ImageId(1), 0).unwrap();
    let e = s.get(ImageId(1)).unwrap();
    assert_eq!(e.frame_count(), 3);
    assert_eq!(
        e.current_frame(),
        1,
        "current follows the same logical frame"
    );
}

/// Catches the `len -> 0` and `is_empty -> true` constant-body mutants.
#[test]
fn len_and_is_empty_reflect_a_populated_store() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(1), entry(8)).unwrap();
    s.insert(ImageId(2), entry(8)).unwrap();
    assert_eq!(s.len(), 2);
    assert!(!s.is_empty());
}

/// The constant `empty()` body and the `!=`→`==` retain mutant both
/// break this.
#[test]
fn iter_ids_reflects_insertion_order_and_deletion() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(3), entry(8)).unwrap();
    s.insert(ImageId(1), entry(8)).unwrap();
    s.insert(ImageId(2), entry(8)).unwrap();
    assert_eq!(
        s.iter_ids().collect::<Vec<_>>(),
        vec![ImageId(3), ImageId(1), ImageId(2)],
    );
    s.delete(ImageId(1));
    assert_eq!(
        s.iter_ids().collect::<Vec<_>>(),
        vec![ImageId(3), ImageId(2)],
        "delete drops only the named id from the order",
    );
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

/// A store that counted pixels alone lets a flood of 1x1 transmissions
/// past a 256 MiB cap while really holding gigabytes: 4 charged bytes
/// hide ~40 real ones.
#[test]
fn the_budget_charges_per_entry_overhead_not_only_pixels() {
    let one_pixel = entry(4);
    assert!(
        one_pixel.byte_len() > 4 * 8,
        "a 4-byte image charged {} bytes — the fixed cost is missing",
        one_pixel.byte_len(),
    );
    // Entry count, not just byte volume, is what the cap bounds.
    let per_entry = one_pixel.byte_len();
    let mut store = ImageStore::new(per_entry * 4);
    for id in 0..4 {
        assert!(store.insert(ImageId(id), entry(4)).is_ok(), "id {id}");
    }
    // Pinned so eviction cannot make room; the fifth is refused on
    // overhead alone.
    for id in 0..4 {
        assert!(store.retain(ImageId(id)));
    }
    assert!(store.insert(ImageId(4), entry(4)).is_err());
}

use super::*;

#[test]
fn extent_resolve_keeps_a_request_and_rounds_an_auto_axis_up() {
    let (r, n) = (Extent::Requested, Extent::Natural);
    assert_eq!(
        Extent::resolve((7, 2), (30, 30), (8, 8)),
        (r(nz(7)), r(nz(2)))
    );
    assert_eq!(
        Extent::resolve((0, 0), (30, 30), (10, 8)),
        (n(nz(3)), n(nz(4)))
    );
}

#[test]
fn extent_resolve_derives_an_auto_axis_from_the_requested_one() {
    let (r, d) = (Extent::Requested, Extent::Derived);
    // r=2 at 20 px rows is 40 px tall, 40.4 px wide: 5 columns of 10 px.
    assert_eq!(
        Extent::resolve((0, 2), (101, 100), (10, 20)),
        (d(nz(5)), r(nz(2)))
    );
    // c=3 at 10 px is 30 px wide, 60 px tall: exactly 3 rows of 20 px.
    assert_eq!(
        Extent::resolve((3, 0), (50, 100), (10, 20)),
        (r(nz(3)), d(nz(3)))
    );
}

#[test]
fn extent_resolve_falls_back_to_natural_without_source_pixels_to_divide_by() {
    let (r, n) = (Extent::Requested, Extent::Natural);
    assert_eq!(
        Extent::resolve((0, 2), (30, 0), (10, 20)),
        (n(nz(3)), r(nz(2)))
    );
}

#[test]
fn extent_resolve_counts_an_unknown_cell_size_as_one_pixel() {
    let n = Extent::Natural;
    assert_eq!(
        Extent::resolve((0, 0), (30, 0), (0, 8)),
        (n(nz(30)), n(nz(1)))
    );
    assert_eq!(
        Extent::resolve((0, 0), (1 << 20, 1), (0, 0)),
        (n(nz(u16::MAX)), n(nz(1)))
    );
    assert_eq!(
        Extent::resolve((0, 2), (30, 40), (0, 0)),
        (Extent::Derived(nz(2)), Extent::Requested(nz(2)))
    );
}

#[test]
fn clip_source_keeps_the_part_of_the_rectangle_inside_the_image() {
    let rect = |x, y, width, height| SourceRect {
        x,
        y,
        width,
        height,
    };
    assert_eq!(clip_source(32, 16, None), Some(rect(0, 0, 32, 16)));
    assert_eq!(
        clip_source(32, 16, Some(rect(4, 2, 8, 8))),
        Some(rect(4, 2, 8, 8))
    );
    assert_eq!(
        clip_source(32, 16, Some(rect(20, 10, 30, 30))),
        Some(rect(20, 10, 12, 6))
    );
    assert_eq!(clip_source(32, 16, Some(rect(40, 0, 8, 8))), None);
    assert_eq!(clip_source(0, 0, None), None);
}

fn store_with_30x40(id: u32) -> ImageStore {
    let mut store = ImageStore::new(1 << 20);
    store
        .insert(
            ImageId(id),
            ImageEntry::new(30, 40, ImageFormat::Rgb24, vec![0u8; 30 * 40 * 3]),
        )
        .unwrap();
    store
}

#[test]
fn rescale_re_resolves_only_auto_axes_and_reports_the_changed_placements() {
    let store = store_with_30x40(1);
    let mut t = Placements::new();
    t.upsert(requested(1, 1, 0, 0, (3, 2)));
    t.upsert(requested(1, 2, 5, 5, (5, 5)));
    t.upsert(requested(1, 3, 0, 2, (2, 2)));
    let mut changed = Vec::new();
    t.rescale(&store, (8, 16), |p| changed.push(p.placement_id));
    let extents: Vec<_> = t.iter().map(Placement::extent).collect();
    assert_eq!(
        extents,
        [(4, 3), (5, 5), (3, 2)],
        "r=2 of 16 px rows is 32 px tall and 24 px wide: 3 columns of 8 px",
    );
    assert_eq!(
        changed,
        [Some(PlacementId(1)), Some(PlacementId(3))],
        "the explicit placement did not move",
    );
}

#[test]
fn rescale_keeps_a_derived_axis_while_either_cell_size_is_unknown() {
    let store = store_with_30x40(1);
    let mut t = Placements::new();
    t.upsert(requested(1, 1, 0, 2, (2, 2)));
    let mut changed = 0;
    t.rescale(&store, (8, 0), |_| changed += 1);
    assert_eq!((t.iter().next().unwrap().extent(), changed), ((2, 2), 0));
}

#[test]
fn a_placement_reports_the_axes_it_was_put_with() {
    let p = requested(1, 1, 0, 2, (3, 2));
    assert_eq!(p.requested(), RequestedExtent { cols: 0, rows: 2 });
}

#[test]
fn rescale_keeps_an_axis_whose_cell_size_is_unknown() {
    let store = store_with_30x40(1);
    let mut t = Placements::new();
    t.upsert(requested(1, 1, 0, 0, (3, 2)));
    t.rescale(&store, (0, 16), |_| {});
    let p = t.iter().next().unwrap();
    assert_eq!(p.extent(), (3, 3));
}

#[test]
fn rescale_leaves_a_placement_whose_image_is_gone() {
    let store = store_with_30x40(1);
    let mut t = Placements::new();
    t.upsert(requested(2, 1, 0, 0, (3, 2)));
    let mut changed = 0;
    t.rescale(&store, (8, 16), |_| changed += 1);
    assert_eq!(changed, 0);
    let p = t.iter().next().unwrap();
    assert_eq!(p.extent(), (3, 2));
}

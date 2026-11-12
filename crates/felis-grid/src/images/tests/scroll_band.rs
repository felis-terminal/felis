use super::*;

const SCROLL_CELL: (u16, u16) = (10, 20);

fn sized_store(sizes: &[(u32, u32)]) -> ImageStore {
    let mut store = ImageStore::new(usize::MAX);
    for (id, &(w, h)) in (1u32..).zip(sizes) {
        store
            .insert(
                ImageId(id),
                ImageEntry::new(w, h, ImageFormat::Rgba32, vec![0u8; 4]),
            )
            .unwrap();
    }
    store
}

/// A put of image `image` at `row` with `c=` / `r=` as `requested`,
/// its extent resolved at [`SCROLL_CELL`].
fn put_at(store: &ImageStore, image: u32, row: i32, requested: (u16, u16)) -> Placement {
    let entry = store.get(ImageId(image)).unwrap();
    let (cols, rows) = Extent::resolve(requested, (entry.width, entry.height), SCROLL_CELL);
    Placement {
        image_id: ImageId(image),
        placement_id: Some(PlacementId(image)),
        anchor: CellPos { row, col: 1 },
        cols,
        rows,
        source: None,
        z_index: 0,
        no_cursor_move: false,
        quiet: 0,
        anonymous: false,
        serial: 0,
    }
}

/// Rows `top..=bottom`, 1-based.
fn band(top: u16, bottom: u16, n: u16, direction: ScrollDirection) -> ScrollOp {
    ScrollOp {
        region_top: top - 1,
        region_bottom: bottom - 1,
        n_rows: n,
        direction,
    }
}

fn scroll(
    t: &mut Placements,
    store: &ImageStore,
    op: ScrollOp,
    into_history: bool,
) -> (Vec<Placement>, Vec<u32>) {
    let (moved, removed) = t.scroll_region(op, into_history, store, SCROLL_CELL, 100);
    (moved, removed.iter().map(|p| p.image_id.0).collect())
}

const fn src(x: u32, y: u32, width: u32, height: u32) -> SourceRect {
    SourceRect {
        x,
        y,
        width,
        height,
    }
}

#[test]
fn a_placement_inside_a_scrolled_band_moves_with_it() {
    let store = sized_store(&[(100, 40)]);
    let mut t = Placements::new();
    t.upsert(put_at(&store, 1, 5, (0, 0)));
    let (moved, removed) = scroll(&mut t, &store, band(3, 8, 2, ScrollDirection::Up), false);
    assert_eq!(removed, Vec::<u32>::new());
    assert_eq!(moved.len(), 1);
    let p = t.iter().next().unwrap();
    assert_eq!((p.anchor.row, p.extent(), p.source), (3, (10, 2), None));
}

#[test]
fn a_placement_scrolled_wholly_out_of_its_band_is_removed() {
    let store = sized_store(&[(100, 40)]);
    let mut t = Placements::new();
    t.upsert(put_at(&store, 1, 3, (0, 0)));
    let (moved, removed) = scroll(&mut t, &store, band(3, 8, 2, ScrollDirection::Up), false);
    assert_eq!(moved, Vec::<Placement>::new());
    assert_eq!(removed, [1]);
    assert!(t.is_empty());
}

#[test]
fn a_placement_crossing_or_outside_the_band_stays_put() {
    let store = sized_store(&[(100, 40), (100, 40), (100, 40)]);
    let mut t = Placements::new();
    t.upsert(put_at(&store, 1, 2, (0, 0))); // rows 2..=3, across the top margin
    t.upsert(put_at(&store, 2, 8, (0, 0))); // rows 8..=9, across the bottom margin
    t.upsert(put_at(&store, 3, 10, (0, 0))); // below the band
    for direction in [ScrollDirection::Up, ScrollDirection::Down] {
        let (moved, removed) = scroll(&mut t, &store, band(3, 8, 1, direction), false);
        assert!(moved.is_empty() && removed.is_empty(), "{direction:?}");
    }
    let rows: Vec<i32> = t.iter().map(|p| p.anchor.row).collect();
    assert_eq!(rows, [2, 8, 10]);
}

/// `c=5,r=4` of a 100×200 image draws 80 px tall at 20 px rows, so
/// each row past the top margin is 50 source rows.
#[test]
fn the_rows_past_the_top_margin_are_cut_from_the_top_of_the_source() {
    let store = sized_store(&[(100, 200)]);
    let mut t = Placements::new();
    t.upsert(put_at(&store, 1, 3, (5, 4)));
    let (moved, _) = scroll(&mut t, &store, band(3, 8, 1, ScrollDirection::Up), false);
    let p = &moved[0];
    assert_eq!(p.anchor.row, 3);
    assert_eq!(p.source, Some(src(0, 50, 100, 150)));
    assert_eq!(p.requested(), RequestedExtent { cols: 5, rows: 3 });
    assert_eq!(t.iter().next(), Some(p));
}

/// A lone `c=5` draws the 100×200 image 50×100 px: two source rows per
/// screen row, and the derived rows shrink with the cut.
#[test]
fn a_top_cut_of_a_width_only_put_keeps_its_aspect_scale() {
    let store = sized_store(&[(100, 200)]);
    let mut t = Placements::new();
    t.upsert(put_at(&store, 1, 3, (5, 0)));
    let (moved, _) = scroll(&mut t, &store, band(3, 8, 1, ScrollDirection::Up), false);
    let p = &moved[0];
    assert_eq!((p.anchor.row, p.extent()), (3, (5, 4)));
    assert_eq!(p.source, Some(src(0, 40, 100, 160)));
}

/// A 50 px tall image at its own size covers three 20 px rows, the
/// last one half: the cut keeps the two whole rows above the margin.
#[test]
fn the_rows_past_the_bottom_margin_are_cut_from_the_bottom_of_the_source() {
    let store = sized_store(&[(100, 50)]);
    let mut t = Placements::new();
    t.upsert(put_at(&store, 1, 6, (0, 0)));
    let (moved, _) = scroll(&mut t, &store, band(3, 8, 1, ScrollDirection::Down), false);
    let p = &moved[0];
    assert_eq!((p.anchor.row, p.extent()), (7, (10, 2)));
    assert_eq!(p.source, Some(src(0, 0, 100, 40)));
    let (moved, _) = scroll(&mut t, &store, band(3, 8, 1, ScrollDirection::Down), false);
    assert_eq!(moved[0].source, Some(src(0, 0, 100, 20)));
    let (moved, removed) = scroll(&mut t, &store, band(3, 8, 1, ScrollDirection::Down), false);
    assert_eq!(moved, Vec::<Placement>::new());
    assert_eq!(removed, [1]);
}

#[test]
fn a_band_scrolling_into_history_carries_its_placements_and_the_history_s() {
    let store = sized_store(&[(100, 40), (100, 40), (100, 40)]);
    let mut t = Placements::new();
    t.upsert(put_at(&store, 1, 2, (0, 0))); // rows 2..=3, inside rows 1..=3
    t.upsert(put_at(&store, 2, -3, (0, 0))); // in history
    t.upsert(put_at(&store, 3, 3, (0, 0))); // rows 3..=4, across the bottom margin
    let (moved, removed) = scroll(&mut t, &store, band(1, 3, 2, ScrollDirection::Up), true);
    assert_eq!(removed, Vec::<u32>::new());
    assert_eq!(moved.len(), 2);
    let placed: Vec<(u32, i32, Option<SourceRect>)> = t
        .iter()
        .map(|p| (p.image_id.0, p.anchor.row, p.source))
        .collect();
    assert_eq!(placed, [(1, 0, None), (2, -5, None), (3, 3, None)]);
}

/// Image sizes, one `(row, (c, r))` put of each, and the scroll.
type ScrollCase = (Vec<(u32, u32)>, Vec<(i32, (u16, u16))>, ScrollOp);

fn scroll_case() -> impl Strategy<Value = ScrollCase> {
    (
        proptest::collection::vec((1u32..300, 1u32..300), 1..5),
        proptest::collection::vec((1i32..13, (0u16..6, 0u16..6)), 1..5),
        (1u16..13, 1u16..13, 1u16..13, any::<bool>()),
    )
        .prop_map(|(sizes, puts, (a, b, n, up))| {
            let puts = puts.into_iter().take(sizes.len()).collect::<Vec<_>>();
            let op = band(
                a.min(b),
                a.max(b),
                n,
                if up {
                    ScrollDirection::Up
                } else {
                    ScrollDirection::Down
                },
            );
            (sizes, puts, op)
        })
}

proptest! {
    /// A band scroll keeps every survivor that started in the band
    /// inside it, drawn from inside its image at the scale it drew at
    /// before, and leaves every other placement as it was.
    #[test]
    fn a_band_scroll_keeps_survivors_in_the_band_at_their_scale(
        (sizes, puts, op) in scroll_case(),
    ) {
        let store = sized_store(&sizes);
        let mut t = Placements::new();
        for (image, &(row, requested)) in (1u32..).zip(&puts) {
            t.upsert(put_at(&store, image, row, requested));
        }
        let before: Vec<Placement> = t.iter().cloned().collect();
        let (top, bottom) = (i32::from(op.region_top) + 1, i32::from(op.region_bottom) + 1);
        let inside = |p: &Placement| {
            p.anchor.row >= top
                && p.anchor.row + i32::from(p.rows.cells().get()) - 1 <= bottom
        };
        let scale = |p: &Placement| {
            let entry = store.get(p.image_id).unwrap();
            let s = clip_source(entry.width, entry.height, p.source).unwrap();
            (source_px_per_screen_px(p.requested(), s, SCROLL_CELL), s)
        };
        t.scroll_region(op, false, &store, SCROLL_CELL, 0);
        for old in &before {
            let now = t.iter().find(|p| p.image_id == old.image_id);
            if !inside(old) {
                prop_assert_eq!(now, Some(old));
                continue;
            }
            let Some(now) = now else { continue };
            prop_assert!(inside(now), "{:?} left the band", now);
            let entry = store.get(now.image_id).unwrap();
            let (new_scale, s) = scale(now);
            prop_assert!(s.y + s.height <= entry.height && s.x + s.width <= entry.width);
            let (old_scale, _) = scale(old);
            let dest_h = f64::from(s.height) / new_scale;
            prop_assert!(
                (new_scale - old_scale).abs() * dest_h <= 1.0,
                "scale {} became {}", old_scale, new_scale
            );
        }
    }
}

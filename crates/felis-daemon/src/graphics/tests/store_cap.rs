use super::*;

/// Three 1×1 images fill the cap; `keys` is one command's controls.
fn run_under_a_three_image_cap(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    keys: &[(u8, &[u8])],
) -> Vec<ImageEvent> {
    let mut grid = felis_grid::Grid::new(24, 80);
    let mut all = keys.to_vec();
    all.extend_from_slice(&[(b'f', b"32".as_slice()), (b's', b"1"), (b'v', b"1")]);
    let mut events = Vec::new();
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images,
            placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        complete(&all, &b64(&[0u8; 4])),
    );
    events
}

fn three_image_store() -> felis_grid::images::ImageStore {
    let charge = ImageEntry::new(1, 1, ImageFormat::Rgba32, vec![0u8; 4]).byte_len();
    felis_grid::images::ImageStore::new(3 * charge)
}

fn stored_ids(images: &felis_grid::images::ImageStore) -> Vec<u32> {
    let mut ids: Vec<u32> = images.iter_ids().map(|id| id.0).collect();
    ids.sort_unstable();
    ids
}

#[test]
fn the_byte_cap_frees_every_unplaced_image_before_a_placed_one() {
    let mut images = three_image_store();
    let mut placements = felis_grid::images::Placements::new();
    for keys in [
        &[(b'a', b"T".as_slice()), (b'i', b"1")][..],
        &[(b'a', b"t"), (b'i', b"2")],
        &[(b'a', b"t"), (b'i', b"3")],
    ] {
        run_under_a_three_image_cap(&mut images, &mut placements, keys);
    }
    run_under_a_three_image_cap(&mut images, &mut placements, &[(b'a', b"t"), (b'i', b"4")]);
    assert_eq!(
        stored_ids(&images),
        vec![1, 4],
        "kitty trims every unplaced image at once"
    );
}

#[test]
fn the_byte_cap_evicts_the_least_recently_used_placed_image() {
    let mut images = three_image_store();
    let mut placements = felis_grid::images::Placements::new();
    for id in [b"1", b"2", b"3"] {
        run_under_a_three_image_cap(&mut images, &mut placements, &[(b'a', b"T"), (b'i', id)]);
    }
    run_under_a_three_image_cap(&mut images, &mut placements, &[(b'a', b"p"), (b'i', b"1")]);
    let events =
        run_under_a_three_image_cap(&mut images, &mut placements, &[(b'a', b"T"), (b'i', b"4")]);
    assert_eq!(
        stored_ids(&images),
        vec![1, 3, 4],
        "image 1 was placed again after 2"
    );
    assert!(events.contains(&ImageEvent::Delete { id: ImageId(2) }));
}

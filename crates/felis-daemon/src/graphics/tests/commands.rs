use super::*;

#[test]
fn transmit_with_q3_or_higher_is_treated_as_silent() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = tiny_rgba_cmd(&[
        (b'a', b"t".as_slice()),
        (b'i', b"3".as_slice()),
        (b'q', b"99".as_slice()),
    ]);
    assert!(
        run(&mut grid, &mut images, &mut placements, cmd).is_none(),
        "q=99 silences",
    );
}

#[test]
fn transmit_with_missing_image_id_auto_allocates_an_anonymous_id() {
    // The Kitty spec allows an anonymous transmit (yazi's direct
    // preview path sends one); with no id to address, nothing replies.
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = tiny_rgba_cmd(&[(b'a', b"t".as_slice())]);
    let response = run(&mut grid, &mut images, &mut placements, cmd);
    assert_eq!(response, None);
    assert!(
        images.get(ImageId(u32::MAX)).is_some(),
        "anonymous image lands under a terminal-allocated id",
    );
}

#[test]
fn transmit_with_image_id_zero_is_anonymous() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = tiny_rgba_cmd(&[(b'a', b"t".as_slice()), (b'i', b"0".as_slice())]);
    let response = run(&mut grid, &mut images, &mut placements, cmd);
    assert_eq!(response, None);
    assert!(images.get(ImageId(u32::MAX)).is_some());
}

#[test]
fn transmit_and_display_records_placement_at_cursor() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = tiny_rgba_cmd(&[(b'a', b"T".as_slice()), (b'i', b"42".as_slice())]);
    run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert!(images.get(ImageId(42)).is_some());
    let placement = placements
        .iter()
        .next()
        .expect("a=T must record a placement");
    assert_eq!(placement.image_id, ImageId(42));
    assert_eq!(placement.anchor, CellPos { row: 1, col: 1 });
    assert_eq!(
        images.get(ImageId(42)).unwrap().refcount(),
        1,
        "placement bumps the image's refcount so eviction can't drop it",
    );
}

#[test]
fn delete_with_d_eq_a_clears_all_placements_but_keeps_images() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    for id in 1..=3u32 {
        let id_str = id.to_string();
        let mut all = vec![(b'a', b"T".as_slice()), (b'i', id_str.as_bytes())];
        all.extend_from_slice(&ctrls);
        run(
            &mut grid,
            &mut images,
            &mut placements,
            complete(&all, &payload),
        )
        .unwrap();
    }
    assert_eq!(placements.len(), 3);
    let cmd = complete(&[(b'a', b"d"), (b'd', b"a")], b"");
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert!(placements.is_empty());
    assert_eq!(images.len(), 3, "images survive d=a");
    for id in 1..=3 {
        assert_eq!(images.get(ImageId(id)).unwrap().refcount(), 0);
    }
}

fn delete_test_state(
    image_ids: &[u32],
) -> (
    felis_grid::Grid,
    felis_grid::images::ImageStore,
    felis_grid::images::Placements,
) {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    for id in image_ids {
        let id_str = id.to_string();
        let mut all = vec![(b'a', b"t".as_slice()), (b'i', id_str.as_bytes())];
        all.extend_from_slice(&ctrls);
        run(
            &mut grid,
            &mut images,
            &mut placements,
            complete(&all, &payload),
        )
        .unwrap();
    }
    (grid, images, placements)
}

#[test]
fn delete_d_eq_p_drops_placement_intersecting_the_xy_cell() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2]);
    placements.upsert(seeded_placement(1, None, 5, 10, 2, 5, 0, false));
    placements.upsert(seeded_placement(2, None, 1, 1, 1, 1, 0, false));
    let cmd = complete(
        &[(b'a', b"d"), (b'd', b"p"), (b'x', b"12"), (b'y', b"5")],
        b"",
    );
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert_eq!(placements.len(), 1, "image 2 should survive the d=p");
    assert_eq!(placements.iter().next().unwrap().image_id, ImageId(2));
    assert!(
        images.get(ImageId(1)).is_some(),
        "lowercase d=p keeps image bytes",
    );
}

#[test]
fn delete_d_eq_capital_p_also_frees_the_image_store() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[7]);
    placements.upsert(seeded_placement(7, None, 5, 10, 2, 5, 0, false));
    let cmd = complete(
        &[(b'a', b"d"), (b'd', b"P"), (b'x', b"12"), (b'y', b"5")],
        b"",
    );
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert!(placements.is_empty());
    assert!(images.get(ImageId(7)).is_none(), "d=P frees the image too");
}

#[test]
fn delete_d_eq_x_drops_placements_intersecting_the_column() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2, 3]);
    placements.upsert(seeded_placement(1, None, 1, 10, 1, 6, 0, false));
    placements.upsert(seeded_placement(2, None, 20, 12, 1, 3, 0, false));
    placements.upsert(seeded_placement(3, None, 5, 1, 1, 5, 0, false));
    let cmd = complete(&[(b'a', b"d"), (b'd', b"x"), (b'x', b"12")], b"");
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![3], "only image 3 (col 1..=5) survives x=12");
}

#[test]
fn delete_d_eq_y_drops_placements_intersecting_the_row() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2, 3]);
    placements.upsert(seeded_placement(1, None, 5, 1, 3, 2, 0, false));
    placements.upsert(seeded_placement(2, None, 5, 70, 1, 1, 0, false));
    placements.upsert(seeded_placement(3, None, 10, 1, 1, 1, 0, false));
    let cmd = complete(&[(b'a', b"d"), (b'd', b"y"), (b'y', b"5")], b"");
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![3], "only image 3 (row 10) survives y=5");
}

#[test]
fn delete_d_eq_z_drops_placements_at_the_specified_z_index() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2, 3]);
    placements.upsert(seeded_placement(1, None, 1, 1, 1, 1, 0, false));
    placements.upsert(seeded_placement(2, None, 1, 1, 1, 1, -1, false));
    placements.upsert(seeded_placement(3, None, 1, 1, 1, 1, -1, false));
    let cmd = complete(&[(b'a', b"d"), (b'd', b"z"), (b'z', b"-1")], b"");
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![1], "z=-1 wipes the two -1 placements");
}

#[test]
fn delete_d_eq_q_requires_all_three_coords_to_match() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2]);
    placements.upsert(seeded_placement(1, None, 5, 5, 1, 1, 0, false));
    placements.upsert(seeded_placement(2, None, 5, 5, 1, 1, 3, false));
    let cmd = complete(
        &[
            (b'a', b"d"),
            (b'd', b"q"),
            (b'x', b"5"),
            (b'y', b"5"),
            (b'z', b"3"),
        ],
        b"",
    );
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![1], "image 1 at z=0 survives the z=3 filter");
}

#[test]
fn delete_d_eq_c_uses_the_current_cursor_position() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2]);
    placements.upsert(seeded_placement(1, None, 1, 1, 1, 1, 0, false));
    placements.upsert(seeded_placement(2, None, 5, 5, 1, 1, 0, false));
    let cmd = complete(&[(b'a', b"d"), (b'd', b"c")], b"");
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![2]);
}

/// For `d=r`, `x=`/`y=` are an image-id range, not cell coordinates
/// (the Kitty spec overloads the keys).
#[test]
fn delete_d_eq_r_drops_placements_in_the_image_id_range() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[3, 5, 7, 9]);
    for id in [3u32, 5, 7, 9] {
        placements.upsert(seeded_placement(id, None, 1, 1, 1, 1, 0, false));
    }
    let cmd = complete(
        &[(b'a', b"d"), (b'd', b"r"), (b'x', b"5"), (b'y', b"7")],
        b"",
    );
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![3, 9], "ids 5 and 7 inside [5..=7] are dropped");
    assert!(images.get(ImageId(5)).is_some());
}

#[test]
fn delete_d_eq_capital_r_also_frees_images_in_range() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[3, 5, 7]);
    for id in [3u32, 5, 7] {
        placements.upsert(seeded_placement(id, None, 1, 1, 1, 1, 0, false));
    }
    let cmd = complete(
        &[(b'a', b"d"), (b'd', b"R"), (b'x', b"5"), (b'y', b"7")],
        b"",
    );
    run(&mut grid, &mut images, &mut placements, cmd)
        .ok_or(())
        .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert_eq!(placements.len(), 1);
    assert!(images.get(ImageId(5)).is_none());
    assert!(images.get(ImageId(7)).is_none());
    assert!(images.get(ImageId(3)).is_some(), "id 3 outside the range");
}

#[test]
fn delete_extended_modes_emit_placement_removed_per_evicted_entry() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2]);
    placements.upsert(seeded_placement(1, None, 1, 1, 1, 1, 0, false));
    placements.upsert(seeded_placement(2, None, 1, 1, 1, 1, 0, false));
    let mut events = Vec::new();
    let cmd = complete(&[(b'a', b"d"), (b'd', b"c")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    let removed_count = events
        .iter()
        .filter(|e| matches!(e, ImageEvent::PlacementRemoved { .. }))
        .count();
    assert_eq!(removed_count, 2);
}

#[test]
fn query_with_valid_payload_emits_ok() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = tiny_rgba_cmd(&[(b'a', b"q".as_slice()), (b'i', b"31".as_slice())]);
    let response = run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert_eq!(response, b"\x1b_Gi=31;OK\x1b\\");
    assert!(
        images.is_empty(),
        "a=q is a dry run; the store stays untouched",
    );
}

#[test]
fn query_with_invalid_payload_emits_specific_error() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = complete(
        &[
            (b'a', b"q"),
            (b'i', b"32"),
            (b'f', b"99"),
            (b's', b"1"),
            (b'v', b"1"),
        ],
        b"",
    );
    let response = run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=32;EINVAL"),
        "f=99 → EINVAL, got: {response:?}",
    );
}

#[test]
fn unknown_action_is_dropped_without_a_reply() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = complete(&[(b'a', b"X"), (b'i', b"1")], b"");
    let response = run(&mut grid, &mut images, &mut placements, cmd);
    assert_eq!(response, None);
}

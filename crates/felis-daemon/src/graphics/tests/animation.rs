use super::*;

fn transmit_root(
    grid: &mut felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    id: &[u8],
) {
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"t".as_slice()), (b'i', id)];
    all.extend_from_slice(&ctrls);
    run(grid, images, placements, complete(&all, &payload));
}

fn push_frame_cmd(gap: &'static [u8], pixel: [u8; 4]) -> (Vec<(u8, &'static [u8])>, Vec<u8>) {
    let ctrls: Vec<(u8, &[u8])> = vec![
        (b'a', b"f"),
        (b'i', b"1"),
        (b'f', b"32"),
        (b's', b"1"),
        (b'v', b"1"),
        (b't', b"d"),
        (b'z', gap),
    ];
    (ctrls, b64(&pixel))
}

#[test]
fn a_frame_rectangle_past_the_canvas_is_einval_and_stores_nothing() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let root = images.get(ImageId(1)).unwrap().clone();
    for (edit, echo) in [(None, ""), (Some(b"1".as_slice()), ",r=1")] {
        let (mut ctrls, payload) = push_frame_cmd(b"100", [0x11, 0x22, 0x33, 0x44]);
        ctrls.push((b'x', b"1"));
        if let Some(r) = edit {
            ctrls.push((b'r', r));
        }
        let mut events = Vec::new();
        let resp = handle_complete(
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
            complete(&ctrls, &payload),
        );
        let want =
            format!("\x1b_Gi=1{echo};EINVAL:a=f: frame data rectangle exceeds image bounds\x1b\\");
        assert_eq!(resp, Some(want.into_bytes()), "r={edit:?}");
        assert!(events.is_empty(), "r={edit:?}: {events:?}");
        assert_eq!(images.get(ImageId(1)), Some(&root), "r={edit:?}");
    }
}

#[test]
fn animation_frame_appends_and_emits_frame_transmission() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let (ctrls, payload) = push_frame_cmd(b"100", [0x11, 0x22, 0x33, 0x44]);
    let mut events = Vec::new();
    let resp = handle_complete(
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
        complete(&ctrls, &payload),
    );
    let entry = images.get(ImageId(1)).unwrap();
    assert_eq!(entry.frame_count(), 2, "a=f appends a frame");
    assert_eq!(entry.frame(1).unwrap().pixels, vec![0x11, 0x22, 0x33, 0x44]);
    assert_eq!(entry.frame(1).unwrap().gap_ms, 100, "z=100 → 100ms gap");
    let wire = materialize_image_events(&events, &images, true);
    assert!(
        wire.iter().any(|e| matches!(
            e,
            ImageMsg::Header {
                target: ImageTarget::Frame { number },
                ..
            } if number.get() == 2
        )),
        "the new frame's pixels ship to attached clients, numbered from 1",
    );
    assert!(
        wire.iter()
            .any(|e| matches!(e, ImageMsg::Complete { id: ImageId(1) }))
    );
    assert_eq!(resp, Some(b"\x1b_Gi=1;OK\x1b\\".to_vec()));
}

fn edit_root_frame(
    grid: &mut felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    mode: (u8, &[u8]),
) -> Vec<u8> {
    let ctrls: Vec<(u8, &[u8])> = vec![
        (b'a', b"f"),
        (b'i', b"1"),
        (b'r', b"1"),
        (b'f', b"32"),
        (b's', b"1"),
        (b'v', b"1"),
        mode,
    ];
    dispatch_controls(
        grid,
        images,
        placements,
        &ctrls,
        &b64(&[0x11, 0x22, 0x33, 0x00]),
    );
    images
        .get(ImageId(1))
        .unwrap()
        .frame(0)
        .unwrap()
        .pixels
        .to_vec()
}

#[test]
fn animation_frame_overwrites_with_x_1_and_blends_otherwise() {
    let root = vec![0xAA, 0xBB, 0xCC, 0xDD];
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    assert_eq!(
        edit_root_frame(&mut grid, &mut images, &mut placements, (b'X', b"1")),
        vec![0x11, 0x22, 0x33, 0x00],
        "X=1 replaces the canvas pixels, transparent ones included",
    );

    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    assert_eq!(
        edit_root_frame(&mut grid, &mut images, &mut placements, (b'C', b"1")),
        root,
        "C= is the a=c composition key; on a=f a transparent pixel still blends",
    );
}

fn shipped_chunks(wire: &[ImageMsg]) -> Vec<Bytes> {
    wire.iter()
        .filter_map(|m| match m {
            ImageMsg::Chunk { bytes, .. } => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_frame_edit_leaves_chunks_already_materialized_with_the_old_pixels() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let in_flight = shipped_chunks(&materialize_image_events(
        &[ImageEvent::Transmit(ImageId(1))],
        &images,
        true,
    ));
    assert_eq!(in_flight, [vec![0xAA, 0xBB, 0xCC, 0xDD]]);

    edit_root_frame(&mut grid, &mut images, &mut placements, (b'X', b"1"));

    assert_eq!(
        in_flight,
        [vec![0xAA, 0xBB, 0xCC, 0xDD]],
        "a chunk awaiting its writer keeps the pixels it was cut from"
    );
    let next = materialize_image_events(
        &[ImageEvent::TransmitFrame {
            id: ImageId(1),
            index: 0,
        }],
        &images,
        true,
    );
    assert_eq!(shipped_chunks(&next), [vec![0x11, 0x22, 0x33, 0x00]]);
}

#[test]
fn a_same_frame_compose_copies_the_source_rectangle_as_it_was() {
    let (mut grid, mut images, mut placements) = handler_state();
    let pixels: Vec<u8> = (0..16).collect();
    let ctrls: [(u8, &[u8]); 6] = [
        (b'a', b"t"),
        (b'i', b"1"),
        (b'f', b"32"),
        (b's', b"4"),
        (b'v', b"1"),
        (b't', b"d"),
    ];
    dispatch_controls(
        &mut grid,
        &mut images,
        &mut placements,
        &ctrls,
        &b64(&pixels),
    );
    let compose: [(u8, &[u8]); 7] = [
        (b'a', b"c"),
        (b'i', b"1"),
        (b'r', b"1"),
        (b'c', b"1"),
        (b'w', b"2"),
        (b'x', b"2"),
        (b'C', b"1"),
    ];
    dispatch_controls(&mut grid, &mut images, &mut placements, &compose, b"");
    let root = &images.get(ImageId(1)).unwrap().frame(0).unwrap().pixels;
    assert_eq!(root[..], [0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7]);
}

const OPAQUE: [u8; 4] = [0x51, 0x52, 0x53, 0xFF];

const CLEAR: [u8; 4] = [0x61, 0x62, 0x63, 0x00];

/// Pixel `i` of a 3x2 RGBA canvas whose six pixels all differ, with
/// half alpha so an opaque write is visible in the alpha byte.
fn canvas_pixel(tag: u8, i: u8) -> [u8; 4] {
    [tag, i, i + 0x10, 0x80]
}

fn canvas(tag: u8) -> Vec<u8> {
    (0..6).flat_map(|i| canvas_pixel(tag, i)).collect()
}

fn with_pixels(mut canvas: Vec<u8>, writes: &[(usize, [u8; 4])]) -> Vec<u8> {
    for &(i, px) in writes {
        canvas[i * 4..i * 4 + 4].copy_from_slice(&px);
    }
    canvas
}

/// Image 1 as a 3x2 RGBA root of `canvas(0xA0)` plus a full-canvas
/// frame 2 of `canvas(0xB0)`.
fn two_frame_canvas() -> (
    felis_grid::Grid,
    felis_grid::images::ImageStore,
    felis_grid::images::Placements,
) {
    let (mut grid, mut images, mut placements) = handler_state();
    let root: [(u8, &[u8]); 6] = [
        (b'a', b"t"),
        (b'i', b"1"),
        (b'f', b"32"),
        (b's', b"3"),
        (b'v', b"2"),
        (b't', b"d"),
    ];
    dispatch_controls(
        &mut grid,
        &mut images,
        &mut placements,
        &root,
        &b64(&canvas(0xA0)),
    );
    let second: [(u8, &[u8]); 6] = [
        (b'a', b"f"),
        (b'i', b"1"),
        (b'f', b"32"),
        (b's', b"3"),
        (b'v', b"2"),
        (b'X', b"1"),
    ];
    let reply = dispatch_controls(
        &mut grid,
        &mut images,
        &mut placements,
        &second,
        &b64(&canvas(0xB0)),
    );
    assert_eq!(reply, Some(b"\x1b_Gi=1;OK\x1b\\".to_vec()));
    (grid, images, placements)
}

fn send_frame(
    state: &mut (
        felis_grid::Grid,
        felis_grid::images::ImageStore,
        felis_grid::images::Placements,
    ),
    controls: &[(u8, &[u8])],
    data: &[u8],
) {
    let mut all: Vec<(u8, &[u8])> = vec![(b'a', b"f"), (b'i', b"1")];
    all.extend_from_slice(controls);
    let (grid, images, placements) = state;
    let reply = dispatch_controls(grid, images, placements, &all, &b64(data));
    assert!(
        reply.as_deref().is_some_and(|r| r.ends_with(b";OK\x1b\\")),
        "{controls:?}: {reply:?}",
    );
}

fn frame_pixels_of(images: &felis_grid::images::ImageStore, index: usize) -> Vec<u8> {
    images
        .get(ImageId(1))
        .unwrap()
        .frame(index)
        .unwrap()
        .pixels
        .to_vec()
}

#[test]
fn a_new_frame_blends_its_offset_rectangle_onto_its_c_base_frame_and_ignores_y() {
    let mut state = two_frame_canvas();
    send_frame(
        &mut state,
        &[
            (b'c', b"2"),
            (b'Y', b"4294967295"),
            (b'f', b"32"),
            (b's', b"2"),
            (b'v', b"1"),
            (b'x', b"1"),
            (b'y', b"1"),
        ],
        &[OPAQUE, CLEAR].concat(),
    );
    assert_eq!(state.1.get(ImageId(1)).unwrap().frame_count(), 3);
    assert_eq!(
        frame_pixels_of(&state.1, 2),
        with_pixels(canvas(0xB0), &[(4, OPAQUE)]),
    );
    assert_eq!(frame_pixels_of(&state.1, 1), canvas(0xB0));
}

#[test]
fn a_new_frame_without_c_blends_its_offset_rectangle_onto_a_y_filled_canvas() {
    let mut state = two_frame_canvas();
    send_frame(
        &mut state,
        &[
            (b'Y', b"270544960"),
            (b'f', b"32"),
            (b's', b"1"),
            (b'v', b"2"),
            (b'x', b"2"),
        ],
        &[OPAQUE, CLEAR].concat(),
    );
    let y = [0x10, 0x20, 0x30, 0x40];
    assert_eq!(
        frame_pixels_of(&state.1, 2),
        with_pixels(y.repeat(6), &[(2, OPAQUE)]),
    );
}

#[test]
fn an_in_place_edit_blends_its_offset_rectangle_and_ignores_c_and_y() {
    let mut state = two_frame_canvas();
    send_frame(
        &mut state,
        &[
            (b'r', b"1"),
            (b'c', b"2"),
            (b'Y', b"4294967295"),
            (b'f', b"32"),
            (b's', b"2"),
            (b'v', b"2"),
            (b'x', b"1"),
        ],
        &[OPAQUE, CLEAR, CLEAR, OPAQUE].concat(),
    );
    assert_eq!(state.1.get(ImageId(1)).unwrap().frame_count(), 2);
    assert_eq!(
        frame_pixels_of(&state.1, 0),
        with_pixels(canvas(0xA0), &[(1, OPAQUE), (5, OPAQUE)]),
    );
}

#[test]
fn an_rgb_frame_lands_opaque_on_an_rgba_canvas_when_appended_and_when_edited() {
    let rgb = [0x71, 0x72, 0x73, 0x81, 0x82, 0x83];
    let rgb_rect: [(u8, &[u8]); 3] = [(b'f', b"24"), (b's', b"1"), (b'v', b"2")];

    let mut state = two_frame_canvas();
    send_frame(
        &mut state,
        &[rgb_rect.as_slice(), &[(b'c', b"1"), (b'x', b"1")]].concat(),
        &rgb,
    );
    let rgb_writes = |first: usize| {
        [
            (first, [0x71, 0x72, 0x73, 0xFF]),
            (first + 3, [0x81, 0x82, 0x83, 0xFF]),
        ]
    };
    assert_eq!(
        frame_pixels_of(&state.1, 2),
        with_pixels(canvas(0xA0), &rgb_writes(1)),
    );

    let mut state = two_frame_canvas();
    send_frame(
        &mut state,
        &[rgb_rect.as_slice(), &[(b'r', b"2"), (b'x', b"2")]].concat(),
        &rgb,
    );
    assert_eq!(
        frame_pixels_of(&state.1, 1),
        with_pixels(canvas(0xB0), &rgb_writes(2)),
    );
}

/// The producer-facing half of the frame cap: `a=f` past 4096 answers
/// `ENOTSUP` (kitty's status set has no `ENOSPC`) and leaves the
/// animation exactly as it was, rather than evicting a frame the
/// producer would never learn it had lost.
#[test]
fn an_animation_frame_past_the_cap_is_declined_and_changes_nothing() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    // Filled through the store: the dispatcher's answer is what is
    // under test, not 4095 round trips through the parser.
    for _ in 1..felis_protocol::messages::MAX_IMAGE_FRAMES {
        images
            .push_frame(
                ImageId(1),
                Frame {
                    pixels: vec![0u8; 4].into(),
                    gap_ms: 40,
                },
            )
            .unwrap();
    }

    let (ctrls, payload) = push_frame_cmd(b"100", [0x11, 0x22, 0x33, 0x44]);
    let mut events = Vec::new();
    let resp = handle_complete(
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
        complete(&ctrls, &payload),
    );

    let resp = resp.unwrap();
    assert!(
        resp.starts_with(b"\x1b_Gi=1;ENOTSUP"),
        "got {}",
        String::from_utf8_lossy(&resp),
    );
    assert_eq!(
        images.get(ImageId(1)).unwrap().frame_count(),
        felis_protocol::messages::MAX_IMAGE_FRAMES,
        "a declined frame must not have grown the animation",
    );
    assert!(
        events.is_empty(),
        "nothing is announced for a frame that was not stored, got {events:?}",
    );
}

/// A frame push makes room by evicting other refcount-0 images. The
/// client mirrors the store's byte total against its own session cap,
/// so an eviction it never hears about would leave it counting bytes
/// the daemon has already freed and refusing the next honest header.
#[test]
fn a_frame_push_that_evicts_announces_the_eviction() {
    let mut grid = felis_grid::Grid::new(24, 80);
    let charge = ImageEntry::new(1, 1, ImageFormat::Rgba32, vec![0u8; 4]).byte_len();
    let mut images = felis_grid::images::ImageStore::new(2 * charge);
    let mut placements = felis_grid::images::Placements::new();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    transmit_root(&mut grid, &mut images, &mut placements, b"2");

    let (ctrls, payload) = push_frame_cmd(b"100", [0x11, 0x22, 0x33, 0x44]);
    let mut events = Vec::new();
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
        complete(&ctrls, &payload),
    );

    assert!(
        images.get(ImageId(2)).is_none(),
        "the frame must have cost image 2 its place",
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ImageEvent::Delete { id } if *id == ImageId(2))),
        "the eviction must reach attached clients, got {events:?}",
    );
    assert_eq!(images.get(ImageId(1)).unwrap().frame_count(), 2);
}

#[test]
fn animation_frame_default_gap_is_40ms_and_negative_is_gapless() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let (mut ctrls, payload) = push_frame_cmd(b"0", [1, 2, 3, 4]);
    ctrls.retain(|(k, _)| *k != b'z');
    run(
        &mut grid,
        &mut images,
        &mut placements,
        complete(&ctrls, &payload),
    );
    assert_eq!(images.get(ImageId(1)).unwrap().frame(1).unwrap().gap_ms, 40);
    let (ctrls, payload) = push_frame_cmd(b"-5", [5, 6, 7, 8]);
    run(
        &mut grid,
        &mut images,
        &mut placements,
        complete(&ctrls, &payload),
    );
    assert_eq!(images.get(ImageId(1)).unwrap().frame(2).unwrap().gap_ms, 0);
}

#[test]
fn animation_control_sets_state_loops_and_jumps() {
    use felis_grid::images::AnimationMode;
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    for px in [[1, 1, 1, 1], [2, 2, 2, 2]] {
        let (ctrls, payload) = push_frame_cmd(b"50", px);
        run(
            &mut grid,
            &mut images,
            &mut placements,
            complete(&ctrls, &payload),
        );
    }
    let a: &[(u8, &[u8])] = &[
        (b'a', b"a"),
        (b'i', b"1"),
        (b's', b"3"),
        (b'v', b"3"),
        (b'c', b"2"),
    ];
    let mut events = Vec::new();
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
        complete(a, b""),
    );
    let entry = images.get(ImageId(1)).unwrap();
    assert_eq!(entry.mode(), AnimationMode::Running);
    assert_eq!(entry.current_frame(), 1, "c=2 jumps to 0-based index 1");
    assert!(
        events.iter().any(|e| matches!(
            e,
            ImageEvent::ShowFrame {
                id: ImageId(1),
                index: 1
            }
        )),
        "the jump ships a ShowFrame",
    );
}

#[test]
fn delete_frame_removes_it_and_reships_the_image() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let (ctrls, payload) = push_frame_cmd(b"50", [9, 9, 9, 9]);
    run(
        &mut grid,
        &mut images,
        &mut placements,
        complete(&ctrls, &payload),
    );
    assert_eq!(images.get(ImageId(1)).unwrap().frame_count(), 2);
    let d: &[(u8, &[u8])] = &[(b'a', b"d"), (b'd', b"f"), (b'i', b"1"), (b'r', b"2")];
    let mut events = Vec::new();
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
        complete(d, b""),
    );
    assert_eq!(
        images.get(ImageId(1)).unwrap().frame_count(),
        1,
        "frame removed"
    );
    // Frame removal has no per-frame wire message: the client gets a
    // full rebuild, so a fresh root Header is re-emitted.
    assert!(
        materialize_image_events(&events, &images, true)
            .iter()
            .any(|e| matches!(e, ImageMsg::Header { id: ImageId(1), .. })),
        "frame delete re-ships the image so the client rebuilds its frames",
    );
}

/// A frame too large for its image is `EINVAL` when carried, but from a
/// file the bounds check judges what the file held.
#[cfg(unix)]
#[test]
fn an_oversized_frame_from_a_file_gets_the_file_medium_reply() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let pixels = [0x11; 8];
    let file = tempfile_with_bytes(&pixels);
    let path = b64(file.path().to_str().unwrap().as_bytes());
    let carried = b64(&pixels);
    let mut reply = |medium: &'static [u8], payload: &[u8]| {
        let ctrls: [(u8, &[u8]); 6] = [
            (b'a', b"f"),
            (b'i', b"1"),
            (b'f', b"32"),
            (b's', b"2"),
            (b'v', b"1"),
            (b't', medium),
        ];
        run(
            &mut grid,
            &mut images,
            &mut placements,
            complete(&ctrls, payload),
        )
        .expect("error response emitted")
    };
    assert!(reply(b"d", &carried).starts_with(b"\x1b_Gi=1;EINVAL"));
    assert_eq!(
        reply(b"f", &path),
        b"\x1b_Gi=1;EBADF:Failed to read image file\x1b\\"
    );
}

#[test]
fn animation_frame_on_missing_image_is_not_found() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (mut ctrls, payload) = push_frame_cmd(b"50", [1, 2, 3, 4]);
    for c in &mut ctrls {
        if c.0 == b'i' {
            c.1 = b"99";
        }
    }
    let resp = run(
        &mut grid,
        &mut images,
        &mut placements,
        complete(&ctrls, &payload),
    )
    .expect("error response emitted");
    assert!(
        resp.starts_with(b"\x1b_Gi=99;ENOENT"),
        "a=f on an unknown image is ENOENT, got: {resp:?}",
    );
}

#[test]
fn compose_copies_a_region_between_frames() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let (ctrls, payload) = push_frame_cmd(b"50", [0xAB, 0xCD, 0xEF, 0xFF]);
    run(
        &mut grid,
        &mut images,
        &mut placements,
        complete(&ctrls, &payload),
    );
    let c: &[(u8, &[u8])] = &[
        (b'a', b"c"),
        (b'i', b"1"),
        (b'r', b"2"),
        (b'c', b"1"),
        (b'C', b"1"),
    ];
    run(&mut grid, &mut images, &mut placements, complete(c, b""));
    assert_eq!(
        images.get(ImageId(1)).unwrap().frame(0).unwrap().pixels,
        vec![0xAB, 0xCD, 0xEF, 0xFF],
    );
}

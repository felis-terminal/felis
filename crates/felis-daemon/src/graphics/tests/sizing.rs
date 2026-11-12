use super::*;

#[test]
fn transmit_and_display_advances_cursor_by_cells() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = tiny_rgba_cmd(&[
        (b'a', b"T".as_slice()),
        (b'i', b"50".as_slice()),
        (b'c', b"4".as_slice()),
        (b'r', b"2".as_slice()),
    ]);
    run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    let cur = grid.cursor();
    assert_eq!(
        (cur.row, cur.col),
        (1, 4),
        "cursor lands right of the image on its last row (r=2, c=4)",
    );
}

#[test]
fn transmit_and_display_with_no_cursor_move_keeps_cursor() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = tiny_rgba_cmd(&[
        (b'a', b"T".as_slice()),
        (b'i', b"51".as_slice()),
        (b'c', b"4".as_slice()),
        (b'r', b"2".as_slice()),
        (b'C', b"1".as_slice()),
    ]);
    run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    let cur = grid.cursor();
    assert_eq!((cur.row, cur.col), (0, 0), "C=1 → cursor stays put");
    assert_eq!(placements.len(), 1);
}

#[test]
fn natural_sizing_resolves_cells_from_image_pixels_and_advances_cursor() {
    // Pins: `c=0 r=0` still advances the cursor past the natural cell
    // box; otherwise the next prompt prints under the image and
    // z-ordering hides it (the pixcat burn-in bug).
    let (mut grid, mut images, mut placements) = handler_state();
    let pixels = vec![0u8; 32 * 16 * 4];
    let payload = b64(&pixels);
    let ctrls = [
        (b'f', b"32".as_slice()),
        (b's', b"32".as_slice()),
        (b'v', b"16".as_slice()),
        (b't', b"d".as_slice()),
    ];
    let mut all = vec![(b'a', b"T".as_slice()), (b'i', b"42".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 8,
            cell_pixel_h: 16,
            anchor_cursor: None,
        },
        cmd,
    )
    .unwrap();
    let cur = grid.cursor();
    assert_eq!(
        (cur.row, cur.col),
        (0, 4),
        "natural sizing must advance cursor past the resolved cell box",
    );
    let p = placements.iter().next().expect("placement recorded");
    assert_eq!(p.extent(), (4, 1));
}

#[test]
fn natural_sizing_resolves_from_the_source_rectangle() {
    let (mut grid, mut images, mut placements) = handler_state();
    let payload = b64(&vec![0u8; 32 * 16 * 4]);
    let cmd = complete(
        &[
            (b'a', b"T".as_slice()),
            (b'i', b"42".as_slice()),
            (b'f', b"32".as_slice()),
            (b's', b"32".as_slice()),
            (b'v', b"16".as_slice()),
            (b'x', b"20".as_slice()),
            (b'y', b"0".as_slice()),
            (b'w', b"30".as_slice()),
            (b'h', b"16".as_slice()),
        ],
        &payload,
    );
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 8,
            cell_pixel_h: 16,
            anchor_cursor: None,
        },
        cmd,
    )
    .unwrap();
    let p = placements.iter().next().expect("placement recorded");
    assert_eq!(
        p.extent(),
        (2, 1),
        "the 12 px the rectangle keeps inside the image, not the image's 32 px",
    );
    assert_eq!(grid.cursor().col, 2);
}

/// kitty's `handle_put_command`: each of `x`/`y`/`w`/`h` defaults on
/// its own, so a partial rectangle still crops.
#[test]
fn a_partial_source_rectangle_defaults_each_missing_key() {
    type Case = (
        &'static [(u8, &'static [u8])],
        Option<SourceRect>,
        (u16, u16),
    );
    let cases: [Case; 4] = [
        (&[(b'x', b"40")], Some(rect(40, 0, 100, 50)), (6, 5)),
        (&[(b'w', b"30")], Some(rect(0, 0, 30, 50)), (3, 5)),
        (
            &[(b'y', b"20"), (b'h', b"0")],
            Some(rect(0, 20, 100, 50)),
            (10, 3),
        ),
        (&[], None, (10, 5)),
    ];
    for (keys, want, extent) in cases {
        let (mut grid, mut images, mut placements) = handler_state();
        let payload = b64(&vec![0u8; 100 * 50 * 4]);
        let mut controls: Vec<(u8, &[u8])> = vec![
            (b'a', b"T"),
            (b'i', b"42"),
            (b'f', b"32"),
            (b's', b"100"),
            (b'v', b"50"),
        ];
        controls.extend_from_slice(keys);
        let mut events = Vec::new();
        handle_complete(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut events,
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 10,
                cell_pixel_h: 10,
                anchor_cursor: None,
            },
            complete(&controls, &payload),
        )
        .unwrap();
        let p = placements.iter().next().expect("placement recorded");
        assert_eq!(p.source, want, "{keys:?}");
        assert_eq!(p.extent(), extent, "{keys:?}");
        let sent = events.iter().find_map(|e| match e {
            ImageEvent::Placement { source, .. } => Some(*source),
            _ => None,
        });
        assert_eq!(sent, Some(want), "{keys:?}");
    }
}

const fn rect(x: u32, y: u32, width: u32, height: u32) -> SourceRect {
    SourceRect {
        x,
        y,
        width,
        height,
    }
}

/// kitty's `update_dest_rect`: with only `r=`, the columns follow from
/// the rows' pixel height at the image's aspect ratio.
#[test]
fn a_put_with_only_rows_derives_its_columns_and_reports_the_request() {
    let (mut grid, mut images, mut placements) = handler_state();
    let payload = b64(&vec![0u8; 101 * 100 * 4]);
    let mut events = Vec::new();
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 10,
            cell_pixel_h: 20,
            anchor_cursor: None,
        },
        complete(
            &[
                (b'a', b"T".as_slice()),
                (b'i', b"42".as_slice()),
                (b'f', b"32".as_slice()),
                (b's', b"101".as_slice()),
                (b'v', b"100".as_slice()),
                (b'r', b"2".as_slice()),
            ],
            &payload,
        ),
    )
    .unwrap();
    let p = placements.iter().next().expect("placement recorded");
    assert_eq!(p.extent(), (5, 2), "40.4 px wide is 5 columns of 10 px");
    assert_eq!(grid.cursor().col, 5);
    let sent = events.iter().find_map(|e| match e {
        ImageEvent::Placement {
            cols,
            rows,
            requested,
            ..
        } => Some((*cols, *rows, *requested)),
        _ => None,
    });
    assert_eq!(
        sent,
        Some((
            5,
            2,
            felis_protocol::messages::RequestedExtent { cols: 0, rows: 2 }
        ))
    );
}

#[test]
fn natural_sizing_falls_back_when_cell_pixel_dims_are_zero() {
    // Pins: before any client reports cell pixel dims, the cursor still
    // moves (no advance burns the image in permanently).
    let (mut grid, mut images, mut placements) = handler_state();
    let pixels = vec![0u8; 32 * 16 * 4];
    let payload = b64(&pixels);
    let ctrls = [
        (b'f', b"32".as_slice()),
        (b's', b"32".as_slice()),
        (b'v', b"16".as_slice()),
        (b't', b"d".as_slice()),
    ];
    let mut all = vec![(b'a', b"T".as_slice()), (b'i', b"43".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    let cur = grid.cursor();
    assert_ne!((cur.row, cur.col), (0, 0));
}

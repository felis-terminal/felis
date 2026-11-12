use super::*;

/// docs/reference/protocols/kitty-graphics.md: a placement scrolled into
/// history survives with a non-positive anchor row, and the mirror hears
/// the move via `PlacementsShifted`.
#[tokio::test]
async fn scroll_into_scrollback_retains_placement_and_emits_shift() {
    use crate::graphics::ImageEvent;
    use std::num::NonZeroU16;

    use felis_grid::images::{
        CellPos, Extent, ImageEntry, ImageFormat, ImageId, Placement, PlacementId,
    };

    let mut session = owned_session("read _x");
    session
        .images
        .insert(
            ImageId(7),
            ImageEntry::new(8, 8, ImageFormat::Rgba32, vec![0xAB; 8 * 8 * 4]),
        )
        .expect("insert under cap");
    session.placements.upsert(Placement {
        image_id: ImageId(7),
        placement_id: Some(PlacementId(1)),
        anchor: CellPos { row: 1, col: 1 },
        cols: Extent::Requested(NonZeroU16::new(4).unwrap()),
        rows: Extent::Requested(NonZeroU16::new(4).unwrap()),
        source: None,
        z_index: 0,
        no_cursor_move: false,
        quiet: 0,
        anonymous: false,
        serial: 0,
    });

    let rows = usize::from(session.lock_core().grid.rows());
    let buf = vec![b'\n'; rows + 10];
    {
        let mut core = session.lock_core();
        let core = &mut *core;
        core.parser.advance(&mut core.grid, &buf);
    }
    let core = Arc::clone(&session.core);
    drain_effects(&mut session, &mut core.lock())
        .expect_done()
        .unwrap();

    let scrolled = u32::try_from(session.lock_core().grid.scrollback().len()).expect("fits");
    assert!(scrolled > 0, "newline flood must reach the scrollback");

    let p = session.placements.iter().next().expect("placement kept");
    let expected_row = 1 - i32::try_from(scrolled).unwrap();
    assert_eq!(p.anchor.row, expected_row);

    let shifted: u32 = session
        .image_events
        .iter()
        .filter_map(|m| match m {
            ImageEvent::PlacementsShifted { lines } => Some(*lines),
            _ => None,
        })
        .sum();
    assert_eq!(shifted, scrolled, "client mirror must hear every shift");
    assert!(
        !session
            .image_events
            .iter()
            .any(|m| matches!(m, ImageEvent::PlacementRemoved { .. })),
        "anchor within retained scrollback must not evict",
    );
}

/// docs/reference/protocols/kitty-graphics.md: a scroll of a DECSTBM
/// region moves the placements inside it, re-stated one by one, and
/// leaves the one below it alone.
#[tokio::test]
async fn a_region_scroll_moves_the_placements_inside_the_region() {
    use crate::graphics::ImageEvent;
    use std::num::NonZeroU16;

    use felis_grid::images::{
        CellPos, Extent, ImageEntry, ImageFormat, ImageId, Placement, PlacementId,
    };

    let mut session = owned_session("read _x");
    session
        .images
        .insert(
            ImageId(7),
            ImageEntry::new(8, 8, ImageFormat::Rgba32, vec![0xAB; 8 * 8 * 4]),
        )
        .expect("insert under cap");
    for (id, row) in [(1, 5), (2, 12)] {
        session.placements.upsert(Placement {
            image_id: ImageId(7),
            placement_id: Some(PlacementId(id)),
            anchor: CellPos { row, col: 1 },
            cols: Extent::Requested(NonZeroU16::new(8).unwrap()),
            rows: Extent::Requested(NonZeroU16::new(3).unwrap()),
            source: None,
            z_index: 0,
            no_cursor_move: false,
            quiet: 0,
            anonymous: false,
            serial: 0,
        });
    }
    {
        let mut core = session.lock_core();
        let core = &mut *core;
        core.parser
            .advance(&mut core.grid, b"\x1b[2;10r\x1b[10;1H\n\n");
    }
    let core = Arc::clone(&session.core);
    drain_effects(&mut session, &mut core.lock())
        .expect_done()
        .unwrap();

    let rows: Vec<(u32, i32)> = session
        .placements
        .iter()
        .map(|p| (p.placement_id.unwrap().0, p.anchor.row))
        .collect();
    assert_eq!(rows, [(1, 3), (2, 12)]);
    let restated: Vec<(Option<PlacementId>, i32)> = session
        .image_events
        .iter()
        .filter_map(|m| match m {
            ImageEvent::Placement {
                placement_id,
                anchor_row,
                ..
            } => Some((*placement_id, *anchor_row)),
            ImageEvent::PlacementsShifted { .. } => panic!("the whole table did not move"),
            _ => None,
        })
        .collect();
    assert_eq!(restated, [(Some(PlacementId(1)), 3)]);
}

/// Header → Chunks → Complete must arrive for every image, each placement
/// after its image.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rehydrate_replays_persisted_images_and_placements() {
    use std::num::NonZeroU16;

    use felis_grid::images::{
        CellPos, Extent, ImageEntry, ImageFormat, ImageId, Placement, PlacementId,
    };
    use felis_protocol::messages::ImageMsg;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read _x");

    spawn_daemon(&path, pool.clone(), factory);

    let mut owned = owned_session("read _x");
    owned
        .images
        .insert(
            ImageId(7),
            ImageEntry::new(2, 2, ImageFormat::Rgba32, vec![0xAB; 16]),
        )
        .expect("insert under cap");
    owned.placements.upsert(Placement {
        image_id: ImageId(7),
        placement_id: Some(PlacementId(1)),
        anchor: CellPos { row: 1, col: 1 },
        cols: Extent::Requested(NonZeroU16::new(2).unwrap()),
        rows: Extent::Requested(NonZeroU16::new(2).unwrap()),
        source: None,
        z_index: 0,
        no_cursor_move: false,
        quiet: 0,
        anonymous: false,
        serial: 0,
    });
    let session_task::SessionLifecycle { id: session_id, .. } = session_task::spawn_owned(
        &pool,
        owned,
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(session_id.0),
            live_only: false,
            image_regions: false,
        },
    )
    .await;
    let _ack = reader.next_frame().await.unwrap().expect("ack");

    // Cells before images (docs/reference/ipc.md
    // "Cross-host carrier: SSH stdio"):
    // first-byte latency is RTT + cells, not RTT + images.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut images = Vec::new();
    let mut saw_row_delta = false;
    let mut row_delta_before_first_image = None;
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "rehydrate burst did not end within deadline"
        );
        let frame = tokio::time::timeout(Duration::from_secs(2), reader.next_frame())
            .await
            .expect("read timed out")
            .unwrap()
            .expect("frame");
        if frame.kind == MessageKind::Image.as_u16() {
            let msg: ImageMsg = codec::decode(&frame.body).unwrap();
            if row_delta_before_first_image.is_none() && matches!(msg, ImageMsg::Header { .. }) {
                row_delta_before_first_image = Some(saw_row_delta);
            }
            images.push(msg);
        } else if frame.kind == MessageKind::Grid.as_u16() {
            let msg: GridMsg = codec::decode(&frame.body).unwrap();
            if matches!(msg, GridMsg::RowDelta { .. }) {
                saw_row_delta = true;
            }
            if matches!(msg, GridMsg::RehydrateEnd) {
                break;
            }
        }
    }
    assert_eq!(
        row_delta_before_first_image,
        Some(true),
        "cells-first rehydrate order: at least one RowDelta must \
         arrive before any ImageMsg::Header so first-byte latency is \
         dominated by cell encoding, not image encoding",
    );
    assert!(
        images
            .iter()
            .any(|m| matches!(m, ImageMsg::Header { id, .. } if id.0 == 7))
    );
    assert!(
        images
            .iter()
            .any(|m| matches!(m, ImageMsg::Chunk { id, .. } if id.0 == 7))
    );
    assert!(
        images
            .iter()
            .any(|m| matches!(m, ImageMsg::Complete { id, .. } if id.0 == 7))
    );
    let placement_idx = images
        .iter()
        .position(|m| matches!(m, ImageMsg::Placement { image_id, .. } if image_id.0 == 7))
        .expect("placement message");
    let complete_idx = images
        .iter()
        .position(|m| matches!(m, ImageMsg::Complete { id, .. } if id.0 == 7))
        .expect("complete message");
    assert!(
        placement_idx > complete_idx,
        "Placement must follow the image's Complete frame so the client's \
         store has the image before any placement references it",
    );
}

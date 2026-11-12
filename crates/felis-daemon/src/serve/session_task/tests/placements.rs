use super::*;

const RIS: &[u8] = b"\x1bc";

const SWITCH_PLACE_RIS: &[&[u8]] = &[
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b_Ga=T,f=24,s=1,v=1,c=2,r=1,q=2;AAAA\x1b\\",
    RIS,
];

proptest! {
    /// The primary screen's stash exists exactly while the alternate
    /// screen is up, whatever mix of switches, placements and RIS the
    /// effects carry; RIS also leaves no placement behind.
    #[test]
    fn the_saved_primary_screen_exists_iff_on_the_alternate_screen(
        ops in prop::collection::vec(prop::sample::select(SWITCH_PLACE_RIS), 1..16),
        per_drain in 1usize..4,
    ) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let mut task = SessionTask::for_tests(session_with("read _x"));
        for burst in ops.chunks(per_drain) {
            write_to_grid(&task, &burst.concat());
            task.drain_effects().expect("effects drain");
            let on_alt = task.session.lock_core().grid.on_alternate_screen();
            prop_assert_eq!(task.session.saved_screens.primary.is_some(), on_alt, "{:?}", burst);
            if burst.last() == Some(&RIS) {
                prop_assert!(task.session.placements.is_empty(), "RIS leaves a placement");
            }
        }
    }
}

/// The task's grid size, at `cell_px` per cell.
fn sized_cells(task: &SessionTask, cell_px: (u16, u16)) -> PtySize {
    let (rows, cols) = {
        let core = task.session.lock_core();
        (core.grid.rows(), core.grid.cols())
    };
    PtySize {
        rows,
        cols,
        pixel_width: cols * cell_px.0,
        pixel_height: rows * cell_px.1,
    }
}

/// A 30×40 px RGB image, placed with `extra` keys.
fn place_30x40(task: &SessionTask, id: u32, extra: &str) {
    let payload = "AAAA".repeat(30 * 40);
    let apc = format!("\x1b_Ga=T,f=24,s=30,v=40,i={id},q=2{extra};{payload}\x1b\\");
    write_to_grid(task, apc.as_bytes());
}

fn placement_extent(placements: &felis_grid::images::Placements, id: u32) -> (u16, u16) {
    let p = placements
        .for_image(felis_protocol::ImageId(id))
        .next()
        .expect("placement recorded");
    p.extent()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cell_size_change_re_resolves_a_natural_extent_and_ships_it_while_idle() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_sub, mut rx) = attach_sub(&mut task, false);
    task.apply_size(sized_cells(&task, (10, 20)));
    place_30x40(&task, 1, "");
    place_30x40(&task, 2, ",c=5,r=5");
    task.drain_effects().expect("effects drain");
    assert_eq!(placement_extent(&task.session.placements, 1), (3, 2));
    drain_events(&mut rx);

    task.apply_size(sized_cells(&task, (8, 16)));
    assert_eq!(placement_extent(&task.session.placements, 1), (4, 3));
    assert_eq!(
        placement_extent(&task.session.placements, 2),
        (5, 5),
        "an explicit c=/r= is not re-resolved",
    );
    assert!(
        drain_events(&mut rx).iter().any(|ev| matches!(
            ev,
            OutEvent::Image(ImageMsg::Placement {
                image_id: felis_protocol::ImageId(1),
                cols: 4,
                rows: 3,
                ..
            })
        )),
        "the resize itself ships the new extent, with no PTY output to carry it",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_without_pixel_dimensions_keeps_a_natural_extent() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    task.apply_size(sized_cells(&task, (10, 20)));
    place_30x40(&task, 1, "");
    task.drain_effects().expect("effects drain");

    task.apply_size(sized_cells(&task, (0, 0)));
    assert_eq!(
        placement_extent(&task.session.placements, 1),
        (3, 2),
        "the 1 px rule would grow the image over text printed past it",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_re_put_with_an_explicit_size_is_not_re_resolved() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    task.apply_size(sized_cells(&task, (10, 20)));
    place_30x40(&task, 1, ",p=1");
    write_to_grid(&task, b"\x1b_Ga=p,i=1,p=1,c=5,r=5,q=2\x1b\\");
    task.drain_effects().expect("effects drain");
    assert_eq!(placement_extent(&task.session.placements, 1), (5, 5));

    task.apply_size(sized_cells(&task, (8, 16)));
    assert_eq!(placement_extent(&task.session.placements, 1), (5, 5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cell_width_change_re_resolves_only_the_width() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_sub, mut rx) = attach_sub(&mut task, false);
    task.apply_size(sized_cells(&task, (10, 20)));
    place_30x40(&task, 1, ",r=4");
    task.drain_effects().expect("effects drain");
    // r=4 of 20 px rows is 80 px tall, so the 30 x 40 image is 60 px
    // wide: 6 columns of 10 px, then 8 columns of 8 px.
    assert_eq!(placement_extent(&task.session.placements, 1), (6, 4));
    drain_events(&mut rx);

    task.apply_size(sized_cells(&task, (8, 20)));
    assert_eq!(placement_extent(&task.session.placements, 1), (8, 4));
    assert!(
        drain_events(&mut rx).iter().any(|ev| matches!(
            ev,
            OutEvent::Image(ImageMsg::Placement {
                image_id: felis_protocol::ImageId(1),
                cols: 8,
                rows: 4,
                ..
            })
        )),
        "the new width is shipped",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_cell_size_re_resolves_an_extent_placed_at_the_nominal_cell() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    place_30x40(&task, 1, "");
    task.drain_effects().expect("effects drain");
    assert_eq!(placement_extent(&task.session.placements, 1), (4, 3));

    task.apply_size(sized_cells(&task, (10, 20)));
    assert_eq!(placement_extent(&task.session.placements, 1), (3, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_saved_primary_screen_follows_a_cell_size_change_on_the_alternate_one() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_sub, mut rx) = attach_sub(&mut task, false);
    task.apply_size(sized_cells(&task, (10, 20)));
    place_30x40(&task, 1, "");
    write_to_grid(&task, b"\x1b[?1049h");
    task.drain_effects().expect("effects drain");

    task.apply_size(sized_cells(&task, (8, 16)));
    let saved = task
        .session
        .saved_screens
        .primary
        .as_ref()
        .expect("primary placements stashed");
    assert_eq!(placement_extent(&saved.placements, 1), (4, 3));

    drain_events(&mut rx);
    write_to_grid(&task, b"\x1b[?1049l");
    task.drain_and_fan().expect("effects drain");
    task.ship_all();
    assert!(
        drain_events(&mut rx).iter().any(|ev| matches!(
            ev,
            OutEvent::Image(ImageMsg::Placement {
                image_id: felis_protocol::ImageId(1),
                cols: 4,
                rows: 3,
                ..
            })
        )),
        "leaving the alternate screen re-states the rescaled extent",
    );
}

/// REQ-307 / REQ-1012: animation frames advance daemon-side
/// regardless of client attachment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn animation_advances_with_no_subscriber_attached() {
    use felis_grid::images::{Frame, ImageEntry};
    use felis_protocol::ImageId;
    use felis_protocol::messages::ImageFormat;

    let mut task = SessionTask::for_tests(session_with("read _x"));
    let id = ImageId(7);
    {
        let images = &mut task.session.images;
        images
            .insert(
                id,
                ImageEntry::new(1, 1, ImageFormat::Rgba32, vec![0xFF; 4]),
            )
            .unwrap();
        for _ in 0..2 {
            images
                .push_frame(
                    id,
                    Frame {
                        pixels: vec![0xFF; 4].into(),
                        gap_ms: 20,
                    },
                )
                .unwrap();
        }
        // `advance_animations` skips unplaced images.
        assert!(images.retain(id), "fixture image must be placed");
    }

    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
    let shutdown = Arc::clone(&task.shutdown);
    let loop_task = tokio::spawn(run_session(task, cmd_rx));

    // Real time, not tokio's paused clock: `graphics::anim_now_ms`
    // is `Instant`-based.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (reply_tx, reply_rx) = oneshot::channel();
    cmd_tx
        .send(SessionCmd::Subscribe(SubscribeReq {
            mode: ConnectionMode::Window,
            pull_paced: false,
            minor: felis_protocol::preface::PROTOCOL_MINOR,
            if_exited: IfExited::Land,
            image_regions: false,
            carrier: AttachCarrier::Local { client_pid: None },
            tx,
            buffered: Arc::new(AtomicUsize::new(0)),
            reply: reply_tx,
        }))
        .await
        .map_err(|_| ())
        .expect("session task accepts the subscribe");
    reply_rx
        .await
        .expect("subscribe reply")
        .expect("the subscribe was admitted");

    let mut shown = None;
    loop {
        match rx.recv().await.expect("rehydrate event") {
            OutEvent::Image(ImageMsg::ShowFrame { id: img, number }) if img == id => {
                shown = Some(number.get());
            }
            OutEvent::Grid(GridMsg::RehydrateEnd) => break,
            _ => {}
        }
    }
    assert!(
        matches!(shown, Some(number) if number > 1),
        "a reattach must land on the live frame; got {shown:?}",
    );

    let _delivered = shutdown.send(&cmd_tx).await;
    loop_task.await.expect("session loop ends cleanly");
}

fn image_targets(events: &[OutEvent]) -> Vec<felis_protocol::messages::ImageTarget> {
    events
        .iter()
        .filter_map(|ev| match ev {
            OutEvent::Image(ImageMsg::Header { target, .. }) => Some(*target),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_edit_reaches_only_an_opted_in_window_at_the_region_minor_as_a_region() {
    use felis_protocol::messages::{ImageTarget, SourceRect};

    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (new, mut rx_new) = attach_sub(&mut task, false);
    let (old, mut rx_old) = attach_sub(&mut task, false);
    let (_off, mut rx_off) = attach_sub(&mut task, false);
    for (sub, minor) in [
        (new, felis_protocol::preface::PROTOCOL_MINOR),
        (old, felis_protocol::minor::IMAGE_HEADER_REGION.minor - 1),
    ] {
        let idx = task.sub_index(sub).expect("subscriber present");
        task.subs[idx].image_regions = true;
        task.subs[idx].minor = minor;
    }
    write_to_grid(
        &task,
        b"\x1b_Ga=t,f=32,s=4,v=2,i=1,q=2;AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\x1b\\",
    );
    task.drain_and_fan().expect("effects drain");
    drain_events(&mut rx_new);
    drain_events(&mut rx_old);
    drain_events(&mut rx_off);

    write_to_grid(
        &task,
        b"\x1b_Ga=f,i=1,r=1,x=1,y=1,s=1,v=1,f=32,q=2;/////w==\x1b\\",
    );
    task.drain_and_fan().expect("effects drain");

    let number = std::num::NonZeroU32::MIN;
    let rect = SourceRect {
        x: 1,
        y: 1,
        width: 1,
        height: 1,
    };
    assert_eq!(
        image_targets(&drain_events(&mut rx_new)),
        [ImageTarget::Region { number, rect }]
    );
    assert_eq!(
        image_targets(&drain_events(&mut rx_old)),
        [ImageTarget::Frame { number }],
        "a window below the region's minor gets the whole frame"
    );
    assert_eq!(
        image_targets(&drain_events(&mut rx_off)),
        [ImageTarget::Frame { number }],
        "a window that did not opt in gets the whole frame"
    );
}

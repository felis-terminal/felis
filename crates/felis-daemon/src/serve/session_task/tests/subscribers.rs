use super::*;

/// An `Ops` attach burst carries only the markers and the keyboard
/// state; a window burst carries rows, meta, and the image store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ops_attach_burst_holds_no_session_content_a_window_burst_holds_it_all() {
    use felis_grid::images::ImageEntry;
    use felis_protocol::ImageId;
    use felis_protocol::messages::ImageFormat;

    let mut task = SessionTask::for_tests(session_with("read _x"));
    {
        let mut core = task.session.lock_core();
        let mut parser = felis_vt::Parser::new();
        parser.advance(
            &mut core.grid,
            b"\x1b]0;fixture\x07\x1b]133;A\x07hello world\r\n",
        );
    }
    task.session
        .images
        .insert(
            ImageId(7),
            ImageEntry::new(1, 1, ImageFormat::Rgba32, vec![0xAA, 0xBB, 0xCC, 0xDD]),
        )
        .expect("insert fixture image");

    let ops = attach_burst(&mut task, ConnectionMode::Ops);
    let shapes: Vec<&str> = ops
        .iter()
        .map(|ev| match ev {
            OutEvent::Grid(GridMsg::RehydrateBegin) => "begin",
            OutEvent::Grid(GridMsg::RehydrateEnd) => "end",
            OutEvent::Grid(GridMsg::KittyKbdFlags { .. }) => "kbd",
            OutEvent::Grid(GridMsg::ModeFlags { .. }) => "modes",
            OutEvent::Grid(GridMsg::RowDelta { .. }) => "rows",
            OutEvent::Image(_) => "image",
            _ => "other",
        })
        .collect();
    assert_eq!(
        shapes,
        ["begin", "kbd", "modes", "end"],
        "an Ops attach burst must carry nothing but the markers and the keyboard modes"
    );

    let window = attach_burst(&mut task, ConnectionMode::Window);
    assert!(
        window
            .iter()
            .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. }))),
        "a window burst must still replay the grid rows"
    );
    assert!(
        window
            .iter()
            .any(|ev| matches!(ev, OutEvent::Image(ImageMsg::Complete { .. }))),
        "a window burst must still replay the image store"
    );
    assert!(
        window
            .iter()
            .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Title { .. }))),
        "a window burst must still replay the session meta"
    );
}

/// An outbox that still takes the cycle's facets but overflows on its
/// composed rows is evicted in the push after the guard drops, and
/// the healthy peer still receives its whole cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outbox_overflowing_on_its_rows_is_evicted_and_its_peer_still_gets_the_cycle() {
    const HEADROOM: usize = 8 * WIRE_LEN_OVERHEAD;
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (dying, mut rx_dying) = attach_sub(&mut task, true);
    let (healthy, mut rx_healthy) = attach_sub(&mut task, true);
    let dying_idx = task.sub_index(dying).expect("dying subscriber present");
    task.subs[dying_idx]
        .buffered
        .store(SUBSCRIBER_BUFFER_CAP - HEADROOM, Ordering::Relaxed);

    let mut screen = String::from("\x1b]0;renamed\x07\x1b[H\x1b[2J");
    for row in 0..20_u32 {
        for col in 0..60_u32 {
            screen.push(char::from(
                b'!' + u8::try_from((row * 7 + col) % 90).unwrap(),
            ));
        }
        screen.push_str("\r\n");
    }
    write_to_grid(&task, screen.as_bytes());
    for sub in &mut task.subs {
        sub.pull_pending = true;
    }
    task.drain_and_fan().expect("effects drain");

    let dying_events = drain_events(&mut rx_dying);
    assert!(
        dying_events
            .iter()
            .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Title { .. }))),
        "the facet fits the dying outbox, so the failure lands on the rows: {dying_events:?}",
    );
    assert_eq!(
        shipped_rows(&dying_events),
        0,
        "the rows overflowed the cap"
    );
    assert!(
        task.sub_index(dying).is_none(),
        "the overflowing outbox is evicted"
    );
    assert_eq!(task.sub_index(healthy), Some(0));

    let events = drain_events(&mut rx_healthy);
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Title { .. }))),
        "the facet reaches the healthy peer: {events:?}",
    );
    assert!(shipped_rows(&events) > 0, "the rows reach the healthy peer");
    assert!(
        matches!(events.last(), Some(OutEvent::Grid(GridMsg::CycleEnd))),
        "the healthy peer's cycle is closed: {events:?}",
    );
    let meta = task.meta.lock().unwrap();
    assert_eq!(meta.subscribers, 1);
    assert_eq!(meta.title.as_deref(), Some("renamed"));
}

/// A client that refuses a grid frame closes only its own
/// attachment (`docs/reference/ipc.md` "Corruption"): the session
/// and every other subscriber must outlive it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attachment_that_closes_leaves_the_session_and_its_peers_running() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, rx_a) = attach_sub(&mut task, false);
    let (sub_b, mut rx_b) = attach_sub(&mut task, false);

    task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
    task.handle_input(
        sub_b,
        InputMsg::Resize {
            dims: RequestedDims {
                rows: 30,
                cols: 120,
                pixel_w: 0,
                pixel_h: 0,
            },
        },
    );
    drain_events(&mut rx_b);
    // The refusing client drops its connection.
    drop(rx_a);

    let end = task.handle_input(sub_b, InputMsg::KeyBytes(b"b".to_vec()));
    assert!(
        end.is_none(),
        "one lost attachment must not end the session"
    );
    assert!(
        !task.subs.iter().any(|sub| sub.id == sub_a),
        "the closed attachment must be dropped from the roster",
    );
    assert_eq!(
        {
            let core = task.session.lock_core();
            (core.grid.rows(), core.grid.cols())
        },
        (30, 120),
        "the surviving subscriber still drives the authoritative grid",
    );
    assert!(
        drain_events(&mut rx_b).iter().any(|ev| matches!(
            ev,
            OutEvent::Grid(GridMsg::Size {
                dims: GridDims {
                    rows: 30,
                    cols: 120,
                    ..
                }
            })
        )),
        "the surviving mirror must keep receiving frames",
    );
}

/// `EvictAll`: every subscriber receives `Evicted` and its outbox
/// closes; the task keeps running parked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evict_all_ships_evicted_to_every_subscriber() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_a, mut rx_a) = attach_sub(&mut task, false);
    let (_b, mut rx_b) = attach_sub(&mut task, false);

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let end = task.handle_cmd(SessionCmd::EvictAll { reply: reply_tx });
    assert!(end.is_none(), "EvictAll must not end the task");
    assert_eq!(reply_rx.try_recv().expect("reply"), 2);
    for rx in [&mut rx_a, &mut rx_b] {
        let evicted = drain_events(rx)
            .iter()
            .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::Evicted { .. })))
            .count();
        assert_eq!(evicted, 1, "each subscriber hears exactly one Evicted");
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)),
            "outbox must close after eviction",
        );
    }
    assert!(task.subs.is_empty());
    assert_eq!(task.meta.lock().unwrap().subscribers, 0);
}

#[cfg(unix)]
#[test]
fn exit_status_to_wire_separates_exit_codes_from_terminating_signals() {
    use std::os::unix::process::ExitStatusExt as _;
    let wire = |raw| exit_status_to_wire(std::process::ExitStatus::from_raw(raw));
    assert_eq!(wire(0), Some(ExitStatus::Code(0)));
    assert_eq!(wire(3 << 8), Some(ExitStatus::Code(3)));
    assert_eq!(wire(255 << 8), Some(ExitStatus::Code(255)));
    assert_eq!(wire(9), Some(ExitStatus::Signal(9)));
    assert_eq!(
        wire(15 | 0x80),
        Some(ExitStatus::Signal(15)),
        "a core dump is still a signal"
    );
}

/// Shell exit ships `SessionExited`, carrying the session's own id,
/// to window subscribers only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_shell_exit_ships_session_exited_only_to_window_subscribers() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_window, mut rx_window) = attach_sub(&mut task, false);
    let (_ops, mut rx_ops) = attach_ops_sub(&mut task);

    task.notify_shell_exit(Some(ExitStatus::Code(3)));

    let pushed: Vec<_> = drain_events(&mut rx_window)
        .into_iter()
        .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::SessionExited { .. })))
        .collect();
    assert_eq!(pushed.len(), 1, "the window hears one SessionExited");
    assert!(
        matches!(
            pushed[0],
            OutEvent::Push(PushMsg::SessionExited {
                id,
                status: Some(ExitStatus::Code(3)),
            }) if id == task.id.0
        ),
        "SessionExited carries this session's id and the status it was given",
    );
    assert!(
        drain_events(&mut rx_ops).is_empty(),
        "a scripted Ops attach has no window to keep open, so it gets no SessionExited",
    );
}

/// `felis -- true` can exit before the create's own attach lands.
#[tokio::test]
async fn the_creating_attach_hears_an_exit_that_beat_it_and_a_deliberate_one_stays() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    task.notify_shell_exit(Some(ExitStatus::Code(3)));

    let attach = |task: &mut SessionTask, mode, if_exited| {
        let (_, mut rx) = attach_sub_if_exited(task, mode, false, if_exited);
        let exits = drain_events(&mut rx)
            .into_iter()
            .filter_map(|ev| match ev {
                OutEvent::Push(PushMsg::SessionExited { status, .. }) => Some(status),
                _ => None,
            })
            .collect::<Vec<_>>();
        let closed = matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected));
        (exits, closed)
    };
    assert_eq!(
        attach(&mut task, ConnectionMode::Window, IfExited::Notify),
        (vec![Some(ExitStatus::Code(3))], true),
        "the creating window hears the exit it lost the race to, then the close",
    );
    assert_eq!(
        attach(&mut task, ConnectionMode::Ops, IfExited::Notify),
        (vec![], true),
        "a scripted create sees only the close",
    );
    assert_eq!(
        attach(&mut task, ConnectionMode::Window, IfExited::Land),
        (vec![], false),
        "a deliberate attach stays to read the final screen",
    );
    assert_eq!(task.subs.len(), 1);
}

/// A subscriber whose backlog crosses `SUBSCRIBER_BUFFER_CAP` is
/// evicted on its next ship while a healthy peer keeps receiving.
/// The gauge is pre-loaded to just under the cap rather than
/// materializing the backlog.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_subscriber_is_evicted_while_healthy_peer_keeps_streaming() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_slow, mut rx_slow) = attach_sub(&mut task, false);
    let (sub_healthy, mut rx_healthy) = attach_sub(&mut task, false);

    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut task.session.lock_core().grid, b"hello world\r\n");

    let slow_idx = task.sub_index(sub_slow).expect("slow subscriber present");
    task.subs[slow_idx].buffered.store(
        SUBSCRIBER_BUFFER_CAP - WIRE_LEN_OVERHEAD + 1,
        Ordering::Relaxed,
    );

    task.fan_out_grid_state();
    task.ship_all();

    assert!(
        task.sub_index(sub_slow).is_none(),
        "over-cap subscriber must be evicted",
    );
    assert!(
        matches!(
            rx_slow.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected),
        ),
        "evicted subscriber's outbox must close",
    );

    assert_eq!(task.subs.len(), 1, "only the healthy peer remains");
    assert_eq!(
        task.sub_index(sub_healthy),
        Some(0),
        "the surviving subscriber is the healthy peer",
    );
    let healthy_rows = drain_events(&mut rx_healthy)
        .iter()
        .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. })))
        .count();
    assert!(
        healthy_rows >= 1,
        "the healthy peer keeps receiving frames despite its peer's eviction",
    );
    assert_eq!(task.meta.lock().unwrap().subscribers, 1);
}

fn switch_push(task: &mut SessionTask, scope: SwitchScope) -> PushOutcome {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let end = task.handle_cmd(SessionCmd::PushToTarget {
        msg: PushMsg::Reattach { id: 0xBEEF },
        scope,
        reply: reply_tx,
    });
    assert!(end.is_none(), "a switch push must not end the task");
    reply_rx.try_recv().expect("reply")
}

fn reattach_count(rx: &mut mpsc::UnboundedReceiver<OutEvent>) -> usize {
    drain_events(rx)
        .iter()
        .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::Reattach { .. })))
        .count()
}

fn roster(task: &SessionTask) -> Vec<Attachment> {
    task.meta
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .attachments
        .clone()
}

/// The default scope resolves to the window whose input last
/// reached the PTY, and to that window alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_scope_targets_the_last_window_input_owner() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_mirror, mut rx_mirror) = attach_sub(&mut task, false);
    let (typed_in, mut rx_typed_in) = attach_sub(&mut task, false);

    task.handle_input(typed_in, InputMsg::KeyBytes(b"b".to_vec()));
    assert_eq!(task.input_owner, Some(typed_in));
    drain_events(&mut rx_mirror);
    drain_events(&mut rx_typed_in);

    assert_eq!(queued(switch_push(&mut task, SwitchScope::Default)), 1,);
    assert_eq!(
        reattach_count(&mut rx_typed_in),
        1,
        "the owner hears the push",
    );
    assert_eq!(
        reattach_count(&mut rx_mirror),
        0,
        "a mirror the user did not type in stays where it is",
    );
}

/// With the marker clear, a session with exactly one window
/// resolves to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_scope_falls_back_to_a_sole_window() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_sole, mut rx) = attach_sub(&mut task, false);
    drain_events(&mut rx);
    assert_eq!(task.input_owner, None, "nothing has typed yet");

    assert_eq!(queued(switch_push(&mut task, SwitchScope::Default)), 1,);
    assert_eq!(reattach_count(&mut rx), 1);
}

/// With the marker clear and two windows attached, the default
/// scope is refused by name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_scope_refuses_two_windows_with_no_input_owner() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_a, mut rx_a) = attach_sub(&mut task, false);
    let (_b, mut rx_b) = attach_sub(&mut task, false);

    assert_eq!(
        switch_push(&mut task, SwitchScope::Default),
        PushOutcome::Denied(SwitchDenied::NoInputOwner),
    );
    assert_eq!(reattach_count(&mut rx_a) + reattach_count(&mut rx_b), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ops_input_takes_size_ownership_but_not_the_switch_target() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (window, mut rx_window) = attach_sub(&mut task, false);
    let (ops, _rx_ops) = attach_ops_sub(&mut task);

    task.handle_input(window, InputMsg::KeyBytes(b"w".to_vec()));
    task.handle_input(ops, InputMsg::KeyBytes(b"o".to_vec()));

    assert_eq!(
        task.active_sub,
        Some(ops),
        "size ownership follows any input that reaches the PTY",
    );
    assert_eq!(
        task.input_owner,
        Some(window),
        "the switch target stays with the window that typed",
    );
    drain_events(&mut rx_window);
    assert_eq!(queued(switch_push(&mut task, SwitchScope::Default)), 1,);
    assert_eq!(reattach_count(&mut rx_window), 1);
}

/// The owning window detaching clears the marker, so the sole
/// survivor becomes the target under the fallback rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_detaching_clears_the_marker_and_the_survivor_takes_over() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (owner, _rx_owner) = attach_sub(&mut task, false);
    let (_survivor, mut rx_survivor) = attach_sub(&mut task, false);

    task.handle_input(owner, InputMsg::KeyBytes(b"o".to_vec()));
    assert_eq!(task.input_owner, Some(owner));

    task.remove_sub(owner);
    assert_eq!(task.input_owner, None, "the marker clears with its owner");

    drain_events(&mut rx_survivor);
    assert_eq!(queued(switch_push(&mut task, SwitchScope::Default)), 1,);
    assert_eq!(reattach_count(&mut rx_survivor), 1);
}

/// An explicit attachment overrules the marker; a stale id is
/// refused by name, never redirected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_attachment_targets_that_window_and_a_stale_id_is_refused() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (owner, mut rx_owner) = attach_sub(&mut task, false);
    let (other, mut rx_other) = attach_sub(&mut task, false);
    task.handle_input(owner, InputMsg::KeyBytes(b"o".to_vec()));
    drain_events(&mut rx_owner);
    drain_events(&mut rx_other);

    assert_eq!(
        queued(switch_push(&mut task, SwitchScope::Attachment(other.0))),
        1,
    );
    assert_eq!(reattach_count(&mut rx_other), 1, "the named window moves");
    assert_eq!(reattach_count(&mut rx_owner), 0, "the marker is overruled");

    let gone = other.0;
    task.remove_sub(other);
    assert_eq!(
        switch_push(&mut task, SwitchScope::Attachment(gone)),
        PushOutcome::Denied(SwitchDenied::NoSuchAttachment { attachment: gone }),
    );
    assert_eq!(
        reattach_count(&mut rx_owner),
        0,
        "a stale id must not fall back to a surviving window",
    );
}

/// The roster carries one stamped, flagged row per window
/// attachment; a marker transfer flips `input_owner` without
/// changing the subscriber count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_roster_reports_attachments_and_flips_the_owner_on_transfer() {
    let before = SystemTime::now();
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, _rx_a) = attach_sub(&mut task, false);
    let (sub_b, _rx_b) = attach_sub(&mut task, false);
    let (_ops, _rx_ops) = attach_ops_sub(&mut task);

    let rows = roster(&task);
    assert_eq!(
        rows.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![sub_a.0, sub_b.0],
        "window attachments only, in attach order",
    );
    assert!(
        rows.iter().all(|a| a.attached_at >= before),
        "every attachment carries the instant its attach landed",
    );
    assert!(
        rows.iter().all(|a| !a.input_owner),
        "nothing has typed, so no attachment claims the marker",
    );

    task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
    let owners: Vec<u64> = roster(&task)
        .iter()
        .filter(|a| a.input_owner)
        .map(|a| a.id)
        .collect();
    assert_eq!(owners, vec![sub_a.0]);

    let count_before = task.subs.len();
    task.handle_input(sub_b, InputMsg::KeyBytes(b"b".to_vec()));
    assert_eq!(
        task.subs.len(),
        count_before,
        "a transfer changes no subscriber count",
    );
    let owners: Vec<u64> = roster(&task)
        .iter()
        .filter(|a| a.input_owner)
        .map(|a| a.id)
        .collect();
    assert_eq!(owners, vec![sub_b.0], "the marker moved with the input");
}

/// Attachment ids are daemon-global and never reused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_ids_are_never_reused_across_sessions() {
    let ids = AttachmentIds::detached();
    let mut first = SessionTask::for_tests(session_with("read _x"));
    first.attachment_ids = ids.clone();
    let mut second = SessionTask::for_tests(session_with("read _x"));
    second.attachment_ids = ids;

    let (a, _rx_a) = attach_sub(&mut first, false);
    let (b, _rx_b) = attach_sub(&mut second, false);
    first.remove_sub(a);
    let (c, _rx_c) = attach_sub(&mut second, false);

    assert_ne!(a.0, b.0, "two sessions' windows carry distinct ids");
    assert_ne!(a.0, c.0, "a departed window's id is never handed out again");
    assert_ne!(b.0, c.0);
}

/// A `Reattach` push reaches the window subscriber and leaves it
/// subscribed; an `Ops` attach is skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reattach_push_reaches_only_window_subscribers() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_window, mut rx_capable) = attach_sub(&mut task, false);
    let (_ops, mut rx_legacy) = attach_ops_sub(&mut task);

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let end = task.handle_cmd(SessionCmd::PushToTarget {
        msg: PushMsg::Reattach { id: 0xBEEF },
        scope: SwitchScope::Default,
        reply: reply_tx,
    });
    assert!(end.is_none(), "the switch push must not end the task");
    assert_eq!(
        queued(reply_rx.try_recv().expect("reply")),
        1,
        "only the window subscriber counts as queued",
    );
    let reattaches = drain_events(&mut rx_capable)
        .iter()
        .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::Reattach { id: 0xBEEF, .. })))
        .count();
    assert_eq!(reattaches, 1, "the window hears one Reattach");
    assert!(
        drain_events(&mut rx_legacy)
            .iter()
            .all(|ev| !matches!(ev, OutEvent::Push(PushMsg::Reattach { .. }))),
        "a scripted Ops attach must not be moved",
    );
    assert_eq!(
        task.subs.len(),
        2,
        "both subscribers stay attached — the push does not evict",
    );
}

/// A `RetargetHost` push reaches the window subscriber and leaves
/// it subscribed; an `Ops` attach is skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retarget_push_reaches_only_window_subscribers() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_window, mut rx_capable) = attach_sub(&mut task, false);
    let (_ops, mut rx_switch_only) = attach_ops_sub(&mut task);

    let target = RetargetTarget {
        carrier: RetargetCarrier::Ssh {
            destination: "user@devbox".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        },
        landing: RetargetLanding::Create(SpawnArgs::default()),
    };
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let end = task.handle_cmd(SessionCmd::PushToTarget {
        msg: PushMsg::RetargetHost {
            target: target.clone(),
        },
        scope: SwitchScope::Default,
        reply: reply_tx,
    });
    assert!(end.is_none(), "the retarget push must not end the task");
    assert_eq!(
        queued(reply_rx.try_recv().expect("reply")),
        1,
        "only the window subscriber counts as queued",
    );
    let retargets: Vec<_> = drain_events(&mut rx_capable)
        .into_iter()
        .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::RetargetHost { .. })))
        .collect();
    assert_eq!(retargets.len(), 1, "the window hears one RetargetHost");
    assert!(
        matches!(
            &retargets[0],
            OutEvent::Push(PushMsg::RetargetHost { target: t, .. }) if *t == target
        ),
        "RetargetHost relays the target verbatim",
    );
    assert!(
        drain_events(&mut rx_switch_only)
            .iter()
            .all(|ev| !matches!(ev, OutEvent::Push(PushMsg::RetargetHost { .. }))),
        "a scripted Ops attach must not be re-dialed",
    );
    assert_eq!(task.subs.len(), 2, "the push does not evict");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_attached_bit_ignores_ops_subscribers() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let mut hub_rx = task.hub.subscribe();
    let (_ops, _rx_ops) = attach_ops_sub(&mut task);

    {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser
            .advance(&mut core.grid, b"\x1b]99;u=1;ops only\x1b\\");
    }
    task.after_drive();
    match hub_rx.try_recv().expect("event published") {
        NotifyToClientMsg::Event { attached, .. } => {
            assert!(!attached, "an Ops reader is not an attached window");
        }
        other => panic!("expected NotifyToClientMsg::Event, got {other:?}"),
    }

    let (_win, _rx_win) = attach_sub(&mut task, false);
    {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser
            .advance(&mut core.grid, b"\x1b]99;u=1;window now\x1b\\");
    }
    task.after_drive();
    loop {
        match hub_rx.try_recv().expect("event published") {
            NotifyToClientMsg::Event {
                attached,
                notification,
                ..
            } if notification.title.as_deref() == Some("window now") => {
                assert!(attached, "a window subscriber sets the bit");
                break;
            }
            _ => {}
        }
    }
}

/// The `subscriber_queue_bytes` subject sample is the deepest outbox
/// (a max, not a sum) and moves with real pushes, while the sibling
/// the daemon-wide total is built from is every outbox summed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_report_the_deepest_subscriber_backlog() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    assert_eq!(
        task.stats().max_subscriber_backlog,
        0,
        "no subscriber, no backlog"
    );

    let (shallow, _rx_shallow) = attach_sub(&mut task, false);
    let (deep, _rx_deep) = attach_sub(&mut task, false);
    let after_attach = task.stats().max_subscriber_backlog;
    assert!(
        after_attach > 0,
        "an undrained attach burst is queued bytes: {after_attach}"
    );

    let deep_idx = task.sub_index(deep).expect("deep subscriber present");
    task.subs[deep_idx]
        .buffered
        .store(after_attach + 4096, Ordering::Relaxed);
    let shallow_idx = task.sub_index(shallow).expect("shallow subscriber present");
    let shallow_depth = task.subs[shallow_idx].buffered.load(Ordering::Relaxed);

    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut task.session.lock_core().grid, b"hello world\r\n");
    task.fan_out_grid_state();
    task.ship_all();

    let sample = task.stats().max_subscriber_backlog;
    assert!(
        sample > after_attach + 4096,
        "the row follows the deepest outbox and the push grew it: {sample}"
    );
    assert!(
        task.subs[shallow_idx].buffered.load(Ordering::Relaxed) > shallow_depth,
        "the shallow peer also grew, so the sample is a max and not a sum"
    );

    let stats = task.stats();
    let shallow_now = task.subs[shallow_idx].buffered.load(Ordering::Relaxed);
    let deep_now = task.subs[deep_idx].buffered.load(Ordering::Relaxed);
    assert_eq!(
        stats.total_subscriber_backlog,
        shallow_now + deep_now,
        "the sibling field is every outbox summed, not the deepest one"
    );
    assert_eq!(stats.max_subscriber_backlog, deep_now);
}

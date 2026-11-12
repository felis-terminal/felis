use super::*;

/// Returns `(RowDelta frame count, total dirty rows, Scrolled count)` once
/// `deadline` passes with no new frame.
async fn drive_and_count_grid_frames(
    body: &'static str,
    deadline: Duration,
) -> (usize, usize, usize) {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory(body);

    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(256 * 1024);
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(256 * 1024);

    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        drop(
            handle_stream(
                daemon_read,
                daemon_write,
                DaemonCaps::default(),
                server_pool,
                factory,
            )
            .await,
        );
    });

    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;

    hello_welcome(&mut reader, &mut writer, false).await;

    create_and_attach(&mut reader, &mut writer).await;

    let mut row_delta = 0usize;
    let mut total_dirty_rows = 0usize;
    let mut scrolled = 0usize;
    loop {
        let res = tokio::time::timeout(deadline, reader.next_frame()).await;
        match res {
            Err(_) => break, // deadline elapsed → end of burst
            Ok(Err(_) | Ok(None)) => break,
            Ok(Ok(Some(frame))) => {
                if frame.kind == MessageKind::Grid.as_u16() {
                    let msg: GridMsg = codec::decode(&frame.body).unwrap();
                    match msg {
                        GridMsg::RowDelta { rows } => {
                            row_delta += 1;
                            total_dirty_rows += rows.len();
                        }
                        GridMsg::Scrolled { .. } => {
                            scrolled += 1;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    drop(writer);
    drop(reader);
    drop(server);

    (row_delta, total_dirty_rows, scrolled)
}

/// docs/reference/ipc.md: each diff cycle collapses its dirty rows into one
/// `RowDelta`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn row_delta_coalescing_collapses_burst_frames() {
    let body = "printf 'line %s\\n' $(seq 1 120); sleep 0.2";

    let (frames, rows, _scrolled) =
        drive_and_count_grid_frames(body, Duration::from_millis(500)).await;

    assert!(frames > 0, "run must ship some RowDelta; got 0");

    // Strictly fewer frames than rows, not a fixed ratio: PTY chunking and
    // flush coalescing align differently per run.
    assert!(
        frames < rows,
        "{frames} RowDelta frames for {rows} dirty rows — \
         the per-cycle batch collapsed nothing"
    );
}

/// Same-user mirroring (session-lifecycle.md): attach is additive, and
/// input from either fans in to the one PTY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_subscribers_mirror_one_session() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x; printf 'got=%s\\n' \"$x\"; sleep 0.5");

    spawn_daemon(&path, pool.clone(), factory);

    let (mut reader_a, mut writer_a) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;

    let (mut reader_b, mut writer_b) = dial(&path, ConnectionMode::Window, false).await;
    attach_existing(&mut reader_b, &mut writer_b, id).await;

    let subscribers = pool
        .lock()
        .await
        .get(SessionId(id))
        .expect("session pooled")
        .meta_snapshot()
        .subscribers;
    assert_eq!(
        subscribers, 2,
        "attach must be additive (same-user mirroring)"
    );

    send_input(&mut writer_b, &InputMsg::KeyBytes(b"HELLO\n".to_vec())).await;
    expect_row_containing(&mut reader_a, "got=HELLO").await;
    expect_row_containing(&mut reader_b, "got=HELLO").await;
}

/// A late joiner is rehydrated from the current grid while the existing
/// subscriber stays on incremental diffs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_joiner_rehydrates_output_that_predates_its_attach() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("printf marker-xyz; read _x");

    spawn_daemon(&path, pool, factory);

    let (mut reader_a, mut writer_a) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;
    expect_row_containing(&mut reader_a, "marker-xyz").await;

    let (mut reader_b, mut writer_b) = dial(&path, ConnectionMode::Window, false).await;
    let rows = attach_existing(&mut reader_b, &mut writer_b, id).await;
    assert!(
        rows.iter().any(|r| r.contains("marker-xyz")),
        "late joiner's rehydrate must carry pre-attach output; rows: {rows:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detaching_one_mirror_keeps_the_other_streaming() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x; printf 'got=%s\\n' \"$x\"; sleep 0.5");

    spawn_daemon(&path, pool.clone(), factory);

    let (mut reader_a, mut writer_a) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;

    let (mut reader_b, mut writer_b) = dial(&path, ConnectionMode::Window, false).await;
    attach_existing(&mut reader_b, &mut writer_b, id).await;

    drop(reader_a);
    drop(writer_a);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let subscribers = pool
            .lock()
            .await
            .get(SessionId(id))
            .expect("session stays pooled")
            .meta_snapshot()
            .subscribers;
        if subscribers == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "first mirror's detach never registered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    send_input(&mut writer_b, &InputMsg::KeyBytes(b"HELLO\n".to_vec())).await;
    expect_row_containing(&mut reader_b, "got=HELLO").await;
}

/// The failure unit is the one connection: a refused frame tears down its
/// sender and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_frame_kills_its_own_connection_and_nothing_else() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x; printf 'got=%s\\n' \"$x\"; sleep 0.5");

    spawn_daemon(&path, pool.clone(), factory);

    let (mut reader_a, mut writer_a) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;

    let (mut reader_b, mut writer_b) = dial(&path, ConnectionMode::Window, false).await;
    attach_existing(&mut reader_b, &mut writer_b, id).await;

    writer_a
        .write_frame_unchecked(&Frame {
            kind: 0xBEEF,
            body: b"corrupt",
        })
        .await
        .unwrap();
    writer_a.flush().await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Ok(None) | Err(_)) =
            tokio::time::timeout(Duration::from_millis(200), reader_a.next_frame()).await
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the corrupt frame never closed its own connection"
        );
    }
    drop(reader_a);
    drop(writer_a);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let subscribers = pool
            .lock()
            .await
            .get(SessionId(id))
            .expect("the session must outlive one peer's corruption")
            .meta_snapshot()
            .subscribers;
        if subscribers == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the corrupt peer's detach never registered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    send_input(&mut writer_b, &InputMsg::KeyBytes(b"HELLO\n".to_vec())).await;
    expect_row_containing(&mut reader_b, "got=HELLO").await;
}

/// `ForceDetach` (docs/explanation/architecture/control-surfaces.md) evicts
/// every attached client, keeps the session pooled, and reports
/// `was_attached = true`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_detach_evicts_every_subscriber_and_keeps_the_session() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // `read` is a `sh` builtin, so the session blocks without a program the
    // host must resolve.
    spawn_daemon(&path, pool.clone(), shell_factory("read _x"));

    let (mut reader_a, mut writer_a) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader_a, &mut writer_a).await;
    let (mut reader_a2, mut writer_a2) = dial(&path, ConnectionMode::Window, false).await;
    attach_existing(&mut reader_a2, &mut writer_a2, id).await;

    let (mut reader_c, mut writer_c) = dial(&path, ConnectionMode::Ops, false).await;
    send_request(
        &mut writer_c,
        &OpsToDaemonMsg::ForceDetach { id_prefix: hex(id) },
        1,
    )
    .await;
    let reply = reader_c
        .next_frame()
        .await
        .unwrap()
        .expect("session-detached");
    match codec::decode::<OpsToClientMsg>(&reply.body).unwrap() {
        OpsToClientMsg::Detached {
            resolved,
            was_attached,
        } => {
            assert_eq!(resolved, ResolvedId::Ok { id });
            assert!(was_attached, "attached clients report was_attached=true");
        }
        other => panic!("expected Ops::Detached, got {other:?}"),
    }

    for reader in [&mut reader_a, &mut reader_a2] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_evicted = false;
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(Some(frame))) =
                tokio::time::timeout(Duration::from_secs(2), reader.next_frame()).await
            else {
                break;
            };
            if frame.kind == MessageKind::Push.as_u16()
                && matches!(
                    codec::decode::<PushMsg>(&frame.body).unwrap(),
                    PushMsg::Evicted { .. }
                )
            {
                saw_evicted = true;
                break;
            }
        }
        assert!(
            saw_evicted,
            "every evicted client must receive PushMsg::Evicted"
        );
    }

    let handle = pool.lock().await.handle_cloned(SessionId(id));
    let meta = handle.expect("session stays pooled").meta_snapshot();
    assert_eq!(meta.subscribers, 0, "all subscribers evicted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_detach_idle_session_reports_not_attached() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("read _x"));

    let (mut reader_b, mut writer_b) = dial(&path, ConnectionMode::Ops, false).await;
    // Spawned rather than created: a `Create` would attach this
    // connection, and the session would not be idle.
    send_request(
        &mut writer_b,
        &OpsToDaemonMsg::Spawn {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
        1,
    )
    .await;
    let spawned = reader_b.next_frame().await.unwrap().expect("spawned");
    let id = match codec::decode::<OpsToClientMsg>(&spawned.body).unwrap() {
        OpsToClientMsg::Spawned {
            outcome: SpawnOutcome::Ok { info },
        } => info.id,
        other => panic!("expected a successful Spawned, got {other:?}"),
    };

    send_request(
        &mut writer_b,
        &OpsToDaemonMsg::ForceDetach { id_prefix: hex(id) },
        2,
    )
    .await;
    let reply = reader_b
        .next_frame()
        .await
        .unwrap()
        .expect("session-detached");
    match codec::decode::<OpsToClientMsg>(&reply.body).unwrap() {
        OpsToClientMsg::Detached {
            resolved,
            was_attached,
        } => {
            assert_eq!(resolved, ResolvedId::Ok { id });
            assert!(!was_attached, "an idle session reports was_attached=false");
        }
        other => panic!("expected Ops::Detached, got {other:?}"),
    }
}

/// Queue admission is the whole reply: a switch with no window to
/// target answers a typed denial beside `queued: 0`, while a switch
/// whose post-condition already holds answers the same count with no
/// denial at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_with_no_targeted_window_separates_the_count_from_the_denial() {
    use felis_protocol::messages::SwitchDenied;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("read _x"));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Ops, false).await;

    // Spawned rather than created: a `Create` would attach this
    // connection, and the switch would then have a window to target.
    let mut ids = Vec::new();
    for request_id in [1, 2] {
        send_request(
            &mut writer,
            &OpsToDaemonMsg::Spawn {
                args: felis_protocol::messages::SpawnArgs::default(),
            },
            request_id,
        )
        .await;
        let spawned = reader.next_frame().await.unwrap().expect("spawned");
        match codec::decode::<OpsToClientMsg>(&spawned.body).unwrap() {
            OpsToClientMsg::Spawned {
                outcome: SpawnOutcome::Ok { info },
            } => ids.push(info.id),
            other => panic!("expected a successful Spawned, got {other:?}"),
        }
    }
    let (from, to) = (ids[0], ids[1]);

    send_request(
        &mut writer,
        &OpsToDaemonMsg::Switch {
            from_prefix: hex(from),
            target: SwitchTarget::Session(hex(to)),
            scope: SwitchScope::Default,
        },
        3,
    )
    .await;
    let reply = reader.next_frame().await.unwrap().expect("switched");
    match codec::decode::<OpsToClientMsg>(&reply.body).unwrap() {
        OpsToClientMsg::Switched { queued, denied, .. } => {
            assert_eq!(queued, 0, "nothing took the push");
            assert!(
                matches!(denied, Some(SwitchDenied::NoInputOwner)),
                "a scope that named no window is a denial, got {denied:?}",
            );
        }
        other => panic!("expected Ops::Switched, got {other:?}"),
    }

    send_request(
        &mut writer,
        &OpsToDaemonMsg::Switch {
            from_prefix: hex(from),
            target: SwitchTarget::Session(hex(from)),
            scope: SwitchScope::Default,
        },
        4,
    )
    .await;
    let reply = reader.next_frame().await.unwrap().expect("switched");
    match codec::decode::<OpsToClientMsg>(&reply.body).unwrap() {
        OpsToClientMsg::Switched { queued, denied, .. } => {
            assert_eq!(queued, 0, "a satisfied post-condition pushes nothing");
            assert!(
                denied.is_none(),
                "zero is a count, not a failure, got {denied:?}",
            );
        }
        other => panic!("expected Ops::Switched, got {other:?}"),
    }
}

use super::*;
use crate::test_support::{pid_alive, read_pid};

/// Out-of-bound geometry on a create is refused with the dedicated reason
/// before anything is execed (REQ-605a); the factory panics if it runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_create_past_the_geometry_bound_is_refused_before_anything_spawns() {
    for rows in [65_536_u32, u32::MAX] {
        let tmp = private_dir();
        let path = tmp.path().join("daemon.sock");
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let factory: SessionFactory =
            Arc::new(|_| panic!("an out-of-range create must not reach the factory"));
        spawn_daemon(&path, pool, factory);

        let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;

        send_kind(
            &mut writer,
            &SessionToDaemonMsg::Create {
                args: felis_protocol::messages::SpawnArgs {
                    dims: Some(felis_protocol::messages::RequestedDims {
                        rows,
                        cols: 80,
                        pixel_w: 0,
                        pixel_h: 0,
                    }),
                    ..Default::default()
                },
                image_regions: false,
            },
        )
        .await;

        let refusal = reader.next_frame().await.unwrap().expect("attach-failed");
        match codec::decode::<SessionToClientMsg>(&refusal.body).unwrap() {
            SessionToClientMsg::AttachFailed { reason, detail } => {
                assert_eq!(
                    reason,
                    AttachRefusal::Create(CreateFailure::GeometryOutOfRange)
                );
                assert!(
                    detail.contains("rows") && detail.contains(&rows.to_string()),
                    "the refusal must name the axis and the value it saw, got: {detail}"
                );
            }
            other => panic!("expected AttachFailed, got {other:?}"),
        }
    }
}

/// Absence is the only way a create asks for the daemon default: an
/// omitted geometry lands on 24x80, and a create that spells a zero
/// axis out is refused rather than defaulted (REQ-605a).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_create_geometry_defaults_and_a_zero_axis_is_refused() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool, shell_factory("read x"));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let info = create_and_attach_info(&mut reader, &mut writer).await;
    assert_eq!(
        (info.dims.rows, info.dims.cols),
        (crate::DEFAULT_ROWS, crate::DEFAULT_COLS),
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs {
                dims: Some(felis_protocol::messages::RequestedDims {
                    rows: 0,
                    cols: 80,
                    pixel_w: 0,
                    pixel_h: 0,
                }),
                ..Default::default()
            },
            image_regions: false,
        },
    )
    .await;
    let refusal = reader.next_frame().await.unwrap().expect("attach-failed");
    match codec::decode::<SessionToClientMsg>(&refusal.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, detail } => {
            assert_eq!(
                reason,
                AttachRefusal::Create(CreateFailure::GeometryOutOfRange)
            );
            assert!(
                detail.contains("rows 0"),
                "the refusal must name the zero axis, got: {detail}"
            );
        }
        other => panic!("expected AttachFailed, got {other:?}"),
    }
}

/// The same values on a resize are clamped instead: a resize the user did
/// not ask for must never cost them their shell (REQ-605a).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_past_the_geometry_bound_is_clamped_and_reported_clamped() {
    use felis_protocol::messages::{MAX_GRID_COLS, MAX_GRID_ROWS};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool, shell_factory("read x"));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    send_input(
        &mut writer,
        &InputMsg::Resize {
            dims: felis_protocol::messages::RequestedDims {
                rows: u32::MAX,
                cols: 65_536,
                pixel_w: u32::MAX,
                pixel_h: 0,
            },
        },
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut announced = None;
    while announced.is_none() && tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout_at(deadline, reader.next_frame()).await {
            Ok(Ok(Some(f))) => f,
            Ok(Err(e)) => panic!("the connection must survive an out-of-range resize: {e:?}"),
            Ok(Ok(None)) => panic!("the daemon closed instead of clamping"),
            Err(_) => break,
        };
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        if let GridMsg::Size { dims } = codec::decode::<GridMsg>(&frame.body).unwrap() {
            announced = Some(dims);
        }
    }
    let dims = announced.expect("the daemon must announce the clamped geometry");
    assert_eq!(dims.rows, MAX_GRID_ROWS);
    assert_eq!(dims.cols, MAX_GRID_COLS);
    assert_eq!(dims.pixel_w, felis_protocol::messages::MAX_GRID_PIXELS);
    // The unknown pixel sentinel is not a value to clamp.
    assert_eq!(dims.pixel_h, 0);
}

/// The cap refuses the create typed, with nothing executed, and the
/// connection survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_past_the_session_cap_is_refused_and_executes_nothing() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        max_sessions: 1,
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("sleep 30"), caps);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    // A second connection, because the first is attached now: a `Create`
    // is a pre-attach frame.
    let (mut reader_b, mut writer_b) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer_b,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
            image_regions: false,
        },
    )
    .await;
    let refused = reader_b.next_frame().await.unwrap().expect("a refusal");
    match codec::decode::<SessionToClientMsg>(&refused.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, detail } => {
            assert_eq!(
                reason,
                AttachRefusal::Create(CreateFailure::SessionLimitReached)
            );
            assert!(detail.contains("at 1 of 1 sessions"), "{detail}");
        }
        other => panic!("expected AttachFailed, got {other:?}"),
    }

    // The cap is charged the same way through the headless entry point.
    let (mut reader_c, mut writer_c) = dial(&path, ConnectionMode::Ops, false).await;
    send_request(
        &mut writer_c,
        &OpsToDaemonMsg::Spawn {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
        1,
    )
    .await;
    let refused = reader_c.next_frame().await.unwrap().expect("a refusal");
    match codec::decode::<OpsToClientMsg>(&refused.body).unwrap() {
        OpsToClientMsg::Spawned {
            outcome: SpawnOutcome::Refused { reason, detail },
        } => {
            assert_eq!(reason, CreateFailure::SessionLimitReached);
            assert!(detail.contains("at 1 of 1 sessions"), "{detail}");
        }
        other => panic!("expected a refused Spawned, got {other:?}"),
    }

    assert_eq!(
        pool.lock().await.len(),
        1,
        "the refused creates must leave the pool where it was"
    );
}

/// Two spawns in flight on one connection: each reply carries its own
/// `request_id` and its own session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_ops_spawns_are_attributed_by_request_id() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Ops, false).await;

    // Tagged so a reply traces back to its request without depending on
    // the session id the daemon minted.
    for (request, tag) in [(1_u64, "first"), (2, "second")] {
        send_request(
            &mut writer,
            &OpsToDaemonMsg::Spawn {
                args: felis_protocol::messages::SpawnArgs {
                    tags: vec![tag.to_owned()],
                    ..Default::default()
                },
            },
            request,
        )
        .await;
    }

    let mut seen = Vec::new();
    for _ in 0..2 {
        let frame = reader.next_frame().await.unwrap().expect("a spawn reply");
        let request = codec::peek_correlation(&frame.body)
            .unwrap()
            .and_then(Correlation::request_id)
            .expect("a spawn reply carries its request id");
        match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
            OpsToClientMsg::Spawned {
                outcome: SpawnOutcome::Ok { info },
            } => seen.push((request.get(), info.tags, info.id)),
            other => panic!("expected a successful Spawned, got {other:?}"),
        }
    }
    seen.sort_by_key(|(request, _, _)| *request);
    assert_eq!(seen[0].1, vec!["first".to_owned()]);
    assert_eq!(seen[1].1, vec!["second".to_owned()]);
    assert_ne!(seen[0].2, seen[1].2, "two spawns are two sessions");
    assert_eq!(pool.lock().await.len(), 2);
}

/// The create ack means "attached" and "nameable": the rehydrate burst
/// follows it with no `Attach` in between, and any connection can
/// already look the session up by id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_is_attached_when_its_ack_is_written() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(
        &path,
        pool.clone(),
        shell_factory("printf 'hello-felis\n'; sleep 30"),
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    let subscribers = pool
        .lock()
        .await
        .get(SessionId(id))
        .expect("an acked session must already be nameable")
        .meta_snapshot()
        .subscribers;
    assert_eq!(subscribers, 1, "the create ack must mean attached");
    expect_row_containing(&mut reader, "hello-felis").await;
}

/// Until a create publishes its row, no other connection can list,
/// resolve, attach to or destroy the session it may still roll back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_in_flight_is_unnameable_until_it_publishes() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory: SessionFactory = Arc::new(|_| sh("exec sleep 30", fixture_path()));
    let caps = DaemonCaps::default();
    let (registered, info) = create_session(
        CreateCtx {
            pool: &pool,
            caps: &caps,
            factory: &factory,
            relay_env: None,
        },
        felis_protocol::messages::SpawnArgs::default(),
    )
    .await
    .expect("the create must be admitted");
    let id = SessionId(info.id);

    {
        let guard = pool.lock().await;
        assert!(guard.handle_cloned(id).is_none(), "attachable too early");
        assert!(guard.roster_by_recency().is_empty(), "listable too early");
        assert_eq!(guard.ids().count(), 0, "resolvable too early");
        assert_eq!(guard.len(), 1, "but it holds a session slot");
    }

    drop(registered.publish().await);
    let guard = pool.lock().await;
    assert!(guard.handle_cloned(id).is_some());
    assert_eq!(guard.roster_by_recency().len(), 1);
}

/// A create whose subscribe cannot land leaves nothing in the pool, and
/// its rollback does not return until the child is reaped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rolled_back_create_leaves_no_pool_entry_and_no_child() {
    let tmp = private_dir();
    let pid_file = tmp.path().join("child.pid");
    let body = format!("printf '%s' $$ > {}; exec sleep 30", pid_file.display());
    rolls_back_a_create_and_reaps(body, &pid_file).await;
}

/// Rollback reaps a child that ignores hangup via `SIGKILL` within timeout.
///
/// `trap '' HUP` survives `exec` so `SIGKILL` cleanup is exercised directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rolled_back_create_reaps_a_child_that_ignores_the_hangup() {
    let tmp = private_dir();
    let pid_file = tmp.path().join("child.pid");
    let body = format!(
        "trap '' HUP; printf '%s' $$ > {}; exec sleep 30",
        pid_file.display()
    );
    rolls_back_a_create_and_reaps(body, &pid_file).await;
}

async fn rolls_back_a_create_and_reaps(body: String, pid_file: &Path) {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory: SessionFactory = Arc::new(move |_| sh(&body, fixture_path()));
    let caps = DaemonCaps::default();
    let (mut registered, _info) = create_session(
        CreateCtx {
            pool: &pool,
            caps: &caps,
            factory: &factory,
            relay_env: None,
        },
        felis_protocol::messages::SpawnArgs::default(),
    )
    .await
    .expect("the create must be admitted");
    assert_eq!(pool.lock().await.len(), 1);

    let pid = read_pid(pid_file).await;
    let mut done = registered.done.clone();
    registered.roll_back().await;

    assert_eq!(
        pool.lock().await.len(),
        0,
        "the rolled-back session must leave the pool"
    );
    assert!(
        *done.borrow_and_update(),
        "roll_back must not return before the owner task finished reaping"
    );
    assert!(!pid_alive(pid), "the child must be gone, not orphaned");
}

/// A peer holding the ack can hand its id to another connection at once,
/// so the row must answer lookups before the ack can be read. The
/// carrier here is a byte or two wide, so the ack cannot complete until
/// this test reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_is_nameable_before_its_ack_is_read() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
    // Narrow on purpose: every daemon write blocks until this test
    // reads it, which is what makes the ordering observable.
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(8);
    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        handle_stream(
            daemon_read,
            daemon_write,
            DaemonCaps::default(),
            server_pool,
            shell_factory("read _x"),
        )
        .await
    });

    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
            image_regions: false,
        },
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let id = loop {
        let listed = pool.lock().await.ids().next();
        if let Some(id) = listed {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the create never became nameable while its ack was unread"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let ack = reader.next_frame().await.unwrap().expect("created");
    match codec::decode::<SessionToClientMsg>(&ack.body).unwrap() {
        SessionToClientMsg::Created { info } => assert_eq!(SessionId(info.id), id),
        other => panic!("expected SessionToClientMsg::Created, got {other:?}"),
    }

    drop(writer);
    drop(reader);
    // A hangup that lands mid-rehydrate may end the connection with a
    // broken pipe, so only its ending is asserted.
    let served = tokio::time::timeout(Duration::from_secs(10), server).await;
    shut_down_and_reap(&pool, id).await;
    let _hangup = served
        .expect("the connection must end once its peer hangs up")
        .expect("handle_stream must not panic");
}

/// A creation still in flight appears in the status row: reading `0`
/// while a concurrent create is refused at the cap would name a limit
/// the refusal disagrees with (REQ-915).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_status_counts_a_creation_still_in_flight() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read _x");
    let caps = DaemonCaps::default();
    let (registered, _info) = create_session(
        CreateCtx {
            pool: &pool,
            caps: &caps,
            factory: &factory,
            relay_env: None,
        },
        felis_protocol::messages::SpawnArgs::default(),
    )
    .await
    .expect("the create must be admitted");
    assert_eq!(
        pool.lock().await.ids().count(),
        0,
        "held, so not yet listed"
    );

    let reply = daemon_status(&pool, &caps).await;
    let OpsToClientMsg::StatusReply { resources, .. } = reply else {
        panic!("expected an OpsToClientMsg::StatusReply, got {reply:?}")
    };
    let sessions = resources
        .iter()
        .find(|row| row.resource == felis_protocol::messages::ResourceKind::Sessions)
        .expect("the reply must carry the sessions row");
    assert_eq!(
        sessions.total_used, 1,
        "a held creation is admitted against the cap and must be counted"
    );

    drop(registered.keep());
}

/// A create is published before its ack, so an ack that never lands
/// leaves the session detached and listed, as a lost `Spawned` does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_whose_ack_cannot_be_written_leaves_a_detached_session() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));

    let (client_to_daemon_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_from_daemon_r) = tokio::io::duplex(64 * 1024);
    let server_pool = pool.clone();
    let server = tokio::spawn(async move {
        handle_stream(
            daemon_read,
            daemon_write,
            DaemonCaps::default(),
            server_pool,
            shell_factory("read _x"),
        )
        .await
    });

    let (mut reader, mut writer) = framed(client_from_daemon_r, client_to_daemon_w).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    // The peer goes away between its `Create` and the ack, so every
    // later daemon write is a broken pipe.
    drop(reader);
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
            image_regions: false,
        },
    )
    .await;

    let served = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the connection must end once its ack cannot be written")
        .expect("handle_stream must not panic");
    assert!(served.is_err(), "the failed ack write ends the connection");
    let listed = pool.lock().await.ids().next();
    let id = listed.expect("an undeliverable ack must leave the session listed");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let subscribers = pool
            .lock()
            .await
            .get(id)
            .expect("still listed")
            .meta_snapshot()
            .subscribers;
        if subscribers == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the connection whose ack failed must not stay subscribed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shut_down_and_reap(&pool, id).await;
}

/// Returns once the session task has left the pool and its child is
/// reaped: `quiescent` stays false until the teardown guard drops.
async fn shut_down_and_reap(pool: &Arc<Mutex<SessionPool>>, id: SessionId) {
    let handle = pool.lock().await.handle_cloned(id).expect("listed");
    let delivered = handle.shutdown().await;
    assert!(delivered, "the session task takes the shutdown");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !pool.lock().await.quiescent() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session was never reaped"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Registration happens after the PTY fork/exec, so a cap that merely reads
/// the pool count admits every create that reached the check before the
/// first one registered; the slot must not overshoot either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creates_never_admit_past_the_session_cap() {
    const CAP: usize = 3;
    const CREATES: usize = 12;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        max_sessions: CAP,
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("sleep 30"), caps);

    // All dialed before any sends, so the creates reach the admission check
    // together.
    let mut clients = Vec::new();
    for _ in 0..CREATES {
        let (read_half, write_half) = connect(&path).await.unwrap();
        let (reader, writer) = framed(read_half, write_half).await;
        clients.push((reader, writer));
    }
    let mut running = Vec::new();
    for (mut reader, mut writer) in clients {
        running.push(tokio::spawn(async move {
            hello_welcome(&mut reader, &mut writer, false).await;
            send_kind(
                &mut writer,
                &SessionToDaemonMsg::Create {
                    args: felis_protocol::messages::SpawnArgs::default(),
                    image_regions: false,
                },
            )
            .await;
            let frame = reader.next_frame().await.unwrap().expect("an answer");
            codec::decode::<SessionToClientMsg>(&frame.body).unwrap()
        }));
    }

    let mut created = 0;
    let mut refused = 0;
    for task in running {
        match task.await.unwrap() {
            SessionToClientMsg::Created { .. } => created += 1,
            SessionToClientMsg::AttachFailed { reason, detail } => {
                assert_eq!(
                    reason,
                    AttachRefusal::Create(CreateFailure::SessionLimitReached)
                );
                assert!(detail.contains(&format!("of {CAP} sessions")), "{detail}");
                refused += 1;
            }
            other => panic!("expected Ready or AttachFailed, got {other:?}"),
        }
    }

    assert_eq!(created, CAP, "the burst must fill the cap exactly");
    assert_eq!(refused, CREATES - CAP);
    assert_eq!(
        pool.lock().await.len(),
        CAP,
        "the pool must never hold more sessions than the cap admits"
    );
}

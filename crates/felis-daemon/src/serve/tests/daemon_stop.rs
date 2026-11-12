use super::*;

#[cfg(unix)]
async fn stop_reply<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    mode: StopMode,
    request: u64,
) -> StopOutcome
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    send_request(writer, &OpsToDaemonMsg::Stop { mode }, request).await;
    loop {
        let frame = reader.next_frame().await.unwrap().expect("a Stop reply");
        if frame.kind != MessageKind::Ops.as_u16() {
            continue;
        }
        match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
            OpsToClientMsg::StopReply { outcome } => return outcome,
            other => panic!("expected OpsToClientMsg::StopReply, got {other:?}"),
        }
    }
}

/// A bare `stop` is not allowed to destroy anything: it reports the
/// count and leaves the daemon serving.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_default_stop_refuses_while_a_session_remains_and_preserves_it() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool.clone(),
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    let (mut ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;
    assert_eq!(
        stop_reply(&mut ops_reader, &mut ops_writer, StopMode::IfEmpty, 1).await,
        StopOutcome::Refused { sessions: 1 }
    );
    {
        let guard = pool.lock().await;
        assert_eq!(guard.len(), 1, "the refused stop touched no session");
        assert!(!guard.draining(), "a refused stop starts no drain");
    }
    // Still serving: a connection opened after the refusal is admitted.
    let (mut after_reader, mut after_writer) = dial(&path, ConnectionMode::Ops, false).await;
    send_request(&mut after_writer, &OpsToDaemonMsg::List, 1).await;
    after_reader.next_frame().await.unwrap().expect("a roster");
    assert!(!serving.is_finished());
    serving.abort();
}

/// A refused upgrade is answered on the connection that asked, which
/// stays open, and the daemon keeps serving every session it held.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_upgrade_is_answered_and_leaves_the_daemon_serving() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool.clone(),
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    let (mut ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;
    send_request(
        &mut ops_writer,
        &OpsToDaemonMsg::Upgrade {
            successor: tmp
                .path()
                .join("no-such-felis-daemon")
                .display()
                .to_string(),
        },
        1,
    )
    .await;
    let frame = ops_reader
        .next_frame()
        .await
        .unwrap()
        .expect("an Upgrade reply");
    match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
        OpsToClientMsg::UpgradeReply {
            outcome: felis_protocol::messages::UpgradeOutcome::Refused { .. },
        } => {}
        other => panic!("expected a refused UpgradeReply, got {other:?}"),
    }
    assert_eq!(pool.lock().await.len(), 1, "the refusal touched no session");

    send_request(&mut ops_writer, &OpsToDaemonMsg::List, 2).await;
    ops_reader
        .next_frame()
        .await
        .unwrap()
        .expect("the asking connection is still answered");
    assert!(!serving.is_finished());
    serving.abort();
}

/// `--force` destroys every session, and the process it was asked to
/// stop actually goes: the accept loop returns and the socket path is
/// cleared.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_stop_destroys_every_session_and_ends_the_accept_loop() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool.clone(),
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    let (mut ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;
    assert_eq!(
        stop_reply(&mut ops_reader, &mut ops_writer, StopMode::Force, 1).await,
        StopOutcome::Stopping,
        "the reply reaches the caller before the daemon goes"
    );
    assert_eq!(pool.lock().await.len(), 0, "force destroys every session");
    tokio::time::timeout(Duration::from_secs(20), serving)
        .await
        .expect("the daemon exits on a forced stop")
        .expect("the serve task did not panic")
        .expect("the accept loop ends cleanly");
    // REQ-009d: exit unlinks nothing, so an old daemon dying can never
    // remove a newer one's socket; the next starter's probe replaces it.
    assert!(
        std::fs::symlink_metadata(&path).is_ok(),
        "the stopped daemon leaves its socket path in place"
    );
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&path)
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionRefused,
        "and nothing answers there any more"
    );
}

/// `--when-empty` is the drain: typed and observable while it lasts,
/// refusing creates, and ending the daemon after the last session.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_when_empty_stop_refuses_creates_and_exits_after_the_last_session() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool.clone(),
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    let (mut ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;
    assert_eq!(
        stop_reply(&mut ops_reader, &mut ops_writer, StopMode::WhenEmpty, 1).await,
        StopOutcome::Draining { sessions: 1 }
    );
    // Idempotent: a second stop while draining answers the same state.
    assert_eq!(
        stop_reply(&mut ops_reader, &mut ops_writer, StopMode::IfEmpty, 2).await,
        StopOutcome::Draining { sessions: 1 }
    );

    // The state is observable in `Ops::Status`.
    send_request(&mut ops_writer, &OpsToDaemonMsg::Status, 3).await;
    let status = loop {
        let frame = ops_reader.next_frame().await.unwrap().expect("a status");
        if frame.kind != MessageKind::Ops.as_u16() {
            continue;
        }
        break codec::decode::<OpsToClientMsg>(&frame.body).unwrap();
    };
    match status {
        OpsToClientMsg::StatusReply { draining, .. } => assert!(draining),
        other => panic!("expected a StatusReply, got {other:?}"),
    }

    // And a create arriving during the drain is refused with a reason
    // that names it.
    let (mut new_reader, mut new_writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut new_writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs::default(),
            image_regions: false,
        },
    )
    .await;
    let refused = new_reader.next_frame().await.unwrap().expect("a refusal");
    match codec::decode::<SessionToClientMsg>(&refused.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, detail } => {
            assert_eq!(reason, AttachRefusal::Create(CreateFailure::DaemonDraining));
            assert!(detail.contains("draining"), "{detail}");
        }
        other => panic!("expected AttachFailed, got {other:?}"),
    }
    assert_eq!(pool.lock().await.len(), 1, "the drain destroyed nothing");

    send_request(
        &mut ops_writer,
        &OpsToDaemonMsg::Destroy { id_prefix: hex(id) },
        4,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(20), serving)
        .await
        .expect("the daemon exits once its last session ends")
        .expect("the serve task did not panic")
        .expect("the accept loop ends cleanly");
    // REQ-009d: exit unlinks nothing, so an old daemon dying can never
    // remove a newer one's socket; the next starter's probe replaces it.
    assert!(
        std::fs::symlink_metadata(&path).is_ok(),
        "the stopped daemon leaves its socket path in place"
    );
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&path)
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionRefused,
        "and nothing answers there any more"
    );
}

/// The emptiness decision cannot race a create: a reservation taken and
/// not yet registered is what a create in flight *is*, so it counts as
/// non-empty and holds the drain open, and once a stop has been
/// answered no further reservation is admitted at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_create_in_flight_counts_as_non_empty_and_holds_the_drain_open() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps::default();
    let slot = pool
        .lock()
        .await
        .try_reserve(8)
        .expect("an empty pool admits a create");

    assert_eq!(
        stop_daemon(&pool, &caps, StopMode::IfEmpty).await,
        StopOutcome::Refused { sessions: 1 },
        "a create between its reservation and its registration is a session"
    );
    assert_eq!(
        stop_daemon(&pool, &caps, StopMode::WhenEmpty).await,
        StopOutcome::Draining { sessions: 1 }
    );
    assert!(!caps.shutdown.fired(), "the drain is not settled yet");
    assert_eq!(
        pool.lock().await.try_reserve(8).unwrap_err(),
        ReserveRefusal::Draining,
        "no create is admitted after a stop was answered"
    );

    // The create it belonged to failed, which resolves the reservation.
    drop(slot);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !caps.shutdown.fired() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the drain settles when the last reservation resolves");
}

/// A session leaves the pool before its child is hung up and reaped,
/// on the panic path from a task that outlives the session's own, so
/// an empty pool is not yet a daemon that may exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_waits_for_a_child_still_being_reaped_after_its_session_left() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps::default();
    let teardown = pool.lock().await.begin_teardown();

    let stopping = tokio::spawn({
        let pool = Arc::clone(&pool);
        let caps = caps.clone();
        async move { stop_daemon(&pool, &caps, StopMode::WhenEmpty).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !stopping.is_finished(),
        "the stop is not answered while a child is still being reaped"
    );

    drop(teardown);
    let outcome = tokio::time::timeout(Duration::from_secs(5), stopping)
        .await
        .expect("the stop settles once the reap is done")
        .expect("the stop task did not panic");
    assert_eq!(outcome, StopOutcome::Stopping);
}

/// The shutdown does not ride on the reply reaching anyone: a requester
/// that vanishes right after sending `Stop` still leaves a daemon whose
/// pool it already emptied, so that daemon must exit anyway.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_whose_reply_cannot_be_written_still_stops_the_daemon() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool.clone(),
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    let (ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;
    send_request(
        &mut ops_writer,
        &OpsToDaemonMsg::Stop {
            mode: StopMode::Force,
        },
        1,
    )
    .await;
    drop(ops_reader);
    drop(ops_writer);

    tokio::time::timeout(Duration::from_secs(20), serving)
        .await
        .expect("the daemon exits even when the stop reply cannot be delivered")
        .expect("the serve task did not panic")
        .expect("the accept loop ends cleanly");
}

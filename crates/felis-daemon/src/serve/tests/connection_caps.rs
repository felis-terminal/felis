use super::*;

// ── Connection admission and handshake deadlines (REQ-916) ──────────

#[cfg(unix)]
fn admission_caps(max_connections: usize, refusal_slots: usize) -> DaemonCaps {
    DaemonCaps {
        admission: ConnectionAdmission::with_refusal_slots(max_connections, refusal_slots),
        ..DaemonCaps::default()
    }
}

/// A permit is released by the connection task's drop, which the peer's
/// close only schedules, so a freed slot has to be polled for.
#[cfg(unix)]
async fn dial_until_admitted(path: &Path) -> bool {
    for _ in 0..200 {
        let (read_half, write_half) = connect(path).await.unwrap();
        let (mut reader, mut writer) = framed(read_half, write_half).await;
        send_kind(
            &mut writer,
            &ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Ops,
                pull_paced: false,
            },
        )
        .await;
        let frame = reader.next_frame().await.unwrap().expect("a Conn reply");
        if matches!(
            codec::decode::<ConnToClientMsg>(&frame.body).unwrap(),
            ConnToClientMsg::Welcome { .. }
        ) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[cfg(unix)]
async fn connections_row<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    request: u64,
) -> felis_protocol::messages::ResourceReport
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    resource_row(
        reader,
        writer,
        request,
        felis_protocol::messages::ResourceKind::Connections,
    )
    .await
}

/// Over-cap peers get a frame, not a bare close: to a client a closed
/// socket is indistinguishable from a crashed daemon.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dial_past_the_connection_cap_is_refused_at_capacity() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon_with_caps(&path, pool, shell_factory("sleep 30"), admission_caps(1, 4));

    let (reader, writer) = dial(&path, ConnectionMode::Ops, false).await;

    let (over_r, over_w) = connect(&path).await.unwrap();
    let (mut over_reader, mut over_writer) = framed(over_r, over_w).await;
    send_kind(
        &mut over_writer,
        &ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Ops,
            pull_paced: false,
        },
    )
    .await;
    let refused = over_reader
        .next_frame()
        .await
        .unwrap()
        .expect("the daemon answers an over-cap dial");
    match codec::decode::<ConnToClientMsg>(&refused.body).unwrap() {
        ConnToClientMsg::Refused {
            reason: RefusalReason::AtCapacity,
            detail,
        } => assert!(
            detail.contains("1 of 1"),
            "the refusal names the count and the ceiling: {detail}"
        ),
        other => panic!("expected Refused(AtCapacity), got {other:?}"),
    }
    drop((over_reader, over_writer));

    drop((reader, writer));
    assert!(
        dial_until_admitted(&path).await,
        "the closed connection must return its permit"
    );
}

/// The refusal path is itself admitted: a flood of silent peers can
/// hold at most `max_connections + refusal slots` tasks and fds.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silent_over_cap_peers_are_bounded_by_the_refusal_slots() {
    use felis_protocol::preface::ClientPreface;
    use felis_transport::preface::{read_daemon_preface, write_client_preface};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon_with_caps(&path, pool, shell_factory("sleep 30"), admission_caps(1, 1));

    let (reader, writer) = dial(&path, ConnectionMode::Ops, false).await;

    // Its preface reply proves the refusal task is running, so the one
    // refusal slot is taken for as long as this peer stays silent.
    let (silent_r, silent_w) = connect(&path).await.unwrap();
    let (silent_reader, silent_writer) = framed(silent_r, silent_w).await;

    // The close can land before or after the third peer's own write, so
    // both halves count as "unanswered".
    let (mut third_r, mut third_w) = connect(&path).await.unwrap();
    let answered = write_client_preface(&mut third_w, ClientPreface::CURRENT)
        .await
        .is_ok()
        && read_daemon_preface(&mut third_r).await.is_ok();
    assert!(
        !answered,
        "with no refusal slot left the daemon drops the socket unanswered"
    );

    drop((silent_reader, silent_writer));
    drop((reader, writer));
}

/// Every accounted connection is one the daemon is serving, and every
/// permit comes back: the aggregate bound the per-resource ceilings
/// multiply against.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_connections_row_counts_admitted_peers_and_every_permit_returns() {
    const CAP: usize = 4;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon_with_caps(
        &path,
        pool,
        shell_factory("sleep 30"),
        admission_caps(CAP, 4),
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Ops, false).await;

    let mut held = Vec::new();
    for _ in 1..CAP {
        let (r, w) = dial(&path, ConnectionMode::Ops, false).await;
        held.push((r, w));
    }

    assert_eq!(held.len(), CAP - 1);
    let row = connections_row(&mut reader, &mut writer, 1).await;
    assert_eq!(
        row.total_used, CAP as u64,
        "every dial is one admitted peer"
    );
    assert_eq!(global_limit(&row), Some(CAP as u64));

    let (over_r, over_w) = connect(&path).await.unwrap();
    let (mut over_reader, mut over_writer) = framed(over_r, over_w).await;
    send_kind(
        &mut over_writer,
        &ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Ops,
            pull_paced: false,
        },
    )
    .await;
    let refused = over_reader.next_frame().await.unwrap().expect("a refusal");
    assert!(
        matches!(
            codec::decode::<ConnToClientMsg>(&refused.body).unwrap(),
            ConnToClientMsg::Refused {
                reason: RefusalReason::AtCapacity,
                ..
            }
        ),
        "a dial past the cap is refused, never queued"
    );
    drop((over_reader, over_writer));

    drop(held);
    let mut request = 2;
    let settled = loop {
        let row = connections_row(&mut reader, &mut writer, request).await;
        request += 1;
        if row.total_used == 1 || request > 200 {
            break row.total_used;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        settled, 1,
        "only the connection asking the question is left holding a permit"
    );
}

/// A peer that opens a socket and writes nothing costs a task and an fd
/// for the preface deadline, not forever.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_peer_that_writes_no_preface_is_cut_at_the_preface_deadline() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        DaemonCaps::default(),
        pool,
        default_session_factory(),
    ));

    let err = server
        .await
        .unwrap()
        .expect_err("a silent peer must be cut, not awaited");
    assert!(
        matches!(
            err,
            ConnError::HandshakeTimeout {
                phase: HandshakePhase::Preface
            }
        ),
        "{err:?}"
    );
    drop((client_r, client_w));
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_peer_that_stops_after_the_preface_is_cut_at_the_hello_deadline() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        DaemonCaps::default(),
        pool,
        default_session_factory(),
    ));

    let (reader, writer) = framed(client_r, client_w).await;
    let err = server.await.unwrap().expect_err("no Hello, no connection");
    assert!(
        matches!(
            err,
            ConnError::HandshakeTimeout {
                phase: HandshakePhase::Hello
            }
        ),
        "{err:?}"
    );
    drop((reader, writer));
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_peer_that_never_attaches_is_cut_at_the_first_operation_deadline() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        DaemonCaps::default(),
        pool,
        default_session_factory(),
    ));

    let (mut reader, mut writer) = framed(client_r, client_w).await;
    // A `Window` names its session in the frame after `Welcome`, so
    // silence there is a stall; an `Ops` peer is the exempt one.
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Window, false).await;
    let err = server
        .await
        .unwrap()
        .expect_err("a handshaked peer that asks nothing is cut too");
    assert!(
        matches!(
            err,
            ConnError::HandshakeTimeout {
                phase: HandshakePhase::FirstOperation
            }
        ),
        "{err:?}"
    );
    drop((reader, writer));
}

/// An editor client dials its ops connection at startup to prove the
/// daemon is reachable and writes nothing until its user asks for
/// something, so the deadline that cut it would kill an idle client
/// for working as documented.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn an_ops_connection_may_idle_before_its_first_verb() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let caps = DaemonCaps::default();
    let idle = caps.handshake.first_op * 3;
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        caps,
        pool,
        default_session_factory(),
    ));

    let (mut reader, mut writer) = framed(client_r, client_w).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;
    tokio::time::sleep(idle).await;

    send_request(&mut writer, &OpsToDaemonMsg::List, 1).await;
    let frame = reader
        .next_frame()
        .await
        .unwrap()
        .expect("the anchor is still open");
    assert!(matches!(
        codec::decode::<OpsToClientMsg>(&frame.body).unwrap(),
        OpsToClientMsg::Listed { .. }
    ));
    drop((reader, writer));
    drop(server.await);
}

/// Only the *first* operation is timed: an ops client idles between verbs
/// and an observer idles for hours, and cutting either would break the
/// connection modes the daemon exists to serve.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn an_ops_connection_that_answered_one_verb_may_idle() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let caps = DaemonCaps::default();
    let idle = caps.handshake.first_op * 3;
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        caps,
        pool,
        default_session_factory(),
    ));

    let (mut reader, mut writer) = framed(client_r, client_w).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;
    for request in [1, 2] {
        if request == 2 {
            tokio::time::sleep(idle).await;
        }
        send_request(&mut writer, &OpsToDaemonMsg::List, request).await;
        let frame = reader.next_frame().await.unwrap().expect("a Listed reply");
        assert!(matches!(
            codec::decode::<OpsToClientMsg>(&frame.body).unwrap(),
            OpsToClientMsg::Listed { .. }
        ));
    }
    drop((reader, writer));
    drop(server.await);
}

/// One dial through the handshake, `None` when the daemon is full. Any
/// other answer is a bug in the admission path, so it panics rather
/// than counting as a refusal.
#[cfg(unix)]
async fn try_admit(path: &Path, mode: ConnectionMode) -> Option<Halves> {
    let (read_half, write_half) = connect(path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    send_kind(
        &mut writer,
        &ConnToDaemonMsg::Hello {
            mode,
            pull_paced: false,
        },
    )
    .await;
    let frame = reader.next_frame().await.unwrap().expect("a Conn reply");
    match codec::decode::<ConnToClientMsg>(&frame.body).unwrap() {
        ConnToClientMsg::Welcome { .. } => Some((reader, writer)),
        ConnToClientMsg::Refused {
            reason: RefusalReason::AtCapacity,
            ..
        } => None,
        other => panic!("expected Welcome or Refused(AtCapacity), got {other:?}"),
    }
}

/// The permit rides on the connection's task, and the deadline paths end
/// that task with `Err`. Were the drop not the release, every silent
/// peer would ratchet the daemon's ceiling down by one.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_cut_at_a_handshake_deadline_returns_its_permit() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        // Long enough to observe the permit being held, short enough
        // that the cut does not pace the test.
        handshake: HandshakeDeadlines {
            hello: Duration::from_millis(500),
            ..HandshakeDeadlines::default()
        },
        ..admission_caps(1, 4)
    };
    spawn_daemon_with_caps(&path, pool, shell_factory("sleep 30"), caps);

    // Takes the daemon's only permit and then says nothing, so its
    // handler ends in `Err(HandshakeTimeout { Hello })`.
    let (silent_reader, silent_writer) = {
        let (read_half, write_half) = connect(&path).await.unwrap();
        framed(read_half, write_half).await
    };
    assert!(
        try_admit(&path, ConnectionMode::Ops).await.is_none(),
        "the silent peer holds the only permit while it stalls"
    );

    assert!(
        dial_until_admitted(&path).await,
        "the failing handler must return its permit"
    );
    drop((silent_reader, silent_writer));
}

/// The aggregate bound under the load it exists for: many sessions,
/// several mirrors each, and more dials than the daemon admits. Nothing
/// the load does pushes the admitted count past the ceiling, over-cap
/// dials are typed rather than dropped, and once the peers are gone
/// every permit is back.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_sessions_with_mirrors_never_pass_the_connection_ceiling() {
    const SESSIONS: usize = 16;
    const MIRRORS: usize = 3;
    // Below `SESSIONS * (1 + MIRRORS)`, so the mirrors meet the ceiling
    // instead of fitting under it.
    const CAP: usize = 24;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon_with_caps(
        &path,
        pool,
        shell_factory("sleep 30"),
        admission_caps(CAP, 8),
    );

    // Admitted first and held throughout: every reading of the row is
    // taken by a connection the row itself counts.
    let (mut probe_reader, mut probe_writer) = try_admit(&path, ConnectionMode::Ops)
        .await
        .expect("the first dial of an empty daemon");
    let mut request = 1;

    let mut owners = Vec::new();
    let mut ids = Vec::new();
    for _ in 0..SESSIONS {
        let (mut reader, mut writer) = try_admit(&path, ConnectionMode::Window)
            .await
            .expect("the daemon admits every session's own connection");
        ids.push(create_and_attach(&mut reader, &mut writer).await);
        owners.push((reader, writer));

        let row = connections_row(&mut probe_reader, &mut probe_writer, request).await;
        request += 1;
        assert!(
            row.total_used <= CAP as u64,
            "creating sessions passed the ceiling: {row:?}"
        );
    }

    let mut mirrors = Vec::new();
    let mut refused = 0_usize;
    for id in ids.iter().copied() {
        for _ in 0..MIRRORS {
            match try_admit(&path, ConnectionMode::Window).await {
                Some((reader, mut writer)) => {
                    send_kind(
                        &mut writer,
                        &SessionToDaemonMsg::Attach {
                            target: AttachTarget::Id(id),
                            live_only: false,
                            image_regions: false,
                        },
                    )
                    .await;
                    mirrors.push((reader, writer));
                }
                None => refused += 1,
            }
        }
        let row = connections_row(&mut probe_reader, &mut probe_writer, request).await;
        request += 1;
        assert!(
            row.total_used <= CAP as u64,
            "mirroring passed the ceiling: {row:?}"
        );
        assert_eq!(global_limit(&row), Some(CAP as u64));
    }
    assert!(
        refused > 0,
        "the load must actually meet the ceiling, or it proves nothing"
    );

    drop(mirrors);
    drop(owners);
    drop((probe_reader, probe_writer));

    let (mut after_reader, mut after_writer) = loop {
        if let Some(peer) = try_admit(&path, ConnectionMode::Ops).await {
            break peer;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let mut request = 1;
    let settled = loop {
        let row = connections_row(&mut after_reader, &mut after_writer, request).await;
        request += 1;
        if row.total_used == 1 || request > 400 {
            break row.total_used;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        settled, 1,
        "after teardown only the asking connection holds a permit"
    );
}

/// The deadline covers the attach, not merely the first frame: the
/// shipped GUI asks `Ops::List` for its roster before it names a
/// session, so a peer that stops after that verb is still a stalled
/// window holding a permit.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_window_that_only_lists_is_still_cut_at_the_first_operation_deadline() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let (client_w, daemon_read) = tokio::io::duplex(64 * 1024);
    let (daemon_write, client_r) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(handle_stream(
        daemon_read,
        daemon_write,
        DaemonCaps::default(),
        pool,
        default_session_factory(),
    ));

    let (mut reader, mut writer) = framed(client_r, client_w).await;
    hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Window, false).await;
    send_request(&mut writer, &OpsToDaemonMsg::List, 1).await;
    let frame = reader.next_frame().await.unwrap().expect("a Listed reply");
    assert!(matches!(
        codec::decode::<OpsToClientMsg>(&frame.body).unwrap(),
        OpsToClientMsg::Listed { .. }
    ));

    let err = server
        .await
        .unwrap()
        .expect_err("a window that lists but never attaches is cut");
    assert!(
        matches!(
            err,
            ConnError::HandshakeTimeout {
                phase: HandshakePhase::FirstOperation
            }
        ),
        "{err:?}"
    );
    drop((reader, writer));
}

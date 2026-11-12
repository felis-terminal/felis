use super::*;

/// Pins the documented defaults: 5 s post-exit grace (docs/reference/ipc.md `exited`), 100 ms
/// parked drain.
#[test]
fn idle_policy_default_carries_the_documented_grace_and_drain_interval() {
    let policy = IdlePolicy::default();
    assert_eq!(policy.post_exit_grace, Duration::from_secs(5));
    assert_eq!(policy.drain_interval, Duration::from_millis(100));
    assert_eq!(
        DaemonCaps::default().idle.post_exit_grace,
        policy.post_exit_grace
    );
}

/// Pins the owner task's reap contract (session-lifecycle.md "Same-user
/// mirroring").
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_task_reaps_dead_session_after_grace() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("exit 0");
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_millis(50),
            drain_interval: Duration::from_millis(20),
        },
        ..DaemonCaps::default()
    };

    spawn_daemon_with_caps(&path, pool.clone(), factory, caps);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;

    create_and_attach(&mut reader, &mut writer).await;

    drop(reader);
    drop(writer);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if pool.lock().await.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let n = pool.lock().await.len();
            panic!("task did not reap dead session within deadline (still {n} live)");
        }
    }
}

/// The session produces no PTY output, so only a `tcgetpgrp` taken while
/// the roster is built can follow the `exec` handover.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_roster_names_the_program_that_owns_the_terminal_now() {
    let tmp = private_dir();
    let gate = gate(tmp.path());
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // `exec` keeps the pid (and so the foreground pgid) while replacing the
    // program.
    session_task::spawn_owned(
        &pool,
        owned_session(&format!("read _ < {}; exec sleep 20", gate.display())),
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    wait_for_listed_foreground(&pool, "sh").await;
    open_gate(&gate).await;
    wait_for_listed_foreground(&pool, "sleep").await;
}

#[cfg(unix)]
async fn wait_for_listed_foreground(pool: &Arc<Mutex<SessionPool>>, expect: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let seen = build_session_list(pool)
            .await
            .first()
            .and_then(|row| row.foreground.clone());
        if seen.as_deref() == Some(expect) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the roster never reported {expect:?} as the foreground program (last: {seen:?})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A lingering corpse must list as `exited = true` so a client's automatic
/// pick skips it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exited_shell_is_flagged_in_meta_during_the_grace() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("exit 0");
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_secs(60),
            drain_interval: Duration::from_millis(10),
        },
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), factory, caps);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        // An absent handle is retried rather than failed: publication
        // follows the ack this caller has already read, so a by-name
        // lookup can still miss the row for an instant.
        let handle = pool.lock().await.handle_cloned(SessionId(id));
        if handle.is_some_and(|handle| handle.meta_snapshot().exited) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "meta.exited never flipped after the shell exited"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The selection race end to end: the picked session exits before the
/// attach reaches the daemon, the live-only attach is refused, and the
/// re-pick lands on the survivor.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pick_that_exits_before_the_attach_is_refused_and_the_re_pick_lands() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_secs(60),
            drain_interval: Duration::from_millis(10),
        },
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("sleep 30"), caps);

    let session_task::SessionLifecycle { id: survivor, .. } = session_task::spawn_owned(
        &pool,
        owned_session("sleep 30"),
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;
    let session_task::SessionLifecycle { id: doomed, .. } = session_task::spawn_owned(
        &pool,
        owned_session("sleep 30"),
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
            target: AttachTarget::Id(survivor.0),
            live_only: true,
            image_regions: false,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, survivor.0);
    drain_rehydrate(&mut reader).await;

    send_request(&mut writer, &OpsToDaemonMsg::List, 1).await;
    let roster = listed_roster(&mut reader).await;
    assert!(
        roster.iter().any(|row| row.id == doomed.0 && !row.exited),
        "the pick's roster must still show the doomed session as live"
    );

    let delivered = pool
        .lock()
        .await
        .handle_cloned(doomed)
        .expect("the doomed session is still pooled")
        .shutdown()
        .await;
    assert!(delivered, "the session task takes the shutdown");
    wait_for_gone(&pool, doomed).await;

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(doomed.0),
            live_only: true,
            image_regions: false,
        },
    )
    .await;
    let refused = reader
        .next_frame()
        .await
        .unwrap()
        .expect("a refusal, not a bare EOF");
    let reason = match codec::decode::<SessionToClientMsg>(&refused.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, .. } => reason,
        other => panic!("expected AttachFailed, got {other:?}"),
    };
    assert!(
        matches!(
            reason,
            AttachRefusal::Attach(AttachFailure::UnknownSession | AttachFailure::SessionExited)
        ),
        "a vanished target must refuse with a reason that says re-pick, got {reason:?}"
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_request(&mut writer, &OpsToDaemonMsg::List, 1).await;
    let roster = listed_roster(&mut reader).await;
    assert!(
        !roster.iter().any(|row| row.id == doomed.0),
        "the refetch must not still offer the session that just went away"
    );
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(survivor.0),
            live_only: true,
            image_regions: false,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, survivor.0);
}

/// `SessionInfo::sequence` orders the switch ring: strictly increasing per
/// creation, stable for a session's life, never reused after a reap.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_sequences_are_monotonic_and_never_reassigned() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let mut created = Vec::new();
    for _ in 0..3 {
        let session_task::SessionLifecycle { id, info, .. } = session_task::spawn_owned(
            &pool,
            owned_session("sleep 30"),
            IdlePolicy::default(),
            SessionId::new(),
            0,
            0,
            Vec::new(),
            None,
            Listing::Public,
        )
        .await;
        created.push((id, info.sequence));
    }
    let sequences: Vec<NonZeroU64> = created.iter().map(|&(_, seq)| seq).collect();
    assert!(
        sequences.windows(2).all(|w| w[0] < w[1]),
        "creation order must run strictly upward, got {sequences:?}"
    );

    let listed = build_session_list(&pool).await;
    for &(id, sequence) in &created {
        let row = listed
            .iter()
            .find(|row| row.id == id.0)
            .expect("every live session is listed");
        assert_eq!(
            row.sequence, sequence,
            "a listing must not renumber a session"
        );
    }

    pool.lock().await.remove(created[0].0);
    let session_task::SessionLifecycle { info: fresh, .. } = session_task::spawn_owned(
        &pool,
        owned_session("sleep 30"),
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;
    assert!(
        fresh.sequence > sequences[2],
        "a reap leaves a gap; it does not hand its place to the next creation"
    );
}

/// `Created` carries the post-subscribe roster row: the row the spawn
/// minted lists no attachment and would tell the creator its own window
/// is not there.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_ack_reports_the_window_it_attached() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let info = create_and_attach_info(&mut reader, &mut writer).await;

    assert_eq!(
        info.attachments.len(),
        1,
        "the create ack's roster row must list the subscriber it answers: {info:?}"
    );
}

/// The roster row carries each window's carrier: the relay's SSH values
/// for a connection that opened with a carrier block, `Local` with the
/// connecting process's pid for one that did not.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_attachment_reports_the_carrier_it_arrived_on() {
    use felis_transport::preface::write_carrier_block;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));

    let (read_half, mut write_half) = connect(&path).await.unwrap();
    let block = CarrierBlock {
        env: vec![entry("SSH_CONNECTION", "192.0.2.7 50122 192.0.2.1 22")],
    };
    write_carrier_block(&mut write_half, &block).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let relayed = create_and_attach_info(&mut reader, &mut writer).await;
    assert_eq!(
        relayed.attachments[0].carrier,
        Some(AttachCarrier::Relay {
            ssh_env: Some(SshEnv {
                connection: Some("192.0.2.7 50122 192.0.2.1 22".into()),
                ..SshEnv::default()
            }),
        })
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let local = create_and_attach_info(&mut reader, &mut writer).await;
    let client_pid = if cfg!(any(target_os = "linux", target_vendor = "apple")) {
        NonZeroU32::new(std::process::id())
    } else {
        None
    };
    assert_eq!(
        local.attachments[0].carrier,
        Some(AttachCarrier::Local { client_pid })
    );
}

/// The roster row names the directory the create asked for, though the
/// child never announces one with OSC 7.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_ack_reports_the_directory_the_child_started_in() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));
    let dir = tmp.path().to_str().unwrap().to_owned();

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Create {
            args: felis_protocol::messages::SpawnArgs {
                cwd: dir.clone(),
                ..Default::default()
            },
            image_regions: false,
        },
    )
    .await;
    let created = reader.next_frame().await.unwrap().expect("session-created");
    let info = match codec::decode::<SessionToClientMsg>(&created.body).unwrap() {
        SessionToClientMsg::Created { info } => info,
        other => panic!("expected SessionToClientMsg::Created, got {other:?}"),
    };

    assert_eq!(info.spawn_cwd, Some(dir));
    assert_eq!(info.cwd, None);
}

/// Stamped at creation and never renumbered: an attach that renumbered it
/// would move the window's anchor mid-switch.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_attach_does_not_move_a_session_in_the_ring() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach_info(&mut reader, &mut writer).await;
    let first = id.sequence;
    let id = id.id;
    drop(reader);
    drop(writer);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
            image_regions: false,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.sequence, first);
}

/// The refusal comes from the session actor, the only place that sees both
/// the exit and the subscribe; the roster is a snapshot and cannot close
/// the window.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_only_attach_is_refused_on_a_corpse_but_a_deliberate_one_lands() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_secs(60),
            drain_interval: Duration::from_millis(10),
        },
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("exit 0"), caps);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;
    wait_for_exited(&pool, SessionId(id)).await;
    drop(reader);
    drop(writer);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: true,
            image_regions: false,
        },
    )
    .await;
    let refused = reader
        .next_frame()
        .await
        .unwrap()
        .expect("a refusal, not EOF");
    match codec::decode::<SessionToClientMsg>(&refused.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, .. } => {
            assert_eq!(reason, AttachRefusal::Attach(AttachFailure::SessionExited));
        }
        other => panic!("expected AttachFailed, got {other:?}"),
    }

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
            image_regions: false,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, id);
}

/// A parked session must keep draining its PTY. The child floods 24 MiB,
/// past `felis-pty`'s bounded reader channel (256 × 64 KiB) plus the kernel
/// PTY buffer; a non-draining daemon wedges it and the pool never empties.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_session_keeps_draining_so_child_does_not_block() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("sleep 0.3; dd if=/dev/zero bs=1048576 count=24 2>/dev/null");
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_millis(50),
            drain_interval: Duration::from_millis(10),
        },
        ..DaemonCaps::default()
    };

    spawn_daemon_with_caps(&path, pool.clone(), factory, caps);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;

    let id = create_and_attach(&mut reader, &mut writer).await;

    // The 0.3 s pre-flood sleep keeps the wire quiet, so a short read
    // timeout means fully attached.
    while tokio::time::timeout(Duration::from_millis(100), reader.next_frame())
        .await
        .is_ok_and(|frame| frame.is_ok_and(|f| f.is_some()))
    {}

    drop(reader);
    drop(writer);

    let parked_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let subscribers = pool
            .lock()
            .await
            .get(SessionId(id))
            .map(|h| h.meta_snapshot().subscribers);
        match subscribers {
            Some(0) | None => break,
            Some(_) => assert!(
                tokio::time::Instant::now() < parked_deadline,
                "session never parked after detach"
            ),
        }
    }

    // The parked drain runs at the sink pacer's sustained rate
    // (`crate::parse_sink`, ≈10 MiB/s), so 24 MiB takes a few seconds; the
    // deadline is generous because the parallel suite slows the drain and a
    // tight bound flakes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if pool.lock().await.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "detached child never completed — drain backpressure regressed"
        );
    }
}

/// PTY EOF expires working and blocked records once the child's last
/// output is applied: the `done` written just before `exit` stays, and
/// the session lists it through the post-exit grace. EOF clears no
/// terminal progress.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pty_eof_expires_transient_records_and_keeps_progress_after_the_final_report() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory(concat!(
        r"printf '\033]7501;id=w:state=working\033\\\033]7501;id=b:state=blocked\007",
        r"\033]7501;id=i:state=idle\007\033]7501;state=done\033\\\033]9;4;2;71\007'; exit 0",
    ));
    let caps = DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_secs(60),
            drain_interval: Duration::from_millis(10),
        },
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), factory, caps);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let listed = loop {
        let meta = pool
            .lock()
            .await
            .handle_cloned(SessionId(id))
            .map(|handle| handle.meta_snapshot());
        let ids: Option<Vec<(String, String)>> = meta.as_ref().map(|meta| {
            meta.program_status
                .iter()
                .map(|r| (r.id.clone(), r.state.clone()))
                .collect()
        });
        if let Some(meta) = meta.as_ref().filter(|meta| meta.exited)
            && ids.as_ref().is_some_and(|ids| ids.len() == 2)
        {
            assert_eq!(
                meta.terminal_progress,
                Some(felis_grid::progress::Progress {
                    state: felis_grid::progress::ProgressState::Error,
                    value: Some(71),
                })
            );
            break ids.unwrap_or_default();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the exited session never settled to two records: {ids:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        listed,
        [
            (String::new(), "done".to_owned()),
            ("i".to_owned(), "idle".to_owned())
        ]
    );
}

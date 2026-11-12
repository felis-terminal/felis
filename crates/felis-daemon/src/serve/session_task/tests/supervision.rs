use super::*;
use crate::test_support::{read_pid, waits_for_exit};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervise_removes_pool_entry_when_the_task_panics() {
    let id = SessionId::new();
    let (cmd, _cmd_rx) = mpsc::channel(1);
    let handle = SessionHandle {
        cmd,
        input_budget: crate::pool::new_input_budget(),
        resizer: None,
        upgrade: None,
        publication: crate::pool::Publication::new(),
        shutdown: Arc::default(),
        meta: Arc::new(StdMutex::new(SessionMeta {
            rows: crate::DEFAULT_ROWS,
            cols: crate::DEFAULT_COLS,
            pixel_w: 0,
            pixel_h: 0,
            title: None,
            cwd: None,
            idle_since: Instant::now(),
            subscribers: 0,
            tags: std::collections::BTreeSet::new(),
            last_notification: None,
            last_exit_code: None,
            exited: false,
            attachments: Vec::new(),
            program_status: Vec::new(),
            spawn_cwd: None,
            terminal_progress: None,
            sequence: std::num::NonZeroU64::MIN,
        })),
    };
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    pool.lock()
        .await
        .register(id, handle, None, Listing::Public);

    let task = tokio::spawn(async { panic!("boom") });
    supervise(
        task,
        Arc::downgrade(&pool),
        id,
        exited_child(),
        crate::pool::Publication::new(),
        watch::channel(false).0,
    )
    .await;

    assert!(
        pool.lock().await.handle_cloned(id).is_none(),
        "the panicked task's entry must be gone from the pool"
    );
}

fn bare_handle(publication: Arc<crate::pool::Publication>) -> SessionHandle {
    let (cmd, _cmd_rx) = mpsc::channel(1);
    SessionHandle {
        cmd,
        input_budget: crate::pool::new_input_budget(),
        resizer: None,
        upgrade: None,
        publication,
        shutdown: Arc::default(),
        meta: Arc::new(StdMutex::new(SessionMeta {
            rows: crate::DEFAULT_ROWS,
            cols: crate::DEFAULT_COLS,
            pixel_w: 0,
            pixel_h: 0,
            title: None,
            cwd: None,
            idle_since: Instant::now(),
            subscribers: 0,
            tags: std::collections::BTreeSet::new(),
            last_notification: None,
            last_exit_code: None,
            exited: false,
            attachments: Vec::new(),
            program_status: Vec::new(),
            spawn_cwd: None,
            terminal_progress: None,
            sequence: std::num::NonZeroU64::MIN,
        })),
    }
}

/// A task that dies takes its own lifecycle items with it; its
/// supervisor sends what the roster implied, whether or not the task
/// had announced the row, and nothing for a row nobody could name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicked_task_owes_observers_the_lifecycle_the_roster_showed() {
    for (listing, announced, expected) in [
        (Listing::Public, false, 2),
        (Listing::Public, true, 1),
        (Listing::Held, false, 0),
    ] {
        let id = SessionId::new();
        let publication = crate::pool::Publication::new();
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let mut rx = {
            let mut guard = pool.lock().await;
            guard.register(id, bare_handle(Arc::clone(&publication)), None, listing);
            guard.subscribe_events()
        };
        if announced {
            assert!(publication.announce());
        }
        let task = tokio::spawn(async { panic!("boom") });
        supervise(
            task,
            Arc::downgrade(&pool),
            id,
            exited_child(),
            Arc::clone(&publication),
            watch::channel(false).0,
        )
        .await;

        let items: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let want: Vec<_> = [
            NotifyToClientMsg::SessionAdded { session_id: id.0 },
            NotifyToClientMsg::SessionRemoved { session_id: id.0 },
        ]
        .into_iter()
        .skip(2 - expected)
        .collect();
        assert_eq!(
            items, want,
            "{listing:?}, announced {announced}: what observers hear of a dead task",
        );
    }
}

/// A destroy queued ahead of the parse drain still lets the session
/// publish what it parsed, and `SessionRemoved` comes after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destroy_ahead_of_the_drain_publishes_what_was_parsed() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser
            .advance(&mut core.grid, b"\x1b]133;A\x07\x1b]7501;state=working\x07");
    }
    let publication = crate::pool::Publication::new();
    let mut pool = SessionPool::new();
    pool.register(
        task.id,
        bare_handle(Arc::clone(&publication)),
        None,
        Listing::Public,
    );
    let (hub, mut rx) = tokio::sync::broadcast::channel(64);
    task.observers = ObserverOutbox::new(hub, task.id, publication);
    task.observers.publish();
    let session_id = task.id.0;

    let (cmd_tx, cmd_rx) = mpsc::channel(4);
    assert!(task.shutdown.send(&cmd_tx).await, "queued");
    run_session(task, cmd_rx).await;

    let items: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(
        items,
        [
            NotifyToClientMsg::SessionAdded { session_id },
            NotifyToClientMsg::PromptShown { session_id },
            NotifyToClientMsg::ProgramStatus {
                session_id,
                records: vec![felis_protocol::messages::ProgramStatusRecord {
                    id: String::new(),
                    state: "working".to_owned(),
                    kind: None,
                    progress: None,
                    app: None,
                    title: None,
                    msg: None,
                }],
            },
            NotifyToClientMsg::SessionRemoved { session_id },
        ]
    );
}

/// Before publication nobody can see a session's items, so a burst
/// stays bounded: program status keeps only its latest set, row
/// hints merge, and prompt items beyond the cap give way oldest
/// first, the loss announced right behind `SessionAdded`.
#[test]
fn a_held_session_keeps_a_bounded_outbox() {
    let id = SessionId::new();
    let session_id = id.0;
    let publication = crate::pool::Publication::new();
    let (hub, mut rx) = tokio::sync::broadcast::channel(1024);
    let mut outbox = ObserverOutbox::new(hub, id, Arc::clone(&publication));
    let overflow = 10;
    for n in 0..HELD_PROMPT_ITEMS + overflow {
        outbox.send(NotifyToClientMsg::CommandFinished {
            session_id,
            exit_code: Some(u32::try_from(n).unwrap()),
        });
        outbox.send(NotifyToClientMsg::ProgramStatus {
            session_id,
            records: Vec::new(),
        });
    }
    outbox.send(NotifyToClientMsg::SessionChanged {
        session_id,
        fields: vec![SessionField::Title],
    });
    outbox.send(NotifyToClientMsg::SessionChanged {
        session_id,
        fields: vec![SessionField::Cwd],
    });
    assert!(rx.try_recv().is_err(), "a held row publishes nothing");

    let mut pool = SessionPool::new();
    pool.register(id, bare_handle(publication), None, Listing::Public);
    outbox.publish();
    let items: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

    let mut want = vec![
        NotifyToClientMsg::SessionAdded { session_id },
        NotifyToClientMsg::Lagged {
            missed: u64::try_from(overflow).unwrap(),
        },
    ];
    want.extend((overflow..HELD_PROMPT_ITEMS + overflow).map(|n| {
        NotifyToClientMsg::CommandFinished {
            session_id,
            exit_code: Some(u32::try_from(n).unwrap()),
        }
    }));
    want.push(NotifyToClientMsg::ProgramStatus {
        session_id,
        records: Vec::new(),
    });
    want.push(NotifyToClientMsg::SessionChanged {
        session_id,
        fields: vec![SessionField::Title, SessionField::Cwd],
    });
    assert_eq!(items, want);
}

fn exited_child() -> Arc<ChildHandle> {
    let cmd = sh("exit 0", "/bin:/usr/bin");
    let spawned = crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh");
    Arc::new(spawned.pty.child)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destroying_a_session_ends_its_shell() {
    let dir = tempfile::TempDir::new().unwrap();
    let pidfile = dir.path().join("pid");
    // External `sleep`, not a builtin blocking on the PTY: it does
    // not end when the master closes, so only the signal can end it.
    let cmd = sh(
        &format!("echo $$ > {}; sleep 60", pidfile.display()),
        crate::test_support::fixture_path(),
    );
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let SessionLifecycle { id, .. } = spawn_session(
        &pool,
        crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"),
        None,
        IdlePolicy::default(),
        SessionId::new(),
        GridDims {
            rows: crate::DEFAULT_ROWS,
            cols: crate::DEFAULT_COLS,
            pixel_w: 0,
            pixel_h: 0,
        },
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;
    let pid = read_pid(&pidfile).await;

    let handle = pool.lock().await.handle_cloned(id).expect("registered");
    assert!(handle.shutdown().await, "send");

    assert!(
        waits_for_exit(pid).await,
        "the destroyed session's shell is still running",
    );
}

/// Panicking tasks terminate their child process even if it ignores `SIGHUP`.
///
/// `trap '' HUP` survives `exec` to verify the hangup, grace, and `SIGKILL` sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervise_ends_the_child_when_the_task_panics() {
    let dir = tempfile::TempDir::new().unwrap();
    let pidfile = dir.path().join("pid");
    let cmd = sh(
        &format!(
            "trap '' HUP; echo $$ > {}; exec sleep 60",
            pidfile.display()
        ),
        crate::test_support::fixture_path(),
    );
    let session = Session::from_spawned(crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"));
    let pid = read_pid(&pidfile).await;
    let child = Arc::clone(&session.child);

    // The real panic path drops the session before the watchdog
    // sees the `JoinError`; same order here, so the drop-side
    // hangup is not what the test proves.
    drop(session);

    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let task = tokio::spawn(async { panic!("boom") });
    supervise(
        task,
        Arc::downgrade(&pool),
        SessionId::new(),
        child,
        crate::pool::Publication::new(),
        watch::channel(false).0,
    )
    .await;

    assert!(
        waits_for_exit(pid).await,
        "the panicked task's shell is still running",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervise_leaves_the_pool_alone_on_clean_exit() {
    let id = SessionId::new();
    let (cmd, _cmd_rx) = mpsc::channel(1);
    let handle = SessionHandle {
        cmd,
        input_budget: crate::pool::new_input_budget(),
        resizer: None,
        upgrade: None,
        publication: crate::pool::Publication::new(),
        shutdown: Arc::default(),
        meta: Arc::new(StdMutex::new(SessionMeta {
            rows: crate::DEFAULT_ROWS,
            cols: crate::DEFAULT_COLS,
            pixel_w: 0,
            pixel_h: 0,
            title: None,
            cwd: None,
            idle_since: Instant::now(),
            subscribers: 0,
            tags: std::collections::BTreeSet::new(),
            last_notification: None,
            last_exit_code: None,
            exited: false,
            attachments: Vec::new(),
            program_status: Vec::new(),
            spawn_cwd: None,
            terminal_progress: None,
            sequence: std::num::NonZeroU64::MIN,
        })),
    };
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    pool.lock()
        .await
        .register(id, handle, None, Listing::Public);

    let task = tokio::spawn(async {});
    supervise(
        task,
        Arc::downgrade(&pool),
        id,
        exited_child(),
        crate::pool::Publication::new(),
        watch::channel(false).0,
    )
    .await;

    assert!(
        pool.lock().await.handle_cloned(id).is_some(),
        "a clean exit must not touch the pool entry"
    );
}

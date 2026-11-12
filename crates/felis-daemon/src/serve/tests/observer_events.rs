//! The observer kinds past notifications: what each session publishes,
//! in which order, and what a subscriber that did not ask receives.

use felis_protocol::messages::{
    ExitStatus, NotifyToClientMsg, ObserverKind, ProgramStatusRecord, SessionField,
};
use tokio::sync::broadcast;

use super::*;

/// Prints a prompt mark, then a title the held row's meta shows once
/// the cycle that parsed the mark has run.
const PROMPT_THEN_TITLE: &str = "printf '\\033]133;A\\007\\033]2;ready\\007'; exec sleep 30";
const ITEM_DEADLINE: Duration = Duration::from_secs(10);

/// The title lands in the meta mid-cycle, ahead of that cycle's
/// observer items; the stats reply comes only after the cycle ends.
async fn await_prompt_handled(pool: &Arc<Mutex<SessionPool>>, registered: &Registered) {
    let deadline = tokio::time::Instant::now() + ITEM_DEADLINE;
    loop {
        let title = pool
            .lock()
            .await
            .row_even_held(registered.id)
            .and_then(|row| row.meta.lock().unwrap().title.clone());
        if title.as_deref() == Some("ready") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held row never parsed its prompt"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (reply, stats) = oneshot::channel();
    assert!(
        registered
            .cmd
            .send(SessionCmd::Stats { reply })
            .await
            .is_ok()
    );
    let answered = tokio::time::timeout(ITEM_DEADLINE, stats).await;
    assert!(matches!(answered, Ok(Ok(_))), "the task answers");
}

fn quick_reap() -> DaemonCaps {
    DaemonCaps {
        idle: IdlePolicy {
            post_exit_grace: Duration::from_millis(50),
            ..IdlePolicy::default()
        },
        ..DaemonCaps::default()
    }
}

async fn next_item(rx: &mut broadcast::Receiver<NotifyToClientMsg>) -> NotifyToClientMsg {
    tokio::time::timeout(ITEM_DEADLINE, rx.recv())
        .await
        .expect("an observer item in time")
        .expect("the events ring stays open")
}

/// Items for `id` up to and including its `SessionRemoved`.
async fn items_until_removed(
    rx: &mut broadcast::Receiver<NotifyToClientMsg>,
    id: u128,
) -> Vec<NotifyToClientMsg> {
    let mut items = Vec::new();
    loop {
        let item = next_item(rx).await;
        if item.session_id() != Some(id) {
            continue;
        }
        let last = matches!(item, NotifyToClientMsg::SessionRemoved { .. });
        items.push(item);
        if last {
            return items;
        }
    }
}

fn record(state: &str) -> ProgramStatusRecord {
    ProgramStatusRecord {
        id: String::new(),
        state: state.to_owned(),
        kind: None,
        progress: None,
        app: None,
        title: None,
        msg: None,
    }
}

/// A published session's items follow `SessionAdded` in parse order,
/// each program-status change as its whole record set, then one exit and
/// `SessionRemoved` last.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_publishes_its_items_in_the_order_it_produced_them() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let mut rx = pool.lock().await.subscribe_events();
    let factory = shell_factory(
        "sleep 1; printf '\\033]133;D;3\\007\\033]7501;state=working\\007\\033]133;A\\007\
         \\033]7501;state=done\\007'; sleep 1; exit 5",
    );
    let caps = quick_reap();
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
    drop(registered.publish().await);

    let session_id = info.id;
    assert_eq!(
        items_until_removed(&mut rx, session_id).await,
        [
            NotifyToClientMsg::SessionAdded { session_id },
            NotifyToClientMsg::CommandFinished {
                session_id,
                exit_code: Some(3),
            },
            NotifyToClientMsg::ProgramStatus {
                session_id,
                records: vec![record("working")],
            },
            NotifyToClientMsg::ProgramStatus {
                session_id,
                records: Vec::new(),
            },
            NotifyToClientMsg::PromptShown { session_id },
            NotifyToClientMsg::ProgramStatus {
                session_id,
                records: vec![record("done")],
            },
            NotifyToClientMsg::SessionExited {
                session_id,
                status: Some(ExitStatus::Code(5)),
            },
            NotifyToClientMsg::SessionRemoved { session_id },
        ]
    );
}

/// A kill takes the row out of the pool before the task hears of it;
/// the task still has the last word, and a child it never saw exit is
/// reported as no exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_killed_while_running_is_removed_without_an_exit() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let mut rx = pool.lock().await.subscribe_events();
    let factory = shell_factory("printf '\\033]133;A\\007'; exec sleep 30");
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
    drop(registered.publish().await);
    let session_id = info.id;
    assert_eq!(
        next_item(&mut rx).await,
        NotifyToClientMsg::SessionAdded { session_id }
    );
    assert_eq!(
        next_item(&mut rx).await,
        NotifyToClientMsg::PromptShown { session_id }
    );

    let handle = pool
        .lock()
        .await
        .remove(SessionId(session_id))
        .expect("the row");
    let delivered = handle.shutdown().await;
    assert!(delivered, "the session task takes the shutdown");
    assert_eq!(
        items_until_removed(&mut rx, session_id).await,
        [NotifyToClientMsg::SessionRemoved { session_id }]
    );
}

/// The pool can make a row nameable and the task end before the
/// creator's signal reaches it: the roster showed the session, so the
/// stream still says it arrived and left, and what the held row parsed
/// goes out between the two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_published_after_its_task_ended_is_still_added_and_removed() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let mut rx = pool.lock().await.subscribe_events();
    let factory = shell_factory(PROMPT_THEN_TITLE);
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
    await_prompt_handled(&pool, &registered).await;
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "a held row publishes nothing",
    );
    let session_id = info.id;
    assert!(pool.lock().await.publish(SessionId(session_id)));
    let shutdown = Arc::clone(&registered.shutdown);
    let cmd = registered.keep();
    assert!(shutdown.send(&cmd).await, "the task is alive");
    // The title that synced the test may or may not change the row the
    // stream already announced, depending on where the shutdown lands.
    let mut items = items_until_removed(&mut rx, session_id).await;
    items.retain(|item| !matches!(item, NotifyToClientMsg::SessionChanged { .. }));
    assert_eq!(
        items,
        [
            NotifyToClientMsg::SessionAdded { session_id },
            NotifyToClientMsg::PromptShown { session_id },
            NotifyToClientMsg::SessionRemoved { session_id },
        ]
    );
}

/// A create nobody was told of leaves no trace on the stream, not even
/// a removal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rolled_back_create_publishes_nothing() {
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let mut rx = pool.lock().await.subscribe_events();
    let factory = shell_factory(PROMPT_THEN_TITLE);
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
    await_prompt_handled(&pool, &registered).await;
    registered.roll_back().await;
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "a rolled-back row publishes nothing",
    );
}

/// A program left behind holds the PTY open, so no EOF comes; with a
/// window attached the exit is still seen, from the child itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exit_under_an_attached_window_is_reported_without_eof() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let mut rx = pool.lock().await.subscribe_events();
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30 & exit 7"));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let session_id = create_and_attach(&mut reader, &mut writer).await;

    let exited = loop {
        if let item @ NotifyToClientMsg::SessionExited { .. } = next_item(&mut rx).await {
            break item;
        }
    };
    assert_eq!(
        exited,
        NotifyToClientMsg::SessionExited {
            session_id,
            status: Some(ExitStatus::Code(7)),
        }
    );
    drop((reader, writer));
}

struct Observer {
    reader: FrameReader<felis_transport::local::ReadHalf>,
    _writer: FrameWriter<felis_transport::local::WriteHalf>,
}

impl Observer {
    async fn subscribe(path: &Path, kinds: Vec<ObserverKind>) -> Self {
        let (mut reader, mut writer) = dial(path, ConnectionMode::Observer, false).await;
        send_stream(
            &mut writer,
            &NotifyToDaemonMsg::Subscribe {
                session_prefix: None,
                kinds,
            },
            1,
        )
        .await;
        let ack = reader.next_frame().await.unwrap().expect("subscribed ack");
        assert!(matches!(
            codec::decode::<NotifyToClientMsg>(&ack.body).unwrap(),
            NotifyToClientMsg::Subscribed { filter: None }
        ));
        Self {
            reader,
            _writer: writer,
        }
    }

    async fn next(&mut self) -> NotifyToClientMsg {
        let frame = tokio::time::timeout(ITEM_DEADLINE, self.reader.next_frame())
            .await
            .expect("an item in time")
            .unwrap()
            .expect("an item, not EOF");
        codec::decode::<NotifyToClientMsg>(&frame.body).unwrap()
    }
}

fn notification(session_id: u128) -> NotifyToClientMsg {
    NotifyToClientMsg::Event {
        session_id,
        notification: Notification {
            title: None,
            body: "ping".into(),
            urgency: felis_protocol::messages::Urgency::Normal,
        },
        notify_id: None,
        session_title: None,
        cwd: None,
        attached: false,
    }
}

/// A subscribe that names no kind is the one every older client sends:
/// it hears notifications and nothing published on the other ring. A
/// subscriber hears only the kinds it named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observer_hears_only_the_kinds_it_asked_for() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));
    let mut default = Observer::subscribe(&path, Vec::new()).await;
    let mut lifecycle = Observer::subscribe(&path, vec![ObserverKind::Lifecycle]).await;

    let (notify, events) = {
        let guard = pool.lock().await;
        (guard.notify_hub(), guard.events_hub())
    };
    for item in [
        NotifyToClientMsg::PromptShown { session_id: 1 },
        NotifyToClientMsg::SessionAdded { session_id: 1 },
    ] {
        events.send(item).expect("both observers hold the ring");
    }
    notify
        .send(notification(2))
        .expect("the default observer holds the ring");
    events
        .send(NotifyToClientMsg::SessionRemoved { session_id: 3 })
        .expect("the lifecycle observer holds the ring");

    assert_eq!(default.next().await, notification(2));
    assert_eq!(
        lifecycle.next().await,
        NotifyToClientMsg::SessionAdded { session_id: 1 }
    );
    assert_eq!(
        lifecycle.next().await,
        NotifyToClientMsg::SessionRemoved { session_id: 3 }
    );
}

/// The consistency recipe: subscribe, then list. A session an observer
/// hears added is already in the `list` it runs on that item, and a
/// relabel it hears names the field to re-read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_list_run_on_an_item_already_shows_what_the_item_reports() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));
    let mut observer =
        Observer::subscribe(&path, vec![ObserverKind::Lifecycle, ObserverKind::Meta]).await;

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Ops, false).await;
    send_request(
        &mut writer,
        &OpsToDaemonMsg::Spawn {
            args: felis_protocol::messages::SpawnArgs::default(),
        },
        1,
    )
    .await;
    let spawned = reader.next_frame().await.unwrap().expect("spawned");
    let OpsToClientMsg::Spawned {
        outcome: SpawnOutcome::Ok { info },
    } = codec::decode::<OpsToClientMsg>(&spawned.body).unwrap()
    else {
        panic!("expected a spawned session");
    };
    let session_id = info.id;

    assert_eq!(
        observer.next().await,
        NotifyToClientMsg::SessionAdded { session_id }
    );
    send_request(&mut writer, &OpsToDaemonMsg::List, 2).await;
    let listed = reader.next_frame().await.unwrap().expect("listed");
    let OpsToClientMsg::Listed { sessions } =
        codec::decode::<OpsToClientMsg>(&listed.body).unwrap()
    else {
        panic!("expected the roster");
    };
    assert!(sessions.iter().any(|row| row.id == session_id));

    for (request, expect_item) in [(3, true), (4, false)] {
        send_request(
            &mut writer,
            &OpsToDaemonMsg::Tag {
                id_prefix: hex(session_id),
                add: vec!["agent".to_owned()],
                remove: Vec::new(),
            },
            request,
        )
        .await;
        let tagged = reader.next_frame().await.unwrap().expect("tagged");
        assert!(matches!(
            codec::decode::<OpsToClientMsg>(&tagged.body).unwrap(),
            OpsToClientMsg::TagsUpdated { .. }
        ));
        if expect_item {
            assert_eq!(
                observer.next().await,
                NotifyToClientMsg::SessionChanged {
                    session_id,
                    fields: vec![SessionField::Tags],
                }
            );
        }
    }
    let delivered = // A tag that was already there changes nothing, so the next item is
    // the removal rather than a second relabel.
    pool.lock()
        .await
        .handle_cloned(SessionId(session_id))
        .expect("the row")
        .shutdown()
        .await;
    assert!(delivered, "the session task takes the shutdown");
    assert_eq!(
        observer.next().await,
        NotifyToClientMsg::SessionRemoved { session_id }
    );
}

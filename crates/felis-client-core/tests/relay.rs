//! The JSON protocol relay against an in-process daemon: which
//! connection each request and stream rides shows in what keeps
//! answering while another stalls, and in how a stream ends.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use felis_client_core::connector::RemoteSpawn;
use felis_client_core::json::{
    AttachTarget, Base64, ErrorKind, InputJson, Item, OpResult, Open, RegionSourceJson, Request,
    SearchSourceJson, SendStepJson, SessionId, SpawnArgsJson, session_id_hex,
};
use felis_client_core::relay::{InputError, Relay, RelayConfig, RelayStream, StreamMessage, Theme};
use felis_client_core::{Carrier, DEFAULT_STREAM_BUFFER};
use felis_daemon::SessionPool;
use felis_daemon::serve::{DaemonCaps, serve_unix};
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

const PATIENCE: Duration = Duration::from_secs(10);

fn private_dir() -> TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = TempDir::new().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

async fn await_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{} never bound", path.display());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn daemon(tmp: &TempDir) -> PathBuf {
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let server_path = path.clone();
    drop(tokio::spawn(async move {
        drop(serve_unix(&server_path, DaemonCaps::default(), pool).await);
    }));
    await_socket(&path).await;
    path
}

async fn relay_on(socket: &Path, theme: Theme) -> Relay {
    Relay::connect(RelayConfig {
        carrier: Carrier::Local(socket.to_path_buf().into()),
        remote_spawn: RemoteSpawn::Refuse,
        theme,
    })
    .await
    .unwrap()
}

fn sh(script: &str) -> SpawnArgsJson {
    serde_json::from_value(serde_json::json!({
        "command": "/bin/sh",
        "args": ["-c", script],
    }))
    .unwrap()
}

async fn create(relay: &Relay, script: &str) -> SessionId {
    let reply = relay
        .request(Request::Create { args: sh(script) })
        .await
        .unwrap();
    let OpResult::Create { session } = reply else {
        panic!("a create answered {reply:?}");
    };
    SessionId(u128::from_str_radix(&session.session.id, 16).unwrap())
}

fn raw(bytes: &[u8]) -> SendStepJson {
    SendStepJson::Raw {
        bytes: Base64(bytes.to_vec()),
    }
}

/// The stream's messages through its terminal.
async fn drain(stream: &mut RelayStream) -> Vec<StreamMessage> {
    let mut seen = Vec::new();
    loop {
        let message = timeout(PATIENCE, stream.next())
            .await
            .expect("the stream reached no terminal");
        let terminal = message.is_terminal();
        seen.push(message);
        if terminal {
            return seen;
        }
    }
}

/// Every printable row text a grid item carries.
fn row_texts(item: &Item) -> Vec<String> {
    let value = serde_json::to_value(item).unwrap();
    value["msg"]["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            row["cells"]["graphemes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| g["byte"].as_u64().map_or(' ', |b| char::from(b as u8)))
                .collect()
        })
        .collect()
}

async fn screen_contains(relay: &Relay, session: SessionId, needle: &str) -> bool {
    let mut capture = relay
        .open(Open::Capture {
            session,
            source: RegionSourceJson::Visible,
            ansi: false,
            max_rows: None,
        })
        .await
        .unwrap();
    drain(&mut capture).await.iter().any(|message| {
        matches!(message, StreamMessage::Item(Item::Row(row)) if row.text.contains(needle))
    })
}

async fn until_screen_contains(relay: &Relay, session: SessionId, needle: &str) {
    let deadline = Instant::now() + PATIENCE;
    while !screen_contains(relay, session, needle).await {
        assert!(
            Instant::now() < deadline,
            "`{needle}` never reached the screen"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn attachments(relay: &Relay, session: SessionId) -> usize {
    match relay.request(Request::Info { session }).await.unwrap() {
        OpResult::Info { session } => session.session.attachments.len(),
        other => panic!("an info answered {other:?}"),
    }
}

async fn until_detached(relay: &Relay, session: SessionId) {
    let deadline = Instant::now() + PATIENCE;
    while attachments(relay, session).await != 0 {
        assert!(
            Instant::now() < deadline,
            "the session still lists the attach"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_and_streams_answer_through_the_relay() {
    let tmp = private_dir();
    let relay = relay_on(&daemon(&tmp).await, Theme::default()).await;
    let session = create(
        &relay,
        "read a; printf 'marker-one\\n'; printf '\\033]9;ping\\007'; \
         read b; printf '\\033]133;D;7\\007'; exec sleep 600",
    )
    .await;

    let mut notifications = relay
        .open(Open::Notifications {
            session: Some(session),
        })
        .await
        .unwrap();
    let sent = relay
        .request(Request::Send {
            session,
            steps: vec![raw(b"a\n")],
        })
        .await
        .unwrap();
    assert!(matches!(sent, OpResult::Send(ref r) if r.id == session_id_hex(session.0)));
    let notified = timeout(PATIENCE, notifications.next()).await.unwrap();
    assert!(
        matches!(&notified, StreamMessage::Item(Item::Notification(n)) if n.body == "ping" && n.session_id == session),
        "{notified:?}"
    );

    let mut search = relay
        .open(Open::Search {
            session,
            query: "marker-one".to_owned(),
            regex: false,
            case_insensitive: false,
            source: SearchSourceJson::Scrollback,
        })
        .await
        .unwrap();
    let found = drain(&mut search).await;
    assert!(matches!(found[0], StreamMessage::Item(Item::Match(_))));
    assert_eq!(
        found.last(),
        Some(&StreamMessage::End {
            count: 1,
            exit_code: None
        })
    );

    let mut wait = relay
        .open(Open::Wait {
            session,
            steps: vec![raw(b"b\n")],
        })
        .await
        .unwrap();
    assert_eq!(
        drain(&mut wait).await,
        [
            StreamMessage::Item(Item::Sent { id: session }),
            StreamMessage::Item(Item::Mark { exit_code: Some(7) }),
            StreamMessage::End {
                count: 2,
                exit_code: None
            },
        ]
    );

    let tagged = relay
        .request(Request::Tag {
            session,
            add: vec!["build".to_owned()],
            remove: Vec::new(),
        })
        .await
        .unwrap();
    assert!(matches!(tagged, OpResult::Tag(ref t) if t.tags == ["build"]));
    let OpResult::List { sessions } = relay.request(Request::List).await.unwrap() else {
        panic!("a list answered something else");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session.tags, ["build"]);
    assert!(sessions[0].sequence >= 1);

    relay.request(Request::Kill { session }).await.unwrap();
    let gone = relay.request(Request::Info { session }).await.unwrap_err();
    assert_eq!(gone.kind, ErrorKind::NoMatch);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_reports_the_embedders_theme_and_carries_input() {
    let tmp = private_dir();
    let relay = relay_on(
        &daemon(&tmp).await,
        Theme {
            bg: Some((0x12, 0x34, 0x56)),
            ..Theme::default()
        },
    )
    .await;
    let mut attach = relay
        .open(Open::Attach {
            target: AttachTarget::Create(sh(
                "read go; stty -echo -icanon min 1; printf '\\033]11;?\\007'; \
                 dd bs=1 count=20 2>/dev/null | tr -d '\\033'; echo; exec sleep 600",
            )),
        })
        .await
        .unwrap();
    let first = timeout(PATIENCE, attach.next()).await.unwrap();
    let StreamMessage::Item(Item::Attached { session: row, .. }) = first else {
        panic!("an attach opened with {first:?}");
    };
    let session = SessionId(u128::from_str_radix(&row.session.id, 16).unwrap());
    assert_eq!(row.session.attachments.len(), 1);

    attach
        .input()
        .unwrap()
        .send(InputJson::KeyBytes {
            bytes: Base64(b"go\n".to_vec()),
        })
        .await
        .unwrap();
    until_screen_contains(&relay, session, "]11;rgb:1212/3434/5").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unread_attach_holds_back_neither_point_ops_nor_its_own_drop() {
    let tmp = private_dir();
    let relay = relay_on(&daemon(&tmp).await, Theme::default()).await;
    let flood = create(&relay, "while :; do echo flood-flood-flood-flood; done").await;
    let quiet = create(&relay, "printf 'quiet-marker\\n'; exec cat >/dev/null").await;

    let mut attach = relay
        .open(Open::Attach {
            target: AttachTarget::Session(flood),
        })
        .await
        .unwrap();
    let input = attach.input().unwrap();
    for _ in 0..256 {
        timeout(PATIENCE, input.send(InputJson::NextGridFrame))
            .await
            .expect("input waited on the unread output")
            .unwrap();
    }

    timeout(PATIENCE, async {
        relay.request(Request::List).await.unwrap();
        relay
            .request(Request::Info { session: quiet })
            .await
            .unwrap();
        relay
            .request(Request::Send {
                session: quiet,
                steps: vec![raw(b"x\n")],
            })
            .await
            .unwrap();
        until_screen_contains(&relay, quiet, "quiet-marker").await;
    })
    .await
    .expect("a point operation waited on the unread attach");

    let first = timeout(PATIENCE, attach.next()).await.unwrap();
    assert!(
        matches!(&first, StreamMessage::Item(Item::Attached { session, .. }) if session.session.id == session_id_hex(flood.0)),
        "{first:?}"
    );
    assert_eq!(attachments(&relay, flood).await, 1);

    drop(attach);
    until_detached(&relay, flood).await;
    assert!(matches!(
        input.send(InputJson::NextGridFrame).await,
        Err(InputError::Ended)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_of_the_window_ends_the_attach_naming_the_target() {
    let tmp = private_dir();
    let socket = daemon(&tmp).await;
    let relay = relay_on(&socket, Theme::default()).await;
    let from = create(&relay, "exec sleep 600").await;
    let to = create(&relay, "exec sleep 600").await;
    let mut attach = relay
        .open(Open::Attach {
            target: AttachTarget::Session(from),
        })
        .await
        .unwrap();
    assert!(matches!(
        timeout(PATIENCE, attach.next()).await.unwrap(),
        StreamMessage::Item(Item::Attached { .. })
    ));

    let mut ops =
        felis_client_core::connector::connect(&socket, felis_client_core::connector::Offer::ops())
            .await
            .unwrap();
    let switched = ops
        .switch_session(
            session_id_hex(from.0),
            session_id_hex(to.0),
            felis_protocol::messages::SwitchScope::Default,
        )
        .await
        .unwrap();
    assert_eq!(switched.queued, 1);

    let terminal = drain(&mut attach).await.pop().unwrap();
    assert!(
        matches!(&terminal, StreamMessage::Error(e) if e.kind == ErrorKind::Moved && e.session == Some(to)),
        "{terminal:?}"
    );
    until_detached(&relay, from).await;
}

/// The capture's rows, read only once the relay holds its terminal.
async fn capture_read_late(relay: &Relay, session: SessionId) -> Vec<StreamMessage> {
    let mut capture = relay
        .open(Open::Capture {
            session,
            source: RegionSourceJson::Scrollback,
            ansi: false,
            max_rows: None,
        })
        .await
        .unwrap();
    let deadline = Instant::now() + PATIENCE;
    while relay.open_streams() != 0 {
        assert!(Instant::now() < deadline, "the capture never ended");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drain(&mut capture).await
}

fn rows(messages: &[StreamMessage]) -> Vec<&str> {
    messages
        .iter()
        .filter_map(|message| match message {
            StreamMessage::Item(Item::Row(row)) => Some(row.text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scrollback_capture_read_late_delivers_every_row() {
    let tmp = private_dir();
    let relay = relay_on(&daemon(&tmp).await, Theme::default()).await;
    let session = create(
        &relay,
        "seq 1 5000; printf 'capture-ready\\n'; exec sleep 600",
    )
    .await;
    until_screen_contains(&relay, session, "capture-ready").await;

    let seen = capture_read_late(&relay, session).await;
    let numbered: Vec<u32> = rows(&seen)
        .into_iter()
        .filter_map(|text| text.parse().ok())
        .collect();
    assert_eq!(numbered, (1..=5000).collect::<Vec<_>>());
    assert!(
        matches!(seen.last(), Some(StreamMessage::End { .. })),
        "{:?}",
        seen.last()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_capture_read_late_past_the_byte_budget_ends_lagged() {
    let tmp = private_dir();
    let relay = relay_on(&daemon(&tmp).await, Theme::default()).await;
    let wide: SpawnArgsJson = serde_json::from_value(serde_json::json!({
        "command": "/bin/sh",
        "args": ["-c", "yes \"$(printf '%02048d' 0)\" | head -n 12000; \
                        printf 'flood-done\\n'; exec sleep 600"],
        "dims": { "rows": 24, "cols": 2048, "pixel_w": 0, "pixel_h": 0 },
    }))
    .unwrap();
    let OpResult::Create { session } = relay.request(Request::Create { args: wide }).await.unwrap()
    else {
        panic!("a create answered something else");
    };
    let session = SessionId(u128::from_str_radix(&session.session.id, 16).unwrap());
    until_screen_contains(&relay, session, "flood-done").await;

    let seen = capture_read_late(&relay, session).await;
    let held = rows(&seen).len();
    assert_eq!(seen.last(), Some(&StreamMessage::Lagged));
    assert!(
        DEFAULT_STREAM_BUFFER < held && held < 10_000,
        "{held} rows came before the lag"
    );
}

/// Forwards every connection to `target` until [`Self::cut`] drops them
/// all, which a peer reads as the daemon going away. [`Self::freeze`]
/// stops the bytes of every connection open so far and keeps them open,
/// a daemon that stopped answering on them.
struct Proxy {
    path: PathBuf,
    connections: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
    frozen: Arc<Notify>,
    accept: JoinHandle<()>,
}

impl Proxy {
    fn start(path: PathBuf, target: PathBuf) -> Self {
        let listener = UnixListener::bind(&path).unwrap();
        let connections = Arc::<std::sync::Mutex<Vec<_>>>::default();
        let frozen = Arc::new(Notify::new());
        let (accepted, freeze) = (Arc::clone(&connections), Arc::clone(&frozen));
        let accept = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let target = target.clone();
                let freeze = Arc::clone(&freeze);
                accepted.lock().unwrap().push(tokio::spawn(async move {
                    let mut daemon = UnixStream::connect(&target).await.unwrap();
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut daemon) => {}
                        () = freeze.notified() => std::future::pending().await,
                    }
                }));
            }
        });
        Self {
            path,
            connections,
            frozen,
            accept,
        }
    }

    fn freeze(&self) {
        self.frozen.notify_waiters();
    }

    fn cut(&self) {
        self.accept.abort();
        for task in self.connections.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_create_streams_while_the_shared_connection_hangs() {
    let tmp = private_dir();
    let socket = daemon(&tmp).await;
    let proxy = Proxy::start(tmp.path().join("proxy.sock"), socket);
    let relay = relay_on(&proxy.path, Theme::default()).await;
    proxy.freeze();

    let mut attach = timeout(
        PATIENCE,
        relay.open(Open::Attach {
            target: AttachTarget::Create(sh("printf 'while-frozen\\n'; exec sleep 600")),
        }),
    )
    .await
    .expect("the attach waited on the shared connection after the create landed")
    .unwrap();
    let session = attach.session().expect("an attach names its session");

    let first = timeout(PATIENCE, attach.next())
        .await
        .expect("the first item waited on the shared connection");
    let StreamMessage::Item(Item::Attached { session: row, .. }) = first else {
        panic!("an attach opened with {first:?}");
    };
    assert_eq!(
        row.session.short_id,
        session_id_hex(session.0),
        "with no answer on the shared connection the row names the full id"
    );
    let pull = attach.input().unwrap();
    let deadline = Instant::now() + PATIENCE;
    loop {
        pull.send(InputJson::NextGridFrame).await.unwrap();
        let message = timeout(PATIENCE, attach.next()).await.unwrap();
        if let StreamMessage::Item(item) = &message
            && row_texts(item)
                .iter()
                .any(|row| row.contains("while-frozen"))
        {
            break;
        }
        assert!(
            !message.is_terminal() && Instant::now() < deadline,
            "no grid row arrived: {message:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_daemon_ends_an_attach_after_what_it_already_sent() {
    let tmp = private_dir();
    let socket = daemon(&tmp).await;
    let proxy = Proxy::start(tmp.path().join("proxy.sock"), socket);
    let relay = relay_on(&proxy.path, Theme::default()).await;
    let session = create(&relay, "printf 'before-the-cut\\n'; exec sleep 600").await;
    until_screen_contains(&relay, session, "before-the-cut").await;

    let mut attach = relay
        .open(Open::Attach {
            target: AttachTarget::Session(session),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    proxy.cut();

    let seen = drain(&mut attach).await;
    assert!(matches!(
        seen.first(),
        Some(StreamMessage::Item(Item::Attached { .. }))
    ));
    assert!(
        seen.iter().any(|message| matches!(
            message,
            StreamMessage::Item(item) if row_texts(item).iter().any(|row| row.contains("before-the-cut"))
        )),
        "the rows sent before the loss were not delivered: {seen:?}"
    );
    let terminal = seen.last().unwrap().clone();
    assert!(
        matches!(&terminal, StreamMessage::Error(e) if e.kind == ErrorKind::DaemonLost),
        "{terminal:?}"
    );
    assert_eq!(attach.next().await, terminal);

    let after = relay.request(Request::List).await.unwrap_err();
    assert_eq!(after.kind, ErrorKind::DaemonLost);
    assert!(relay.closed().is_some());
}

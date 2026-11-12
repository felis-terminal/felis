use super::*;

/// Owns its runtime so `kill` takes every accepted connection down with
/// it; [`spawn_daemon`]'s `JoinHandle::abort` only stops the accept
/// loop.
struct KillableDaemon {
    socket: std::path::PathBuf,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KillableDaemon {
    async fn start(tmp: &TempDir, factory: SessionFactory) -> Self {
        let socket = tmp.path().join("daemon.sock");
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
        let serve_path = socket.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("daemon runtime");
            let serving = {
                let _entered = runtime.enter();
                let pool = Arc::new(Mutex::new(SessionPool::new()));
                bind_unix(&serve_path, DaemonCaps::default(), pool, factory)
                    .expect("the test daemon binds its socket")
            };
            runtime.spawn(async move { drop(serving.await) });
            bound_tx.send(()).unwrap_or_default();
            let _stopped = stop_rx.recv();
            runtime.shutdown_timeout(Duration::from_millis(100));
        });
        bound_rx.await.expect("the daemon thread binds");
        Self {
            socket,
            stop: Some(stop_tx),
            thread: Some(thread),
        }
    }

    fn kill(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _stopping = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

impl Drop for KillableDaemon {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The shell re-emits every second: an observer sees only events
/// published after it subscribed, so a one-shot emission would race the
/// CLI's connect + resolve + subscribe preamble.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_once_waits_for_a_live_notification() {
    let tmp = private_dir();
    let factory = sh_factory("while :; do sleep 1; printf '\\033]9;task done\\007'; done");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_on(&socket)
        .args([
            "notifications",
            "subscribe",
            "--session",
            &id_hex[..8],
            "--once",
            "--timeout",
            "30",
            "--format",
            "jsonl",
        ])
        .output()
        .expect("run felis notifications subscribe --once");
    assert!(
        out.status.success(),
        "subscribe --once must exit 0: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let objects = parse_jsonl(&stdout);
    assert_all_versioned(&objects, "notifications subscribe --once");
    assert_eq!(
        objects.len(),
        2,
        "--once must print one item and one terminal: {stdout}",
    );
    assert_eq!(objects[0]["session_id"], id_hex.as_str());
    assert!(
        objects[0].get("session_short_id").is_none(),
        "a notification is resolved against no roster: {:?}",
        objects[0]
    );
    assert!(
        stdout.contains("task done"),
        "the item must carry the notification body: {stdout}",
    );
    assert_eq!(objects[1]["event"], "end");
    assert_eq!(objects[1]["count"], 1);

    server.abort();
}

/// A `?`-propagated transport error would exit with items already on
/// stdout and no close after them (docs/reference/cli.md "Machine
/// output").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_verb_whose_daemon_dies_still_writes_its_terminal() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let tmp = private_dir();
    let factory = sh_factory("while :; do sleep 1; printf '\\033]9;still here\\007'; done");
    let mut daemon = KillableDaemon::start(&tmp, factory).await;
    let id = create_and_detach(&daemon.socket).await;
    let id_hex = format!("{id:032x}");

    // Read while it runs, so the daemon can be killed under a producing
    // stream.
    let mut child = tokio::process::Command::from(cli_on(&daemon.socket))
        .args([
            "notifications",
            "subscribe",
            "--session",
            &id_hex,
            "--format",
            "jsonl",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run felis notifications subscribe --format jsonl");
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();

    // Kill only after the first item, or this tests the failed-open
    // path.
    let first = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .expect("the first notification arrives within the timeout")
        .expect("read stdout")
        .expect("the stream produced an item");
    let item: serde_json::Value = serde_json::from_str(&first).expect("the item is one object");
    assert_eq!(item["v"], 1, "every object carries the epoch: {first}");
    assert_eq!(item["session_id"], id_hex.as_str());

    daemon.kill();

    let mut terminal = None;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("the stream closes within the timeout")
            .expect("read stdout");
        let Some(line) = line else { break };
        let object: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
        assert_eq!(object["v"], 1, "every object carries the epoch: {line}");
        // The in-band `lag` marker is not a terminal.
        if matches!(object["event"].as_str(), Some(event) if event != "lag") {
            assert!(terminal.is_none(), "a stream writes one terminal: {line}");
            terminal = Some(object);
        }
    }
    let terminal = terminal.expect("a killed daemon still owes the stream its terminal");
    assert_eq!(terminal["event"], "error", "{terminal}");
    assert_eq!(terminal["error"]["kind"], "daemon_lost", "{terminal}");

    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .expect("the verb exits within the timeout")
        .expect("wait for the verb");
    assert_eq!(
        status.code(),
        Some(2),
        "a stream the daemon abandoned is a protocol failure",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_once_times_out_on_a_silent_session() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = felis_out(
        &socket,
        &[
            "notifications",
            "subscribe",
            "--session",
            &id_hex,
            "--once",
            "--timeout",
            "1",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "timeout must exit 1: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        out.stdout.is_empty(),
        "no notification means no stdout: {}",
        String::from_utf8_lossy(&out.stdout),
    );

    server.abort();
}

/// `--timeout 0` is a deadline that has already passed: the subscription
/// waits for nothing, so the stream's terminal is an error object of kind
/// `timeout` and the verb exits 1, rather than reading `0` as unbounded
/// or as a usage error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_once_timeout_zero_gives_up_at_once() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = felis_out(
        &socket,
        &[
            "notifications",
            "subscribe",
            "--session",
            &id_hex,
            "--once",
            "--timeout",
            "0",
            "--format",
            "jsonl",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "--timeout 0 exits 1: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let objects: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}")))
        .collect();
    assert_all_versioned(&objects, "notifications subscribe --timeout 0");
    let terminal = objects.last().expect("a terminal object");
    assert_eq!(terminal["event"], "error", "{stdout}");
    assert_eq!(terminal["error"]["kind"], "timeout", "{stdout}");

    server.abort();
}

/// A daemon answering `Hello` with `Welcome`, the subscribe ack and one
/// notification in a single write, so `FrameReader` takes all three in
/// one `read_buf`. Answering after reading `Subscribe` leaves the event
/// in the kernel, where an unready socket times the wait out early.
async fn eager_notify_daemon(listener: felis_transport::Listener) {
    use felis_protocol::MessageKind;
    use felis_protocol::messages::{
        ConnToClientMsg, Correlation, Notification, NotifyToClientMsg, StreamId, Urgency,
    };
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::{
        CheckedFrame, FrameReader, FrameWriter,
        preface::{read_client_bootstrap, write_daemon_preface},
        server_split,
    };

    // Both ends number streams from 1, and this connection opens
    // exactly one, so the ack can be addressed before the request that
    // opens it arrives. The assertion below holds the daemon to it.
    let stream_id = StreamId::new(1).expect("1 is a stream id");

    let stream = listener.accept().await.expect("accept the CLI");
    let (mut read_half, mut write_half) = server_split(stream);
    let _bootstrap = read_client_bootstrap(&mut read_half)
        .await
        .expect("client preface");
    write_daemon_preface(
        &mut write_half,
        DaemonPreface::Accept {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        },
    )
    .await
    .expect("daemon preface");

    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::at_build_minor(write_half);

    // Read `Hello` first: it proves the CLI is past the sized preface
    // read and parked in `FrameReader`, so the one write below is the
    // next thing it sees.
    let hello = reader
        .next_frame()
        .await
        .expect("read Hello")
        .expect("the CLI sends Hello");
    assert_eq!(hello.kind, MessageKind::Conn.as_u16());

    let welcome =
        CheckedFrame::encode(&ConnToClientMsg::Welcome { identity: None }).expect("encode Welcome");
    let ack = CheckedFrame::encode_correlated(
        &NotifyToClientMsg::Subscribed { filter: None },
        Correlation::stream(stream_id),
    )
    .expect("encode the ack");
    let event = CheckedFrame::encode_correlated(
        &NotifyToClientMsg::Event {
            session_id: 0xF00D,
            notification: Notification {
                title: Some("done".into()),
                body: "build finished".into(),
                urgency: Urgency::Normal,
            },
            notify_id: None,
            session_title: None,
            cwd: None,
            attached: false,
        },
        Correlation::stream(stream_id),
    )
    .expect("encode the event");
    for frame in [&welcome, &ack, &event] {
        writer
            .send_checked_unflushed(frame)
            .await
            .expect("stage a frame");
    }
    writer.flush().await.expect("flush the preloaded frames");

    let subscribe = reader
        .next_frame()
        .await
        .expect("read Subscribe")
        .expect("the CLI subscribes");
    let opened = felis_protocol::codec::peek_correlation(&subscribe.body)
        .expect("the subscribe carries an envelope")
        .expect("a stream opener names its stream")
        .stream_id()
        .expect("the envelope names a stream, not a request");
    assert_eq!(
        opened, stream_id,
        "the preloaded ack must address the stream the CLI actually opened",
    );

    // Holding the socket open keeps a hang distinguishable from the
    // refusal the verb owes.
    tokio::time::sleep(Duration::from_secs(30)).await;
}

/// The deadline is checked before the wait, not merely raced against
/// it: with a notification already readable, `--timeout 0` still exits
/// 1 under `timeout` and prints no notification.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_timeout_zero_outranks_an_already_readable_notification() {
    let tmp = private_dir();
    let socket = tmp.path().join("eager.sock");
    let daemon = tokio::spawn(eager_notify_daemon(bind_listener(&socket)));

    let out = felis_out(
        &socket,
        &[
            "notifications",
            "subscribe",
            "--once",
            "--timeout",
            "0",
            "--format",
            "jsonl",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "--timeout 0 exits 1 even with a notification in hand: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let objects: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}")))
        .collect();
    assert_all_versioned(&objects, "notifications subscribe --timeout 0");
    assert_eq!(objects.len(), 1, "no notification is printed: {stdout}");
    assert_eq!(objects[0]["event"], "error", "{stdout}");
    assert_eq!(objects[0]["error"]["kind"], "timeout", "{stdout}");

    daemon.abort();
}

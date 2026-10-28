//! Session-addressed operations on an unattached `Ops` connection.

use felis_protocol::limits::{MAX_SEND_STEPS, PTY_INPUT_BUDGET};
use felis_protocol::messages::{
    InputMods, Key, KeyEvent, KeyEventKind, KeyLocation, MouseAction, MouseButton, MouseEvent,
    NamedKey, SearchOptions, SearchSource, SendOutcome, SendRefusal, SendStep,
};
use felis_transport::local::{ReadHalf, WriteHalf};

use super::*;

/// One unattached `Ops` connection, numbering its own requests and
/// streams as a client's driver does.
struct Peer {
    reader: FrameReader<ReadHalf>,
    writer: FrameWriter<WriteHalf>,
    requests: u64,
    streams: u64,
}

impl Peer {
    async fn ops(path: &Path) -> Self {
        let (read_half, write_half) = connect(path).await.unwrap();
        let (mut reader, mut writer) = framed(read_half, write_half).await;
        hello_welcome_as(&mut reader, &mut writer, ConnectionMode::Ops, false).await;
        Self {
            reader,
            writer,
            requests: 0,
            streams: 0,
        }
    }

    async fn request(&mut self, msg: &OpsToDaemonMsg) -> RequestId {
        self.requests += 1;
        send_request(&mut self.writer, msg, self.requests).await;
        RequestId::new(self.requests).unwrap()
    }

    async fn open<M>(&mut self, msg: &M) -> StreamId
    where
        M: codec::Correlated + felis_protocol::MinorGated + Directed + Clone + Sync,
    {
        self.streams += 1;
        send_stream(&mut self.writer, msg, self.streams).await;
        StreamId::new(self.streams).unwrap()
    }

    async fn frame_within(&mut self, secs: u64) -> Option<OwnedFrame> {
        tokio::time::timeout(Duration::from_secs(secs), self.reader.next_frame())
            .await
            .ok()
            .map(|read| read.unwrap().expect("the connection stays open"))
    }

    async fn frame(&mut self) -> OwnedFrame {
        self.frame_within(30).await.expect("a frame within 30s")
    }

    async fn ops_reply(&mut self) -> OpsToClientMsg {
        let frame = self.frame().await;
        assert_eq!(frame.kind, MessageKind::Ops.as_u16(), "an Ops frame");
        codec::decode(&frame.body).unwrap()
    }

    async fn sent(&mut self) -> SendOutcome {
        match self.ops_reply().await {
            OpsToClientMsg::Sent { outcome } => outcome,
            other => panic!("expected Sent, got {other:?}"),
        }
    }

    async fn spawn(&mut self) -> u128 {
        self.request(&OpsToDaemonMsg::Spawn {
            args: felis_protocol::messages::SpawnArgs::default(),
        })
        .await;
        match self.ops_reply().await {
            OpsToClientMsg::Spawned {
                outcome: SpawnOutcome::Ok { info },
            } => info.id,
            other => panic!("expected a successful Spawned, got {other:?}"),
        }
    }

    async fn send(&mut self, id: u128, steps: Vec<SendStep>) -> SendOutcome {
        self.request(&OpsToDaemonMsg::Send {
            id_prefix: hex(id),
            steps,
        })
        .await;
        self.sent().await
    }

    async fn cancel(&mut self, stream: StreamId) {
        send_kind(
            &mut self.writer,
            &ConnToDaemonMsg::Cancel { stream_id: stream },
        )
        .await;
    }

    /// Search `id`'s visible screen until `needle` shows: the program
    /// has printed, and the daemon parsed, everything before it.
    async fn await_visible(&mut self, id: u128, needle: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let stream = self
                .open(&SearchToDaemonMsg::Query {
                    query: needle.to_owned(),
                    options: SearchOptions::default(),
                    source: SearchSource::Visible,
                    session: Some(id),
                })
                .await;
            let (matches, _) = self.drain_stream(stream).await;
            if matches > 0 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{needle:?} never showed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Items until `stream`'s terminal: how many, and the terminal.
    async fn drain_stream(&mut self, stream: StreamId) -> (u32, ConnToClientMsg) {
        let mut items = 0;
        loop {
            let frame = self.frame().await;
            if frame.kind == MessageKind::Conn.as_u16() {
                let msg: ConnToClientMsg = codec::decode(&frame.body).unwrap();
                match &msg {
                    ConnToClientMsg::End { stream_id, .. }
                    | ConnToClientMsg::Error {
                        subject: Subject::Stream(stream_id),
                        ..
                    } if *stream_id == stream => return (items, msg),
                    other => panic!("unexpected control frame {other:?}"),
                }
            }
            items += 1;
        }
    }

    /// The next `SendAndWait` frame on this connection.
    async fn wait_frame(&mut self) -> WaitFrame {
        let frame = self.frame().await;
        if frame.kind == MessageKind::Conn.as_u16() {
            return WaitFrame::Terminal(codec::decode(&frame.body).unwrap());
        }
        match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
            OpsToClientMsg::WaitSent { outcome } => WaitFrame::Sent(outcome),
            OpsToClientMsg::Mark { exit_code } => WaitFrame::Mark(exit_code),
            other => panic!("unexpected wait item {other:?}"),
        }
    }
}

#[derive(Debug, PartialEq)]
enum WaitFrame {
    Sent(SendOutcome),
    Mark(Option<u32>),
    Terminal(ConnToClientMsg),
}

fn hex(id: u128) -> String {
    format!("{}", felis_protocol::SessionHex(id))
}

fn key(named: NamedKey) -> SendStep {
    SendStep::Key(KeyEvent {
        key: Key::Named(named),
        text: None,
        mods: felis_protocol::messages::KeyMods::empty(),
        kind: KeyEventKind::Press,
        location: KeyLocation::Standard,
    })
}

fn mouse(action: MouseAction) -> SendStep {
    SendStep::Mouse(MouseEvent {
        button: Some(MouseButton::Left),
        action,
        mods: InputMods::empty(),
        x: 1,
        y: 1,
        px: 0,
        py: 0,
    })
}

/// Prints `OSC 133 ; D ; <line>` for each line it reads.
const MARK_EACH_LINE: &str = r"stty -echo; while read l; do printf '\033]133;D;%s\007' $l; done";

async fn daemon(factory: SessionFactory) -> (TempDir, PathBuf, Arc<Mutex<SessionPool>>) {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, Arc::clone(&pool), factory).await;
    (tmp, path, pool)
}

async fn budget_in_use(pool: &Arc<Mutex<SessionPool>>, id: u128) -> usize {
    let handle = pool.lock().await.handle_cloned(SessionId(id)).unwrap();
    PTY_INPUT_BUDGET - handle.input_budget.available_permits()
}

async fn await_budget(pool: &Arc<Mutex<SessionPool>>, id: u128, at_least: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while budget_in_use(pool, id).await < at_least {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the budget never reached {at_least} bytes"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn await_file(path: &Path, done: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let bytes = std::fs::read(path).unwrap_or_default();
        if done(&bytes) {
            return bytes;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} never reached its expected content ({} bytes)",
            path.display(),
            bytes.len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A child that reads nothing until the test opens `gate`, then copies
/// its input to `received`.
fn gated_sink(gate: &Path, received: &Path) -> SessionFactory {
    script_factory(format!(
        "stty raw -echo; read _ < {}; exec cat > {}",
        gate.display(),
        received.display()
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_stalled_on_the_input_budget_leaves_other_connections_answering() {
    let dir = private_dir();
    let gate = gate(dir.path());
    let received = dir.path().join("received");
    let (_tmp, path, pool) = daemon(gated_sink(&gate, &received)).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    let mut sender = Peer::ops(&path).await;
    sender
        .request(&OpsToDaemonMsg::Send {
            id_prefix: hex(id),
            steps: vec![
                SendStep::Raw(vec![b'x'; PTY_INPUT_BUDGET]),
                SendStep::Raw(b"y".to_vec()),
            ],
        })
        .await;
    await_budget(&pool, id, PTY_INPUT_BUDGET).await;

    ops.request(&OpsToDaemonMsg::List).await;
    assert!(matches!(
        ops.ops_reply().await,
        OpsToClientMsg::Listed { .. }
    ));
    assert!(
        sender.frame_within(1).await.is_none(),
        "the stalled send answers before its last step was taken"
    );

    open_gate(&gate).await;
    assert_eq!(sender.sent().await, SendOutcome::Sent { id });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raw_step_the_size_of_the_whole_budget_is_followed_by_its_key() {
    let dir = private_dir();
    let received = dir.path().join("received");
    let factory = script_factory(format!("stty raw -echo; exec cat > {}", received.display()));
    let (_tmp, path, _pool) = daemon(factory).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    let outcome = ops
        .send(
            id,
            vec![
                SendStep::Raw(vec![b'x'; PTY_INPUT_BUDGET]),
                key(NamedKey::Tab),
            ],
        )
        .await;
    assert_eq!(outcome, SendOutcome::Sent { id });
    let bytes = await_file(&received, |b| b.len() > PTY_INPUT_BUDGET).await;
    assert_eq!(bytes.len(), PTY_INPUT_BUDGET + 1);
    assert_eq!(bytes.last(), Some(&b'\t'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_whose_last_mouse_event_is_unreported_writes_nothing() {
    let dir = private_dir();
    let received = dir.path().join("received");
    // `?1000` reports clicks but not drags.
    let factory = script_factory(format!(
        r"stty raw -echo; printf '\033[?1000hREADY'; exec cat > {}",
        received.display()
    ));
    let (_tmp, path, _pool) = daemon(factory).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;
    ops.await_visible(id, "READY").await;

    let outcome = ops
        .send(
            id,
            vec![
                SendStep::Paste(b"lost".to_vec()),
                mouse(MouseAction::Press),
                mouse(MouseAction::Drag),
            ],
        )
        .await;
    assert_eq!(
        outcome,
        SendOutcome::Refused {
            reason: SendRefusal::Unreported,
            step: Some(2),
            detail: "the program reports clicks but not drags (?1000 without ?1002); nothing \
                     was sent"
                .into(),
        }
    );
    // A later send lands alone: the refused one wrote no byte ahead of it.
    let outcome = ops.send(id, vec![SendStep::Raw(b"ok".to_vec())]).await;
    assert_eq!(outcome, SendOutcome::Sent { id });
    assert_eq!(await_file(&received, |b| !b.is_empty()).await, b"ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sent_carries_the_full_id_a_prefix_resolved_to() {
    let (_tmp, path, _pool) = daemon(shell_factory("read _x")).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    let full = hex(id);
    ops.request(&OpsToDaemonMsg::Send {
        id_prefix: full[..full.len() - 4].to_owned(),
        steps: vec![SendStep::Paste(b"x".to_vec())],
    })
    .await;
    assert_eq!(ops.sent().await, SendOutcome::Sent { id });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_that_ends_at_once_is_marked_right_after_wait_sent() {
    let (_tmp, path, _pool) = daemon(shell_factory(MARK_EACH_LINE)).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    let stream = ops
        .open(&OpsToDaemonMsg::SendAndWait {
            id_prefix: hex(id),
            steps: vec![SendStep::Paste(b"7".to_vec()), key(NamedKey::Enter)],
        })
        .await;
    assert_eq!(
        ops.wait_frame().await,
        WaitFrame::Sent(SendOutcome::Sent { id })
    );
    assert_eq!(ops.wait_frame().await, WaitFrame::Mark(Some(7)));
    assert_eq!(
        ops.wait_frame().await,
        WaitFrame::Terminal(ConnToClientMsg::End {
            stream_id: stream,
            count: 1,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mark_that_lands_during_a_stalled_admission_follows_wait_sent() {
    let dir = private_dir();
    let gate = gate(dir.path());
    let factory = script_factory(format!(
        r"stty raw -echo; read _ < {}; printf '\033]133;D;5\007'; exec cat > /dev/null",
        gate.display()
    ));
    let (_tmp, path, pool) = daemon(factory).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    ops.open(&OpsToDaemonMsg::SendAndWait {
        id_prefix: hex(id),
        steps: vec![
            SendStep::Raw(vec![b'x'; PTY_INPUT_BUDGET]),
            SendStep::Raw(b"y".to_vec()),
        ],
    })
    .await;
    await_budget(&pool, id, PTY_INPUT_BUDGET).await;
    open_gate(&gate).await;

    assert_eq!(
        ops.wait_frame().await,
        WaitFrame::Sent(SendOutcome::Sent { id })
    );
    assert_eq!(ops.wait_frame().await, WaitFrame::Mark(Some(5)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canceling_a_wait_after_wait_sent_ends_its_stream() {
    let (_tmp, path, _pool) = daemon(shell_factory("read _x; read _y")).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    let stream = ops
        .open(&OpsToDaemonMsg::SendAndWait {
            id_prefix: hex(id),
            steps: vec![SendStep::Paste(b"x".to_vec())],
        })
        .await;
    assert_eq!(
        ops.wait_frame().await,
        WaitFrame::Sent(SendOutcome::Sent { id })
    );
    ops.cancel(stream).await;
    assert_eq!(
        ops.wait_frame().await,
        WaitFrame::Terminal(ConnToClientMsg::End {
            stream_id: stream,
            count: 0,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_on_an_exited_session_ends_unavailable_at_once() {
    let (_tmp, path, _pool) = daemon(shell_factory("exit 0")).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        ops.request(&OpsToDaemonMsg::Info { id_prefix: hex(id) })
            .await;
        if let OpsToClientMsg::InfoReply {
            outcome: InfoOutcome::Found { session, .. },
        } = ops.ops_reply().await
            && session.exited
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never exited");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let stream = ops
        .open(&OpsToDaemonMsg::SendAndWait {
            id_prefix: hex(id),
            steps: Vec::new(),
        })
        .await;
    match ops.wait_frame().await {
        WaitFrame::Terminal(ConnToClientMsg::Error {
            subject: Subject::Stream(ended),
            reason: StreamErrorReason::Unavailable,
            ..
        }) => assert_eq!(ended, stream),
        other => panic!("expected the stream to end Unavailable, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watch_only_wait_reports_the_mark_another_connection_caused() {
    let (_tmp, path, _pool) = daemon(shell_factory(MARK_EACH_LINE)).await;
    let mut watcher = Peer::ops(&path).await;
    let id = watcher.spawn().await;

    watcher
        .open(&OpsToDaemonMsg::SendAndWait {
            id_prefix: hex(id),
            steps: Vec::new(),
        })
        .await;
    assert_eq!(
        watcher.wait_frame().await,
        WaitFrame::Sent(SendOutcome::Sent { id })
    );
    let mut typist = Peer::ops(&path).await;
    let outcome = typist
        .send(
            id,
            vec![SendStep::Paste(b"4".to_vec()), key(NamedKey::Enter)],
        )
        .await;
    assert_eq!(outcome, SendOutcome::Sent { id });
    assert_eq!(watcher.wait_frame().await, WaitFrame::Mark(Some(4)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mark_that_arrives_while_the_connection_is_idle_is_delivered() {
    let dir = private_dir();
    let gate = gate(dir.path());
    let factory = script_factory(format!(
        r"read _ < {}; printf '\033]133;D;9\007'; read _x",
        gate.display()
    ));
    let (_tmp, path, _pool) = daemon(factory).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    ops.open(&OpsToDaemonMsg::SendAndWait {
        id_prefix: hex(id),
        steps: Vec::new(),
    })
    .await;
    assert_eq!(
        ops.wait_frame().await,
        WaitFrame::Sent(SendOutcome::Sent { id })
    );
    assert!(ops.frame_within(1).await.is_none(), "nothing to report yet");
    open_gate(&gate).await;
    assert_eq!(ops.wait_frame().await, WaitFrame::Mark(Some(9)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_stops_a_running_unattached_search() {
    let factory = shell_factory(
        "i=0; while [ $i -lt 20000 ]; do echo line$i; i=$((i+1)); done; echo READY; read _x",
    );
    let (_tmp, path, _pool) = daemon(factory).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;
    ops.await_visible(id, "READY").await;

    let stream = ops
        .open(&SearchToDaemonMsg::Query {
            query: "line".into(),
            options: SearchOptions::default(),
            source: SearchSource::Scrollback,
            session: Some(id),
        })
        .await;
    let first = ops.frame().await;
    assert_eq!(first.kind, MessageKind::Search.as_u16(), "a first match");
    ops.cancel(stream).await;
    let (items, terminal) = ops.drain_stream(stream).await;
    match terminal {
        ConnToClientMsg::End { count, .. } => {
            assert_eq!(count, items + 1, "the terminal counts what was sent");
            assert!(count < 20_000, "the walk stopped short: {count}");
        }
        other => panic!("expected End, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_past_the_step_cap_is_refused_before_its_steps_are_decoded() {
    use felis_protocol::frame::Frame;

    let (_tmp, path, _pool) = daemon(shell_factory("read _x")).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    // Every step is an empty message, which a decode refuses: the
    // typed answer proves the cap was read off the tags first.
    let mut body = codec::encode_correlated(
        &OpsToDaemonMsg::Send {
            id_prefix: hex(id),
            steps: Vec::new(),
        },
        Correlation::request(RequestId::new(2).unwrap()),
    );
    let steps = [0x12, 0x00].repeat(MAX_SEND_STEPS + 1);
    let mut arm = vec![0xAA, 0x01];
    let mut len = steps.len();
    while len >= 0x80 {
        arm.push(u8::try_from(len & 0x7F).unwrap() | 0x80);
        len >>= 7;
    }
    arm.push(u8::try_from(len).unwrap());
    arm.extend_from_slice(&steps);
    body.extend_from_slice(&arm);
    ops.writer
        .write_frame_unchecked(&Frame {
            kind: MessageKind::Ops.as_u16(),
            body: &body,
        })
        .await
        .unwrap();
    match ops.sent().await {
        SendOutcome::Refused {
            reason: SendRefusal::TooLarge,
            step: None,
            ..
        } => {}
        other => panic!("expected a TooLarge refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_ends_unavailable_when_the_session_exits_with_no_mark() {
    let dir = private_dir();
    let gate = gate(dir.path());
    let factory = script_factory(format!("read _ < {}; exit 0", gate.display()));
    let (_tmp, path, _pool) = daemon(factory).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    let watch_only = ops
        .open(&OpsToDaemonMsg::SendAndWait {
            id_prefix: hex(id),
            steps: Vec::new(),
        })
        .await;
    assert_eq!(
        ops.wait_frame().await,
        WaitFrame::Sent(SendOutcome::Sent { id })
    );
    let after_send = ops
        .open(&OpsToDaemonMsg::SendAndWait {
            id_prefix: hex(id),
            steps: vec![SendStep::Paste(b"x".to_vec())],
        })
        .await;
    assert_eq!(
        ops.wait_frame().await,
        WaitFrame::Sent(SendOutcome::Sent { id })
    );
    open_gate(&gate).await;
    let mut ended = Vec::new();
    for _ in 0..2 {
        match ops.wait_frame().await {
            WaitFrame::Terminal(ConnToClientMsg::Error {
                subject: Subject::Stream(stream),
                reason: StreamErrorReason::Unavailable,
                ..
            }) => ended.push(stream),
            other => panic!("expected a stream to end Unavailable, got {other:?}"),
        }
    }
    ended.sort_by_key(|s| s.get());
    assert_eq!(ended, [watch_only, after_send]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_whose_connection_closes_mid_admission_writes_no_later_step() {
    let dir = private_dir();
    let gate = gate(dir.path());
    let received = dir.path().join("received");
    let (_tmp, path, pool) = daemon(gated_sink(&gate, &received)).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    let mut sender = Peer::ops(&path).await;
    sender
        .request(&OpsToDaemonMsg::Send {
            id_prefix: hex(id),
            steps: vec![
                SendStep::Raw(vec![b'x'; PTY_INPUT_BUDGET]),
                SendStep::Raw(b"MARK".to_vec()),
            ],
        })
        .await;
    await_budget(&pool, id, PTY_INPUT_BUDGET).await;
    drop(sender);
    // Long enough for the daemon to notice the hangup before the budget
    // frees: a step admitted after it would be a write nobody asked for.
    tokio::time::sleep(Duration::from_millis(200)).await;
    open_gate(&gate).await;

    await_file(&received, |b| b.len() >= PTY_INPUT_BUDGET).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while budget_in_use(&pool, id).await > 0 {
        assert!(tokio::time::Instant::now() < deadline, "budget never freed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A probe behind the abandoned step: had `MARK` been admitted, it
    // would precede this.
    assert_eq!(
        ops.send(id, vec![SendStep::Raw(b"END".to_vec())]).await,
        SendOutcome::Sent { id }
    );
    let bytes = await_file(&received, |b| b.ends_with(b"END")).await;
    assert_eq!(&bytes[PTY_INPUT_BUDGET..], b"END");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_connections_sending_to_one_stalled_session_share_its_budget() {
    const HALF_AND_MORE: usize = PTY_INPUT_BUDGET / 2 + 1;

    let dir = private_dir();
    let gate = gate(dir.path());
    let received = dir.path().join("received");
    let (_tmp, path, pool) = daemon(gated_sink(&gate, &received)).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    let mut first = Peer::ops(&path).await;
    let mut second = Peer::ops(&path).await;
    for peer in [&mut first, &mut second] {
        peer.request(&OpsToDaemonMsg::Send {
            id_prefix: hex(id),
            steps: vec![SendStep::Raw(vec![b'x'; HALF_AND_MORE])],
        })
        .await;
    }
    // The second step waits on the permits the first left, so the one
    // budget drains past what either connection alone was admitted.
    await_budget(&pool, id, HALF_AND_MORE + 1).await;
    assert!(budget_in_use(&pool, id).await <= PTY_INPUT_BUDGET);
    assert!(
        second.frame_within(1).await.is_none(),
        "the step that does not fit waits"
    );

    open_gate(&gate).await;
    assert_eq!(first.sent().await, SendOutcome::Sent { id });
    assert_eq!(second.sent().await, SendOutcome::Sent { id });
    await_file(&received, |b| b.len() == 2 * HALF_AND_MORE).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_capture_names_its_session_and_reads_the_live_screen() {
    let (_tmp, path, _pool) = daemon(shell_factory("echo hello-capture; read _x")).await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;
    ops.await_visible(id, "hello-capture").await;

    let stream = ops
        .open(&RegionToDaemonMsg::Rows {
            source: felis_protocol::messages::RegionSource::Visible,
            ansi: false,
            max_rows: None,
            session: Some(id),
        })
        .await;
    let mut rows = Vec::new();
    loop {
        let frame = ops.frame().await;
        if frame.kind == MessageKind::Conn.as_u16() {
            assert!(matches!(
                codec::decode::<ConnToClientMsg>(&frame.body).unwrap(),
                ConnToClientMsg::End { stream_id, .. } if stream_id == stream
            ));
            break;
        }
        if let RegionToClientMsg::Row { text, .. } = codec::decode(&frame.body).unwrap() {
            rows.push(text);
        }
    }
    assert!(rows.iter().any(|r| r.contains("hello-capture")), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_addressed_search_on_an_attached_connection_is_refused() {
    let (_tmp, path, _pool) = daemon(shell_factory("read _x")).await;
    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;
    hello_welcome(&mut reader, &mut writer, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    send_stream(
        &mut writer,
        &SearchToDaemonMsg::Query {
            query: "x".into(),
            options: SearchOptions::default(),
            source: SearchSource::Scrollback,
            session: Some(id),
        },
        1,
    )
    .await;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), reader.next_frame())
            .await
            .expect("a terminal")
            .unwrap()
            .expect("frame");
        if frame.kind != MessageKind::Conn.as_u16() {
            continue;
        }
        match codec::decode::<ConnToClientMsg>(&frame.body).unwrap() {
            ConnToClientMsg::Error {
                reason: StreamErrorReason::InvalidRequest,
                ..
            } => break,
            other => panic!("expected an InvalidRequest terminal, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_on_an_unattached_connection_is_answered_before_the_daemon_exits() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let serving = spawn_daemon_watching(
        &path,
        pool,
        shell_factory("sleep 30"),
        DaemonCaps::default(),
    )
    .await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;
    assert_eq!(
        ops.send(id, vec![SendStep::Raw(b"x".to_vec())]).await,
        SendOutcome::Sent { id }
    );

    ops.request(&OpsToDaemonMsg::Stop {
        mode: StopMode::Force,
    })
    .await;
    assert_eq!(
        ops.ops_reply().await,
        OpsToClientMsg::StopReply {
            outcome: StopOutcome::Stopping
        }
    );
    tokio::time::timeout(Duration::from_secs(20), serving)
        .await
        .expect("the daemon exits after answering the stop")
        .expect("the serve task did not panic")
        .expect("the accept loop ends cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_ends_when_the_program_exits_while_its_background_job_holds_the_pty() {
    let (_tmp, path, _pool) = daemon(shell_factory(
        "trap '' HUP; sleep 20 & trap - HUP; read _x; exit 0",
    ))
    .await;
    let mut ops = Peer::ops(&path).await;
    let id = ops.spawn().await;

    let stream = ops
        .open(&OpsToDaemonMsg::SendAndWait {
            id_prefix: hex(id),
            steps: vec![SendStep::Paste(b"x".to_vec()), key(NamedKey::Enter)],
        })
        .await;
    assert_eq!(
        ops.wait_frame().await,
        WaitFrame::Sent(SendOutcome::Sent { id })
    );
    let frame = ops
        .frame_within(3)
        .await
        .expect("the wait ends at the exit, not at the post-exit reap");
    match codec::decode::<ConnToClientMsg>(&frame.body).unwrap() {
        ConnToClientMsg::Error {
            subject: Subject::Stream(ended),
            reason: StreamErrorReason::Unavailable,
            ..
        } if ended == stream => {}
        other => panic!("expected the wait to end Unavailable, got {other:?}"),
    }
}

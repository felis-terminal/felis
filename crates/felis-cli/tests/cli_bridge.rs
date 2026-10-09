//! `felis bridge` integration tests: the JSONL stdio surface.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use felis_daemon::{
    SessionPool,
    serve::{DaemonCaps, SessionFactory, serve_unix_with_factory},
};
use felis_protocol::{
    MessageKind,
    messages::{ConnToClientMsg, PushMsg, SpawnArgs},
};
use felis_pty::Command as PtyCommand;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::Mutex;

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/schema.rs"]
mod schema;

use fixtures::{create_and_detach, private_dir, quiet_factory};

const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Owns its runtime so [`Daemon::kill`] takes every accepted connection
/// down with it; aborting the accept loop alone would leave them alive.
struct Daemon {
    socket: std::path::PathBuf,
    /// Locking this parks the daemon mid-verb, the only way to keep a
    /// request outstanding for as long as a test needs.
    pool: Arc<Mutex<SessionPool>>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Daemon {
    async fn start(tmp: &TempDir, factory: SessionFactory) -> Self {
        Self::start_with_caps(tmp, factory, DaemonCaps::default()).await
    }

    async fn start_with_caps(tmp: &TempDir, factory: SessionFactory, caps: DaemonCaps) -> Self {
        let socket = tmp.path().join("daemon.sock");
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let serve_path = socket.clone();
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let served = Arc::clone(&pool);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("daemon runtime");
            runtime.spawn(async move {
                drop(serve_unix_with_factory(&serve_path, caps, served, factory).await);
            });
            let _stopped = stop_rx.recv();
            runtime.shutdown_timeout(Duration::from_millis(100));
        });
        // Readiness is an accepted connection, not an existing path:
        // `bind` publishes the inode before `listen`, so a path probe can
        // hand the next line ECONNREFUSED.
        for _ in 0..400 {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                return Self {
                    socket,
                    pool,
                    stop: Some(stop_tx),
                    thread: Some(thread),
                };
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("daemon on {} never accepted a connection", socket.display());
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

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

fn banner_factory() -> SessionFactory {
    Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf 'BRIDGE-LINE-ONE\\r\\nBRIDGE-LINE-TWO\\r\\n'; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    })
}

fn cli_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("CARGO_BIN_EXE_felis").expect("cargo sets the bin path"))
}

struct Bridge {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    /// Taken by [`Bridge::close_stdout`]: dropping the read end is the
    /// only way to fail a child process's stdout from outside.
    stdout: Option<tokio::io::Lines<BufReader<tokio::process::ChildStdout>>>,
    stderr: Option<tokio::task::JoinHandle<String>>,
    seen: Vec<Value>,
}

impl Bridge {
    fn start(socket: &std::path::Path) -> Self {
        Self::start_in(socket, None)
    }

    fn start_in(socket: &std::path::Path, cwd: Option<&std::path::Path>) -> Self {
        let mut command = tokio::process::Command::new(cli_bin());
        command
            .arg("--socket")
            .arg(socket)
            .arg("bridge")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().expect("spawn felis bridge");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        let mut err = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
        let stderr = tokio::spawn(async move {
            let mut collected = String::new();
            while let Ok(Some(line)) = err.next_line().await {
                collected.push_str(&line);
                collected.push('\n');
            }
            collected
        });
        Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
            seen: Vec::new(),
        }
    }

    async fn send(&mut self, request: &Value) {
        let mut line = request.to_string();
        line.push('\n');
        self.stdin
            .as_mut()
            .expect("stdin is open")
            .write_all(line.as_bytes())
            .await
            .expect("write a request");
    }

    async fn send_raw(&mut self, line: &str) {
        self.stdin
            .as_mut()
            .expect("stdin is open")
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write a raw line");
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(
            REPLY_TIMEOUT,
            self.stdout.as_mut().expect("stdout is open").next_line(),
        )
        .await
        .expect("the bridge answered within the timeout")
        .expect("stdout read")
        .expect("the bridge closed stdout before answering");
        let object: Value =
            serde_json::from_str(&line).unwrap_or_else(|err| panic!("`{line}` is not JSON: {err}"));
        schema::assert_bridge_object(&object);
        self.seen.push(object.clone());
        object
    }

    /// Arrival order is not assumed: concurrent requests may answer out
    /// of order.
    async fn collect_ids(&mut self, ids: &[&str]) -> std::collections::HashMap<String, Value> {
        let mut found = std::collections::HashMap::new();
        while found.len() < ids.len() {
            let object = self.next().await;
            let id = object["id"].as_str().unwrap_or_default().to_owned();
            if ids.contains(&id.as_str()) {
                found.insert(id, object);
            }
        }
        found
    }

    async fn collect_for(&mut self, id: &Value, want: usize) -> Vec<Value> {
        let mut mine = Vec::new();
        while mine.len() < want {
            let object = self.next().await;
            if object.get("id") == Some(id) {
                mine.push(object);
            }
        }
        mine
    }

    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    fn close_stdout(&mut self) {
        drop(self.stdout.take());
    }

    async fn wait(&mut self) -> (i32, Vec<Value>, String) {
        let mut transcript = std::mem::take(&mut self.seen);
        while let Some(stdout) = self.stdout.as_mut() {
            match tokio::time::timeout(REPLY_TIMEOUT, stdout.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let object: Value = serde_json::from_str(&line)
                        .unwrap_or_else(|err| panic!("`{line}` is not JSON: {err}"));
                    schema::assert_bridge_object(&object);
                    transcript.push(object);
                }
                _ => break,
            }
        }
        let status = tokio::time::timeout(REPLY_TIMEOUT, self.child.wait())
            .await
            .expect("the bridge exited within the timeout")
            .expect("wait for the bridge");
        let stderr = self.stderr.take().expect("stderr task").await.unwrap();
        (status.code().unwrap_or(-1), transcript, stderr)
    }

    async fn finish(mut self) -> (i32, Vec<Value>, String) {
        self.close_stdin();
        self.wait().await
    }

    /// stdin stays open, so an exit here is the bridge's own decision
    /// rather than the clean stdin-EOF shutdown.
    async fn exit_without_stdin_eof(mut self) -> (i32, Vec<Value>, String) {
        self.wait().await
    }
}

fn request(id: &str, op: &str, params: &Value) -> Value {
    json!({"v": 1, "id": id, "op": op, "params": params})
}

/// `v` moves with the CLI output contract, never with the daemon wire.
fn assert_surface_version(object: &Value) {
    assert_eq!(
        object.get("v"),
        Some(&json!(1)),
        "every object carries the surface version: {object}"
    );
}

fn assert_stderr_is_human_only(stderr: &str) {
    for line in stderr.lines() {
        if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(line) {
            assert!(
                !object.contains_key("v"),
                "stderr carried a protocol object: {line}",
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies_echo_the_id_of_the_request_they_answer() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let id = create_and_detach(&daemon.socket).await;
    let hex = format!("{id:032x}");

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request("list-1", "sessions.list", &json!({})))
        .await;
    bridge
        .send(&request(
            "info-2",
            "sessions.info",
            &json!({"session": hex}),
        ))
        .await;

    let replies = bridge.collect_ids(&["list-1", "info-2"]).await;

    let listed = &replies["list-1"];
    assert_surface_version(listed);
    assert_eq!(
        listed["result"]["sessions"][0]["id"],
        json!(hex),
        "the list result carries the `sessions list` session object: {listed}"
    );

    let info = &replies["info-2"];
    assert_surface_version(info);
    assert_eq!(info["result"]["id"], json!(hex));

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stdin EOF is a clean exit; stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

/// Bridge ops run on their own tasks over one daemon connection, so
/// several verbs allocate request ids concurrently. A request id is
/// positional on the wire: an id that overtakes the frame carrying it
/// ends the connection, failing every op on the link and not just its
/// own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_verbs_reach_the_daemon_in_request_id_order() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);

    // Parked, every verb is outstanding at once instead of one at a
    // time, which is what makes the allocations race.
    let parked = Arc::clone(&daemon.pool).lock_owned().await;
    let ids: Vec<String> = (0..8).map(|n| format!("list-{n}")).collect();
    for id in &ids {
        bridge.send(&request(id, "sessions.list", &json!({}))).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    drop(parked);

    let names: Vec<&str> = ids.iter().map(String::as_str).collect();
    let replies = bridge.collect_ids(&names).await;
    for name in &names {
        let reply = &replies[*name];
        assert_surface_version(reply);
        assert!(
            reply["result"]["sessions"].is_array(),
            "{name} answered {reply}"
        );
    }

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stdin EOF is a clean exit; stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_refuses_operations_beyond_its_in_flight_limit() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let parked = Arc::clone(&daemon.pool).lock_owned().await;
    let mut bridge = Bridge::start(&daemon.socket);

    for index in 0..=64 {
        bridge
            .send(&request(
                &format!("op-{index}"),
                "sessions.list",
                &json!({}),
            ))
            .await;
    }
    bridge
        .send(&request(
            "stream-over",
            "notifications.subscribe",
            &json!({}),
        ))
        .await;
    bridge
        .send(&request(
            "bad-over",
            "sessions.list",
            &json!({"unknown": true}),
        ))
        .await;
    bridge
        .send(&request("missing-over", "sessions.capture", &json!({})))
        .await;
    bridge
        .send(&request(
            "typed-over",
            "sessions.list",
            &json!({"tags": true}),
        ))
        .await;
    bridge
        .send(&request(
            "large-over",
            "sessions.search",
            &json!({
                "session": "deadbeef",
                "pattern": "x".repeat(felis_protocol::messages::MAX_SEARCH_PATTERN_BYTES + 1)
            }),
        ))
        .await;
    bridge
        .send(&request(
            "tag-empty-over",
            "sessions.tag",
            &json!({"session": "deadbeef"}),
        ))
        .await;
    let refused = bridge
        .collect_ids(&[
            "op-64",
            "stream-over",
            "bad-over",
            "missing-over",
            "typed-over",
            "large-over",
            "tag-empty-over",
        ])
        .await;
    assert_eq!(refused["op-64"]["error"]["kind"], json!("at_capacity"));
    assert_eq!(
        refused["stream-over"]["event"],
        json!("error"),
        "a stream refusal is terminal: {}",
        refused["stream-over"]
    );
    assert_eq!(
        refused["stream-over"]["error"]["kind"],
        json!("at_capacity")
    );
    assert_eq!(
        refused["bad-over"]["error"]["kind"],
        json!("malformed_request"),
        "validation precedes admission: {}",
        refused["bad-over"]
    );
    assert_eq!(
        refused["missing-over"]["error"]["kind"],
        json!("malformed_request")
    );
    assert_eq!(refused["missing-over"]["event"], json!("error"));
    assert_eq!(
        refused["typed-over"]["error"]["kind"],
        json!("malformed_request")
    );
    assert_eq!(
        refused["large-over"]["error"]["kind"],
        json!("invalid_request")
    );
    assert_eq!(refused["large-over"]["event"], json!("error"));
    assert_eq!(
        refused["tag-empty-over"]["error"]["kind"],
        json!("malformed_request")
    );

    drop(parked);
    let (code, transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        transcript.len(),
        71,
        "every admitted operation and refusal is answered"
    );
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_and_stdin_eof_remain_live_at_the_operation_limit() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let parked = Arc::clone(&daemon.pool).lock_owned().await;
    let mut bridge = Bridge::start(&daemon.socket);

    for index in 0..64 {
        bridge
            .send(&request(
                &format!("op-{index}"),
                "sessions.list",
                &json!({}),
            ))
            .await;
    }
    bridge
        .send(&request("stop", "cancel", &json!({"target": "op-0"})))
        .await;
    let canceled = bridge.next().await;
    assert_eq!(canceled["id"], json!("stop"), "{canceled}");
    assert_eq!(canceled["result"]["canceled"], json!(true), "{canceled}");

    let (code, transcript, stderr) = bridge.finish().await;
    assert_eq!(
        code, 0,
        "EOF remains observable at the cap; stderr: {stderr}"
    );
    assert_eq!(
        transcript
            .iter()
            .filter(|object| object["id"] != json!("stop"))
            .count(),
        64,
        "shutdown answers every admitted operation"
    );
    drop(parked);
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_refuses_auxiliary_links_beyond_their_limit() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);

    for index in 0..=32 {
        bridge
            .send(&request(
                &format!("watch-{index}"),
                "notifications.subscribe",
                &json!({}),
            ))
            .await;
    }
    let refused = bridge.next().await;
    assert_eq!(refused["error"]["kind"], json!("at_capacity"), "{refused}");
    let refused_id = refused["id"].clone();

    let (code, transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        transcript
            .iter()
            .filter(|object| {
                object.get("event").is_some() && object.get("id") != Some(&refused_id)
            })
            .count(),
        32,
        "every admitted subscription receives one terminal"
    );
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdout_failure_stops_the_bridge_without_waiting_for_stdin_eof() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);
    bridge.close_stdout();

    bridge
        .send(&request("watch", "notifications.subscribe", &json!({})))
        .await;
    bridge
        .send(&request("reply", "sessions.list", &json!({})))
        .await;

    let (code, _transcript, stderr) = bridge.exit_without_stdin_eof().await;
    assert_eq!(code, 1, "stdout failure is unsuccessful; stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

/// The fifth `felis bridge` scenario: a stdout loss under an operation
/// the daemon has not answered yet. The channel that would carry a
/// correlated error is the one that failed, so the operation gets no
/// terminal at all and the process exits `1`
/// (`docs/reference/cli.md`, "felis bridge").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stdout_loss_under_an_in_flight_operation_exits_one_with_no_terminal() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);

    // A served reply first: without it the exit below could be a bridge
    // that never reached the daemon rather than the closed reader.
    bridge
        .send(&request("served", "sessions.list", &json!({})))
        .await;
    let served = bridge.next().await;
    assert_surface_version(&served);
    assert_eq!(served["id"], json!("served"));

    let parked = Arc::clone(&daemon.pool).lock_owned().await;
    bridge
        .send(&request("in-flight", "sessions.list", &json!({})))
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    bridge.close_stdout();
    drop(parked);

    let (code, transcript, stderr) = bridge.exit_without_stdin_eof().await;
    assert_eq!(
        code, 1,
        "a stdout loss exits 1 without waiting for stdin EOF; stderr: {stderr}"
    );
    assert_eq!(
        transcript.len(),
        1,
        "the served reply is all stdout ever carried: {transcript:?}"
    );
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_streams_stay_correlated_and_each_ends_once() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, banner_factory()).await;
    let id = create_and_detach(&daemon.socket).await;
    let hex = format!("{id:032x}");
    // Let the parked drain advance the banner into the grid.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request(
            "cap",
            "sessions.capture",
            &json!({"session": hex}),
        ))
        .await;
    bridge
        .send(&request(
            "search",
            "sessions.search",
            &json!({"session": hex, "pattern": "BRIDGE-LINE"}),
        ))
        .await;

    let mut terminals: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut capture_rows = Vec::new();
    let mut search_hits = 0_usize;
    while terminals.len() < 2 {
        let object = bridge.next().await;
        assert_surface_version(&object);
        let id = object["id"]
            .as_str()
            .expect("every object echoes its id")
            .to_owned();
        assert!(
            id == "cap" || id == "search",
            "an object belongs to a request that was made: {object}"
        );
        match object.get("event").and_then(Value::as_str) {
            Some("end" | "error") => {
                *terminals.entry(id).or_insert(0) += 1;
            }
            Some(_) => {}
            None => {
                if id == "cap" {
                    capture_rows.push(object);
                } else {
                    search_hits += 1;
                }
            }
        }
    }

    assert_eq!(
        terminals.get("cap"),
        Some(&1),
        "the capture ended exactly once"
    );
    assert_eq!(
        terminals.get("search"),
        Some(&1),
        "the search ended exactly once"
    );
    assert!(
        capture_rows.iter().any(|row| row["item"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("BRIDGE-LINE-ONE"))),
        "the capture stream carried the banner: {capture_rows:?}"
    );
    assert!(search_hits > 0, "the search stream carried its matches");

    let (code, transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    for id in ["cap", "search"] {
        let positions: Vec<_> = transcript
            .iter()
            .enumerate()
            .filter(|(_, object)| object["id"] == json!(id))
            .collect();
        assert_eq!(
            positions
                .iter()
                .filter(|(_, object)| object.get("event").is_some())
                .count(),
            1,
            "`{id}` ends exactly once: {transcript:?}"
        );
        assert!(
            positions
                .last()
                .is_some_and(|(_, object)| object.get("event").is_some()),
            "`{id}` has no item after its terminal: {transcript:?}"
        );
    }
    assert_stderr_is_human_only(&stderr);
}

/// Same fields as `capture --format jsonl`, except `ansi` is always
/// present (null when unasked) where the CLI omits the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_rows_ride_the_structural_json_row_shape() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, banner_factory()).await;
    let id = create_and_detach(&daemon.socket).await;
    let hex = format!("{id:032x}");
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request(
            "cap",
            "sessions.capture",
            &json!({"session": hex, "ansi": true}),
        ))
        .await;

    let mut banner = None;
    loop {
        let object = bridge.next().await;
        if object.get("event").is_some() {
            assert_eq!(
                object["event"],
                json!("end"),
                "the capture must end cleanly: {object}"
            );
            break;
        }
        let item = &object["item"];
        assert!(
            item.get("Row").is_none(),
            "the frame's variant tag is not part of the item: {object}"
        );
        assert!(
            item["row"].is_i64(),
            "every row carries its index: {object}"
        );
        assert!(
            item["text"].is_string(),
            "every row carries its text: {object}"
        );
        assert!(
            item["soft_wrap_continued"].is_boolean(),
            "every row carries its soft-wrap bit: {object}"
        );
        if item["text"]
            .as_str()
            .is_some_and(|t| t.contains("BRIDGE-LINE-ONE"))
        {
            banner = Some(item.clone());
        }
    }
    let banner = banner.expect("the banner row is in the capture");
    assert!(
        banner["ansi"].is_string(),
        "`ansi: true` fills the row's SGR reconstruction: {banner}"
    );

    bridge
        .send(&request(
            "plain",
            "sessions.capture",
            &json!({"session": hex}),
        ))
        .await;
    loop {
        let object = bridge.next().await;
        if object.get("event").is_some() {
            assert_eq!(
                object["event"],
                json!("end"),
                "the second capture must end cleanly: {object}"
            );
            break;
        }
        let item = &object["item"];
        assert_eq!(
            item["ansi"],
            Value::Null,
            "an unasked `ansi` is null, never absent: {object}"
        );
    }

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_line_is_answered_and_the_bridge_serves_the_next_request() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let _id = create_and_detach(&daemon.socket).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge.send_raw("{not json at all").await;
    let unattributable = bridge.next().await;
    assert_surface_version(&unattributable);
    assert_eq!(
        unattributable["id"],
        Value::Null,
        "an unreadable line has no id to echo: {unattributable}"
    );
    assert_eq!(unattributable["error"]["kind"], json!("malformed_request"));

    bridge
        .send_raw(r#"{"v":1,"id":"bad-op","op":"sessions.fly"}"#)
        .await;
    let attributable = bridge.next().await;
    assert_eq!(
        attributable["id"],
        json!("bad-op"),
        "a readable id is echoed even when the rest is not: {attributable}"
    );
    assert_eq!(attributable["error"]["kind"], json!("malformed_request"));

    bridge
        .send(&request("after", "sessions.list", &json!({})))
        .await;
    let survived = bridge.next().await;
    assert_eq!(survived["id"], json!("after"));
    assert!(
        survived["result"]["sessions"].is_array(),
        "the bridge kept serving: {survived}"
    );

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_request_fields_and_operation_parameters_are_refused() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);

    bridge
        .send_raw(r#"{"v":1,"id":"top","op":"sessions.list","extra":true}"#)
        .await;
    bridge
        .send(&request(
            "param",
            "sessions.list",
            &json!({"session": "unused"}),
        ))
        .await;
    bridge
        .send(&request("shape", "sessions.list", &json!(7)))
        .await;
    bridge
        .send_raw(r#"{"v":1,"id":"null","op":"sessions.list","params":null}"#)
        .await;
    bridge
        .send_raw(
            r#"{"v":1,"id":"stream-top","op":"sessions.capture","params":{"session":"deadbeef"},"extra":true}"#,
        )
        .await;
    bridge
        .send_raw(r#"{"v":1,"id":"stream-null","op":"sessions.search","params":null}"#)
        .await;

    let replies = bridge
        .collect_ids(&["top", "param", "shape", "null", "stream-top", "stream-null"])
        .await;
    assert_eq!(replies["top"]["error"]["kind"], json!("malformed_request"));
    assert_eq!(
        replies["param"]["error"]["kind"],
        json!("malformed_request")
    );
    assert_eq!(
        replies["shape"]["error"]["kind"],
        json!("malformed_request")
    );
    assert_eq!(replies["null"]["error"]["kind"], json!("malformed_request"));
    for id in ["stream-top", "stream-null"] {
        assert_eq!(
            replies[id]["event"],
            json!("error"),
            "an attributable stream parse failure is terminal: {}",
            replies[id]
        );
        assert_eq!(replies[id]["error"]["kind"], json!("malformed_request"));
    }

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(
        code, 0,
        "bad input does not stop the bridge; stderr: {stderr}"
    );
    assert_stderr_is_human_only(&stderr);
}

/// A streaming op whose `id` is unreadable has no id to terminate, so
/// the answer is the untagged error: the one published shape that may
/// go out with `id: null`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_streaming_request_with_no_usable_id_answers_the_untagged_error() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);

    for line in [
        r#"{"v":1,"op":"sessions.capture","params":{"session":"deadbeef"}}"#,
        r#"{"v":1,"id":{},"op":"sessions.search","params":{"session":"deadbeef","pattern":"x"}}"#,
        r#"{"v":1,"id":1.5,"op":"notifications.subscribe"}"#,
    ] {
        bridge.send_raw(line).await;
        let answer = bridge.next().await;
        assert_eq!(answer["id"], Value::Null, "{line} answered: {answer}");
        assert!(
            answer.get("event").is_none(),
            "a terminal must echo the id it terminates: {answer}",
        );
        assert_eq!(answer["error"]["kind"], json!("malformed_request"));
    }

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

/// JSON Schema counts `1.0` as the integer 1, so a line the published
/// grammar accepts must be a line the bridge accepts: `v`, `id`, and a
/// bounded parameter alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integers_spelled_as_floats_are_accepted_and_echoed_as_integers() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, banner_factory()).await;
    let id = create_and_detach(&daemon.socket).await;
    let hex = format!("{id:032x}");

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send_raw(&format!(
            r#"{{"v":1.0,"id":1.0,"op":"sessions.capture","params":{{"session":"{hex}","lines":10.0}}}}"#
        ))
        .await;

    let terminal = loop {
        let object = bridge.next().await;
        assert_eq!(
            object["id"],
            json!(1),
            "the echoed id is the integer the float spells: {object}",
        );
        if object.get("event").is_some() {
            break object;
        }
    };
    assert_eq!(
        terminal["event"],
        json!("end"),
        "the capture ran rather than being refused: {terminal}",
    );

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_frame_sized_request_is_refused_without_losing_the_bridge() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);
    let oversized = "x".repeat(felis_protocol::frame::DEFAULT_MAX_BODY as usize);

    bridge
        .send(&request(
            "large",
            "sessions.tag",
            &json!({"session": "deadbeef", "add": [oversized]}),
        ))
        .await;
    let refused = bridge.next().await;
    assert_eq!(refused["id"], json!("large"));
    assert_eq!(refused["error"]["kind"], json!("invalid_request"));

    bridge
        .send(&request("after", "sessions.list", &json!({})))
        .await;
    let survived = bridge.next().await;
    assert_eq!(survived["id"], json!("after"));
    assert!(
        survived["result"]["sessions"].is_array(),
        "a local frame refusal must not poison the daemon link: {survived}"
    );

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdin_eof_cancels_outstanding_streams_and_exits_zero() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let _id = create_and_detach(&daemon.socket).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request("notify", "notifications.subscribe", &json!({})))
        .await;
    // The round trip orders the subscription before the close.
    bridge
        .send(&request("ping", "sessions.list", &json!({})))
        .await;
    let acked = bridge.collect_for(&json!("ping"), 1).await;
    assert!(acked[0]["result"]["sessions"].is_array());

    let (code, transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "a client that closed stdin did nothing wrong");
    let terminals: Vec<&Value> = transcript
        .iter()
        .filter(|object| object["id"] == json!("notify") && object.get("event").is_some())
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "the canceled subscription gets exactly one terminal: {transcript:?}"
    );
    assert_surface_version(terminals[0]);
    // The daemon's own terminal may beat the bridge's synthesized one.
    match terminals[0]["event"].as_str() {
        Some("end") => {}
        Some("error") => assert_eq!(
            terminals[0]["error"]["kind"],
            json!("canceled"),
            "a stream stopped by the client's own EOF ends as canceled: {}",
            terminals[0]
        ),
        _ => panic!("a terminal names its event: {}", terminals[0]),
    }
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_daemon_answers_pending_point_requests_with_error_replies() {
    let tmp = private_dir();
    let mut daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);

    // A served reply first: a bridge still dialing when the daemon dies
    // reports an unreachable daemon instead of answering the requests.
    bridge
        .send(&request("served", "sessions.list", &json!({})))
        .await;
    assert_eq!(bridge.next().await["id"], json!("served"));

    // Holding the pool keeps both outstanding when the daemon dies.
    let parked = Arc::clone(&daemon.pool).lock_owned().await;
    bridge
        .send(&request("make", "sessions.spawn", &json!({})))
        .await;
    bridge
        .send(&request("queued", "sessions.list", &json!({})))
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    daemon.kill();
    drop(parked);

    let replies = bridge.collect_ids(&["make", "queued"]).await;
    for id in ["make", "queued"] {
        let reply = &replies[id];
        assert_surface_version(reply);
        assert!(
            reply.get("event").is_none(),
            "a point request is answered as a reply, never as a stream terminal: {reply}"
        );
        assert!(
            reply["error"]["kind"].is_string(),
            "a request the daemon can no longer answer gets a typed error: {reply}"
        );
    }

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_ne!(
        code, 0,
        "a bridge that lost its daemon must not report success"
    );
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_id_is_refused_while_live_and_free_once_its_terminal_is_written() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let _id = create_and_detach(&daemon.socket).await;

    let mut bridge = Bridge::start(&daemon.socket);
    for round in 0..2 {
        bridge
            .send(&request("reused", "sessions.list", &json!({})))
            .await;
        let reply = bridge.next().await;
        assert_eq!(reply["id"], json!("reused"));
        assert!(
            reply["result"]["sessions"].is_array(),
            "round {round}: an id whose reply is out is served again: {reply}"
        );
    }
    bridge
        .send(&request(
            "control-reused",
            "cancel",
            &json!({"target": "absent"}),
        ))
        .await;
    assert_eq!(bridge.next().await["id"], json!("control-reused"));
    bridge
        .send(&request("control-reused", "sessions.list", &json!({})))
        .await;
    assert!(
        bridge.next().await["result"]["sessions"].is_array(),
        "an immediate reply releases its id after stdout writes its delimiter"
    );

    // The subscription registers as its line is read, so the duplicate
    // behind it cannot win the race.
    bridge
        .send(&request("live", "notifications.subscribe", &json!({})))
        .await;
    bridge
        .send(&request("live", "sessions.list", &json!({})))
        .await;
    let refused = bridge.collect_for(&json!("live"), 1).await;
    assert_eq!(
        refused[0]["error"]["kind"],
        json!("malformed_request"),
        "two operations cannot answer under one id: {}",
        refused[0]
    );

    let (code, transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let terminals = transcript
        .iter()
        .filter(|object| object["id"] == json!("live") && object.get("event").is_some())
        .count();
    assert_eq!(
        terminals, 1,
        "the refusal did not disturb the stream holding the id: {transcript:?}"
    );
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_daemon_answers_every_outstanding_operation_and_exits_nonzero() {
    let tmp = private_dir();
    let mut daemon = Daemon::start(&tmp, quiet_factory()).await;
    let _id = create_and_detach(&daemon.socket).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request("notify", "notifications.subscribe", &json!({})))
        .await;
    bridge
        .send(&request("ping", "sessions.list", &json!({})))
        .await;
    let acked = bridge.collect_for(&json!("ping"), 1).await;
    assert!(acked[0]["result"]["sessions"].is_array());

    daemon.kill();
    let _terminal = bridge.collect_for(&json!("notify"), 1).await;

    // Not `finish`: the terminal above arrives on the observer link, so
    // a stdin EOF can still beat the anchor's loss to a clean exit.
    let (code, transcript, stderr) = bridge.exit_without_stdin_eof().await;
    assert_ne!(
        code, 0,
        "a bridge that lost its daemon must not report success"
    );
    let terminals: Vec<&Value> = transcript
        .iter()
        .filter(|object| object["id"] == json!("notify") && object.get("event").is_some())
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "the live subscription gets exactly one terminal: {transcript:?}"
    );
    assert_eq!(
        terminals[0]["event"],
        json!("error"),
        "a lost daemon is a failing terminal, never a clean end: {}",
        terminals[0]
    );
    assert!(
        terminals[0]["error"]["kind"].is_string(),
        "the terminal names a typed kind: {}",
        terminals[0]
    );
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cold_socket_is_reported_on_stdout_and_exits_nonzero() {
    let tmp = private_dir();
    let socket = tmp.path().join("no-daemon.sock");

    let mut bridge = Bridge::start(&socket);
    let object = bridge.next().await;
    assert_surface_version(&object);
    assert_eq!(
        object["id"],
        Value::Null,
        "a startup failure belongs to no request: {object}"
    );
    assert_eq!(object["error"]["kind"], json!("daemon_unreachable"));

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_ne!(code, 0, "no daemon is not success");
    assert!(
        !socket.exists(),
        "the bridge must not have started a daemon at {}",
        socket.display()
    );
    assert_stderr_is_human_only(&stderr);
}

/// A full daemon answered, so this is exit 1 (the verb met a ceiling),
/// not the exit 2 that means there is no daemon to reach
/// (docs/reference/cli.md "Exit codes").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_at_capacity_dial_is_typed_and_exits_one() {
    use felis_daemon::serve::ConnectionAdmission;

    let tmp = private_dir();
    // A cap of zero admits nothing, so the refusal needs no racing
    // second dial to provoke.
    let daemon = Daemon::start_with_caps(
        &tmp,
        quiet_factory(),
        DaemonCaps {
            admission: ConnectionAdmission::with_refusal_slots(0, 4),
            ..DaemonCaps::default()
        },
    )
    .await;

    let mut bridge = Bridge::start(&daemon.socket);
    let object = bridge.next().await;
    assert_surface_version(&object);
    assert_eq!(object["id"], Value::Null, "{object}");
    assert_eq!(object["error"]["kind"], json!("at_capacity"), "{object}");

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 1, "a ceiling is a refusal, not an unreachable daemon");
    assert_stderr_is_human_only(&stderr);
}

/// The anchor is opened at startup and stays silent until the editor
/// asks for something, which is exactly the shape the first-operation
/// deadline would cut. The daemon here runs with that deadline set far
/// below the idle, so a bridge that survives it proves the `Ops`
/// exemption rather than the test's own patience.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_bridge_outlives_the_first_operation_deadline() {
    use felis_daemon::serve::HandshakeDeadlines;

    let tmp = private_dir();
    let first_op = Duration::from_millis(200);
    let daemon = Daemon::start_with_caps(
        &tmp,
        quiet_factory(),
        DaemonCaps {
            handshake: HandshakeDeadlines {
                first_op,
                ..HandshakeDeadlines::default()
            },
            ..DaemonCaps::default()
        },
    )
    .await;

    let mut bridge = Bridge::start(&daemon.socket);
    tokio::time::sleep(first_op * 5).await;

    bridge
        .send(&request("late", "sessions.list", &json!({})))
        .await;
    let object = bridge.next().await;
    assert_eq!(
        object["id"],
        json!("late"),
        "the anchor must still be usable after idling: {object}"
    );
    assert!(
        object.get("error").is_none(),
        "an idled anchor answers the verb, not a lost daemon: {object}"
    );

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stdin EOF is a clean exit");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relative_spawn_cwd_is_resolved_from_the_bridge_process() {
    let tmp = private_dir();
    let nested = tmp.path().join("editor-project");
    std::fs::create_dir(&nested).unwrap();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args(["-c", "pwd; read x"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let daemon = Daemon::start(&tmp, factory).await;
    let mut bridge = Bridge::start_in(&daemon.socket, Some(tmp.path()));

    bridge
        .send(&request(
            "spawn",
            "sessions.spawn",
            &json!({"cwd": "editor-project"}),
        ))
        .await;
    let session = bridge.next().await["result"]["id"]
        .as_str()
        .expect("spawn returns a session id")
        .to_owned();
    tokio::time::sleep(Duration::from_millis(200)).await;
    bridge
        .send(&request(
            "capture",
            "sessions.capture",
            &json!({"session": session}),
        ))
        .await;
    let mut rows = Vec::new();
    loop {
        let object = bridge.next().await;
        if object.get("event").is_some() {
            assert_eq!(object["event"], json!("end"), "{object}");
            break;
        }
        rows.push(
            object["item"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        );
    }
    // `pwd` prints the physical path, and a long temp dir wraps it
    // across rows.
    let nested = nested.canonicalize().unwrap();
    assert!(
        rows.concat()
            .contains(&nested.to_string_lossy().into_owned()),
        "the child reports the cwd anchored by the bridge: {rows:?}"
    );

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutating_verbs_answer_on_stdout_with_the_id_they_were_given() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request("spawn", "sessions.spawn", &json!({})))
        .await;
    let spawned = bridge.next().await;
    assert_eq!(spawned["id"], json!("spawn"));
    let session = spawned["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("spawn replies with the new id: {spawned}"))
        .to_owned();

    bridge
        .send(&request(
            "tag",
            "sessions.tag",
            &json!({"session": session, "add": ["work"]}),
        ))
        .await;
    let tagged = bridge.next().await;
    assert_eq!(tagged["result"]["tags"], json!(["work"]));

    bridge
        .send(&request(
            "kill",
            "sessions.kill",
            &json!({"session": session}),
        ))
        .await;
    let killed = bridge.next().await;
    assert_eq!(killed["result"]["id"], json!(session));

    bridge
        .send(&request(
            "gone",
            "sessions.kill",
            &json!({"session": session}),
        ))
        .await;
    let missing = bridge.next().await;
    assert_eq!(missing["id"], json!("gone"));
    assert_eq!(missing["error"]["kind"], json!("no_match"));

    // Half a geometry names no grid: `rows` and `cols` travel together
    // or not at all, since absence is what asks for the default.
    bridge
        .send(&request("half", "sessions.spawn", &json!({"rows": 24})))
        .await;
    let half = bridge.next().await;
    assert_eq!(half["id"], json!("half"));
    assert_eq!(half["error"]["kind"], json!("invalid_request"));

    // An explicit `null` is absence, so it names no axis: both nulls
    // ask for the default grid, one null beside a number is still half
    // a geometry. The published grammar draws the same two states.
    bridge
        .send(&request(
            "nulled",
            "sessions.spawn",
            &json!({"rows": null, "cols": null}),
        ))
        .await;
    let nulled = bridge.next().await;
    let default_grid = nulled["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a nulled grid spawns on the default: {nulled}"))
        .to_owned();
    bridge
        .send(&request(
            "half-null",
            "sessions.spawn",
            &json!({"rows": null, "cols": 80}),
        ))
        .await;
    let half_null = bridge.next().await;
    assert_eq!(half_null["id"], json!("half-null"));
    assert_eq!(half_null["error"]["kind"], json!("invalid_request"));

    bridge
        .send(&request(
            "kill-default",
            "sessions.kill",
            &json!({"session": default_grid}),
        ))
        .await;
    assert_eq!(bridge.next().await["result"]["id"], json!(default_grid));

    // A whole geometry past the REQ-605a bound is refused by the
    // daemon, never clamped, and the bridge narrows nothing on the way:
    // a value past `u16::MAX` reaches the same admission point and is
    // refused on the merits rather than as an unreadable request.
    for (id, rows) in [("wide", 3000), ("very-wide", 70_000)] {
        bridge
            .send(&request(
                id,
                "sessions.spawn",
                &json!({"rows": rows, "cols": 80}),
            ))
            .await;
        let refused = bridge.next().await;
        assert_eq!(refused["id"], json!(id));
        assert_eq!(
            refused["error"]["kind"],
            json!("invalid_request"),
            "{refused}"
        );
    }

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

/// REQ-105a's ordering half for the bridge: an over-limit payload is
/// refused before the session is resolved or attached. The session
/// prefix names nothing, so a check that ran after the resolve would
/// answer `no_match`; `invalid_request` is only reachable if the
/// payload was measured first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_limit_send_is_refused_before_the_session_is_resolved() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);

    let text = "a".repeat(felis_protocol::messages::MAX_PASTE_BYTES + 1);
    bridge
        .send(&request(
            "big",
            "sessions.send",
            &json!({"session": "ffffffffffffffffffffffffffffffff", "text": text}),
        ))
        .await;
    let refused = bridge.next().await;
    assert_eq!(refused["id"], json!("big"));
    assert_eq!(refused["error"]["kind"], json!("invalid_request"));

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_spells_the_all_subscriber_disconnect_evict() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request("spawn", "sessions.spawn", &json!({})))
        .await;
    let session = bridge.next().await["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("spawn replies with the new id"))
        .to_owned();

    bridge
        .send(&request(
            "evict",
            "sessions.evict",
            &json!({"session": session}),
        ))
        .await;
    let evicted = bridge.next().await;
    assert_eq!(evicted["id"], json!("evict"));
    assert_eq!(evicted["result"]["id"], json!(session));
    assert_eq!(evicted["result"]["was_attached"], json!(false));

    bridge
        .send(&request(
            "old",
            "sessions.detach",
            &json!({"session": session}),
        ))
        .await;
    let refused = bridge.next().await;
    assert_eq!(refused["id"], json!("old"));
    assert_eq!(refused["error"]["kind"], json!("malformed_request"));

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_types_into_a_session_that_a_later_capture_reads_back() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        // The PTY's own echo prints `marker` too; only the shell
        // produces `ECHOED-`.
        cmd.args(["-c", "read x; printf 'ECHOED-%s\\r\\n' \"$x\"; read y"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let daemon = Daemon::start(&tmp, factory).await;
    let id = create_and_detach(&daemon.socket).await;
    let hex = format!("{id:032x}");

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request(
            "send",
            "sessions.send",
            &json!({"session": hex, "text": "marker\n", "raw": true}),
        ))
        .await;
    let acked = bridge.next().await;
    assert_eq!(acked["id"], json!("send"));
    assert_eq!(
        acked["result"]["id"],
        json!(hex),
        "the ack names the session it typed into: {acked}"
    );

    // Let the shell run and the parked drain land its output in the grid.
    tokio::time::sleep(Duration::from_millis(600)).await;

    bridge
        .send(&request(
            "cap",
            "sessions.capture",
            &json!({"session": hex}),
        ))
        .await;
    let mut echoed = false;
    loop {
        let object = bridge.next().await;
        if object.get("event").is_some() {
            assert_eq!(object["event"], json!("end"), "capture ended cleanly");
            break;
        }
        if object["item"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("ECHOED-marker"))
        {
            echoed = true;
        }
    }
    assert!(echoed, "the session ran what the bridge typed into it");

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_stops_one_stream_and_leaves_the_bridge_serving() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let _id = create_and_detach(&daemon.socket).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request("notify", "notifications.subscribe", &json!({})))
        .await;
    // The round trip orders the subscription before the cancel.
    bridge
        .send(&request("ping", "sessions.list", &json!({})))
        .await;
    let acked = bridge.collect_for(&json!("ping"), 1).await;
    assert!(acked[0]["result"]["sessions"].is_array());

    bridge
        .send(&request("stop", "cancel", &json!({"target": "notify"})))
        .await;
    let confirmed = bridge.collect_for(&json!("stop"), 1).await;
    assert_eq!(
        confirmed[0]["result"]["canceled"],
        json!(true),
        "cancel found the stream it named: {}",
        confirmed[0]
    );

    bridge
        .send(&request("after", "sessions.list", &json!({})))
        .await;
    let survived = bridge.collect_for(&json!("after"), 1).await;
    assert!(survived[0]["result"]["sessions"].is_array());

    let (code, transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let terminals = transcript
        .iter()
        .filter(|object| object["id"] == json!("notify") && object.get("event").is_some())
        .count();
    assert_eq!(
        terminals, 1,
        "the canceled stream ends exactly once: {transcript:?}"
    );
    assert_stderr_is_human_only(&stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_stream_terminal_carries_count_beside_the_event() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, banner_factory()).await;
    let id = create_and_detach(&daemon.socket).await;
    let hex = format!("{id:032x}");

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request(
            "cap",
            "sessions.capture",
            &json!({"session": hex}),
        ))
        .await;

    let terminal = loop {
        let object = bridge.next().await;
        assert_surface_version(&object);
        if object.get("event").is_some() {
            break object;
        }
    };
    assert_eq!(terminal["event"], json!("end"));
    assert!(
        terminal["count"].is_u64(),
        "the tally rides the terminal itself: {terminal}"
    );
    assert!(
        terminal.get("summary").is_none() && terminal.get("done").is_none(),
        "the pre-C-14 summary body is gone, not nested: {terminal}"
    );
    // No OSC 133 marks in the fixture, so no exit code: absent, never
    // null.
    assert!(
        terminal.get("exit_code").is_none(),
        "exit_code is omitted when the op has none: {terminal}"
    );
    assert_eq!(
        terminal.as_object().map(serde_json::Map::len),
        Some(4),
        "v, id, event, count — and nothing else: {terminal}"
    );

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

/// Two spawns sent before either is answered: both are dispatched and
/// each gets its own result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelined_spawns_each_get_their_own_result() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request("first", "sessions.spawn", &json!({})))
        .await;
    bridge
        .send(&request("second", "sessions.spawn", &json!({})))
        .await;

    let mut ids = Vec::new();
    for _ in 0..2 {
        let reply = bridge.next().await;
        let request_id = reply["id"]
            .as_str()
            .unwrap_or_else(|| panic!("every reply names its request: {reply}"))
            .to_owned();
        let session = reply["result"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("spawn replies with the new id: {reply}"))
            .to_owned();
        ids.push((request_id, session));
    }
    ids.sort();
    assert_eq!(ids[0].0, "first");
    assert_eq!(ids[1].0, "second");
    assert_ne!(ids[0].1, ids[1].1, "two spawns are two sessions");
}

/// A daemon stand-in that answers two `Ops::Spawn` requests in reverse
/// order, so the bridge has only the `request_id` to attribute them by.
/// It returns the ids it saw, sorted: which op wins the lower id is the
/// runtime's business, but the pair must arrive without a gap or a
/// reuse.
fn reversing_peer(socket: std::path::PathBuf) -> tokio::task::JoinHandle<Vec<u64>> {
    use felis_protocol::codec;
    use felis_protocol::messages::{
        ConnToClientMsg, ConnToDaemonMsg, Correlation, GridDims, OpsToClientMsg, OpsToDaemonMsg,
        SessionInfo, SpawnOutcome,
    };
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::{FrameReader, FrameWriter};

    tokio::spawn(async move {
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the peer socket");
        let (stream, _addr) = listener.accept().await.expect("the bridge dials once");
        let (mut read_half, mut write_half) = stream.into_split();
        let _bootstrap = felis_transport::preface::read_client_bootstrap(&mut read_half)
            .await
            .expect("client preface");
        felis_transport::preface::write_daemon_preface(
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
        let hello = reader.next_frame().await.expect("read").expect("a hello");
        assert!(matches!(
            codec::decode::<ConnToDaemonMsg>(&hello.body).expect("decode hello"),
            ConnToDaemonMsg::Hello { .. }
        ));
        writer
            .send(&ConnToClientMsg::Welcome { identity: None })
            .await
            .expect("welcome");

        let mut requests = Vec::new();
        for _ in 0..2 {
            let frame = reader.next_frame().await.expect("read").expect("a spawn");
            let request = codec::peek_correlation(&frame.body)
                .expect("correlated")
                .and_then(Correlation::request_id)
                .expect("a spawn carries its request id");
            let OpsToDaemonMsg::Spawn { args } =
                codec::decode::<OpsToDaemonMsg>(&frame.body).expect("decode ops")
            else {
                panic!("the bridge sends Ops::Spawn for sessions.spawn");
            };
            let tag = args.tags.first().expect("the test tags each spawn").clone();
            requests.push((request, tag));
        }

        for (request, tag) in requests.iter().rev() {
            let seq = if tag == "first" { 1 } else { 2 };
            let info = SessionInfo {
                id: u128::from(seq),
                dims: GridDims {
                    rows: 24,
                    cols: 80,
                    pixel_w: 0,
                    pixel_h: 0,
                },
                title: None,
                cwd: None,
                idle_seconds: None,
                tags: vec![tag.clone()],
                last_notification: None,
                foreground: None,
                exited: false,
                last_exit_code: None,
                attachments: Vec::new(),
                sequence: std::num::NonZeroU64::new(seq).expect("a minted sequence"),
            };
            writer
                .send_correlated(
                    &OpsToClientMsg::Spawned {
                        outcome: SpawnOutcome::Ok {
                            info: Box::new(info),
                        },
                    },
                    Correlation::request(*request),
                )
                .await
                .expect("a spawn reply");
        }
        // Held open until the bridge hangs up: a closed link would
        // reach it as a lost daemon instead of two answered ops.
        while let Ok(Some(_frame)) = reader.next_frame().await {}
        let mut wire: Vec<u64> = requests
            .into_iter()
            .map(|(request, _tag)| request.get())
            .collect();
        wire.sort_unstable();
        wire
    })
}

/// Two spawns in flight with the replies reordered: each result is
/// attributed by its request id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reordered_spawn_replies_are_attributed_by_request_id() {
    let tmp = private_dir();
    let socket = tmp.path().join("peer.sock");
    let peer = reversing_peer(socket.clone());

    let mut bridge = Bridge::start(&socket);
    bridge
        .send(&request(
            "first",
            "sessions.spawn",
            &json!({"tags": ["first"]}),
        ))
        .await;
    bridge
        .send(&request(
            "second",
            "sessions.spawn",
            &json!({"tags": ["second"]}),
        ))
        .await;

    let found = bridge.collect_ids(&["first", "second"]).await;
    assert_eq!(
        found["first"]["result"]["id"],
        json!(format!("{:032x}", 1_u128)),
        "the first op keeps the reply carrying its own request id: {:?}",
        found["first"]
    );
    assert_eq!(
        found["second"]["result"]["id"],
        json!(format!("{:032x}", 2_u128)),
        "the second op keeps its own reply: {:?}",
        found["second"]
    );

    drop(bridge);
    let wire = tokio::time::timeout(REPLY_TIMEOUT, peer)
        .await
        .expect("the peer finished")
        .expect("the peer must not panic");
    assert_eq!(
        wire,
        vec![1, 2],
        "two ops in flight take two consecutive ids: the peer must see 1 and 2, neither skipped nor reused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_switch_denials_use_the_shared_snake_case_kinds() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request("a", "sessions.spawn", &json!({})))
        .await;
    let from = bridge.next().await["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("spawn replies with the new id"))
        .to_owned();
    bridge
        .send(&request("b", "sessions.spawn", &json!({})))
        .await;
    let to = bridge.next().await["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("spawn replies with the new id"))
        .to_owned();

    // Neither session has a window.
    bridge
        .send(&request(
            "default",
            "sessions.switch",
            &json!({"from": from, "to": to}),
        ))
        .await;
    let denied = bridge.next().await;
    assert_eq!(denied["id"], json!("default"));
    assert_eq!(denied["error"]["kind"], json!("no_input_owner"), "{denied}");

    bridge
        .send(&request(
            "named",
            "sessions.switch",
            &json!({"from": from, "to": to, "attachment": "4242"}),
        ))
        .await;
    let stale = bridge.next().await;
    assert_eq!(stale["id"], json!("named"));
    assert_eq!(
        stale["error"]["kind"],
        json!("no_such_attachment"),
        "{stale}"
    );

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

/// The `sessions.switch` result object: exactly `from`, `to` and
/// `queued`, with `queued` counting the windows that took the push.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_switch_reports_from_to_and_the_queued_count() {
    use felis_client_core::{Offer, connect};

    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let to = create_and_detach(&daemon.socket).await;
    // The window-mode connection is the one the daemon pushes to.
    let mut window = connect(&daemon.socket, Offer::window(false)).await.unwrap();
    let from = window.create_with(SpawnArgs::default()).await.unwrap().id;

    let mut bridge = Bridge::start(&daemon.socket);
    bridge
        .send(&request(
            "switch",
            "sessions.switch",
            &json!({"from": format!("{from:032x}"), "to": format!("{to:032x}")}),
        ))
        .await;
    let reply = bridge.next().await;
    assert_eq!(reply["id"], json!("switch"), "{reply}");
    assert_eq!(
        reply["result"],
        json!({
            "from": format!("{from:032x}"),
            "to": format!("{to:032x}"),
            "queued": 1,
        }),
        "the result object carries the resolved pair and the queue count: {reply}",
    );

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_stderr_is_human_only(&stderr);
}

/// A hand-rolled daemon that completes the handshake and writes one
/// push: the real daemon routes the window-only pushes to `Window`
/// subscribers, so nothing else puts one on an `Ops` connection.
async fn rogue_daemon(socket: std::path::PathBuf, rogue: PushMsg) {
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::{
        Endpoint, FrameReader, FrameWriter, Listener,
        preface::{read_client_bootstrap, write_daemon_preface},
        server_split,
    };

    let listener = Listener::bind(&Endpoint::unix(socket)).expect("bind the rogue socket");
    let stream = listener.accept().await.expect("accept the bridge");
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
    let hello = reader
        .next_frame()
        .await
        .expect("read Hello")
        .expect("the bridge sends Hello");
    assert_eq!(hello.kind, MessageKind::Conn.as_u16());
    writer
        .send(&ConnToClientMsg::Welcome { identity: None })
        .await
        .expect("write Welcome");
    // Not before the bridge has a request outstanding: the pump that
    // routes replies is what must meet the push.
    let _request = reader
        .next_frame()
        .await
        .expect("read the verb's request")
        .expect("the bridge sends a request");
    writer.send(&rogue).await.expect("write the rogue push");
    // The bridge must end the connection itself; holding the socket
    // open keeps a hang distinguishable from the refusal.
    tokio::time::sleep(Duration::from_secs(30)).await;
}

/// REQ-114: a frame the arm table says this connection may never
/// receive ends the connection, even on the drain path that has no use
/// for it. The bridge dials `Ops` and never attaches, so the whole
/// `Push` family is out of phase.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_only_push_on_the_ops_link_fails_the_link() {
    let tmp = private_dir();
    let socket = tmp.path().join("rogue.sock");
    let daemon = tokio::spawn(rogue_daemon(socket.clone(), PushMsg::Reattach { id: 1 }));
    for _ in 0..400 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let mut bridge = Bridge::start(&socket);
    bridge
        .send(&request("list", "sessions.list", &json!({})))
        .await;
    let answer = bridge.next().await;
    assert_eq!(answer["id"], json!("list"));
    assert_eq!(
        answer["error"]["kind"],
        json!("protocol"),
        "the push must fail the link, not be dropped: {answer}"
    );
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Push") && message.contains("Setup"),
        "the refusal must name what was denied and the phase that denied it: {answer}"
    );

    let (_code, _transcript, _stderr) = bridge.finish().await;
    daemon.abort();
}

/// Concurrent replies arrive in whatever order the daemon answers, and
/// a session id is minted per run, so a byte-exact transcript would
/// flake. A golden conversation is therefore diffed after masking the
/// per-run values and grouping stdout by the request id it answers:
/// order within one id is the contract, order across ids is not.
fn normalize(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let masked = match key.as_str() {
                        "message" | "text" | "ansi" | "cwd" | "title" | "session_title"
                        | "foreground" | "attached_at" | "short_id" | "notification_id"
                        | "idle_seconds" => {
                            json!(format!("<{key}>"))
                        }
                        _ => normalize(value),
                    };
                    (key.clone(), masked)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::String(text) if text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()) => {
            json!("<session>")
        }
        other => other.clone(),
    }
}

/// A reply or a terminal: what closes one request id. `item` and the
/// `lag` event do not.
fn is_final(object: &Value) -> bool {
    match object.get("event").and_then(Value::as_str) {
        Some("end" | "error") => true,
        Some(_) => false,
        None => object.get("result").is_some() || object.get("error").is_some(),
    }
}

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bridge")
        .join(format!("{name}.jsonl"))
}

fn read_golden(name: &str) -> Vec<Value> {
    let path = golden_path(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} is missing ({err})", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a golden line is JSON"))
        .collect()
}

/// Feeds a fixture's `in` lines to a live bridge and diffs its stdout
/// against the fixture's `out` lines. `UPDATE_GOLDEN=1` rewrites them,
/// so the committed transcript is what the binary actually produced.
async fn run_golden(name: &str, factory: SessionFactory) {
    let tmp = private_dir();
    let mut daemon = Daemon::start(&tmp, factory).await;
    let id = create_and_detach(&daemon.socket).await;
    let hex = format!("{id:032x}");

    let fixture = read_golden(name);
    let mut bridge = Bridge::start(&daemon.socket);
    for line in &fixture {
        match line["dir"].as_str() {
            Some("in") => {
                let mut request = line.clone();
                request.as_object_mut().unwrap().remove("dir");
                let request: Value =
                    serde_json::from_str(&request.to_string().replace("{session}", &hex)).unwrap();
                bridge.send(&request).await;
            }
            // Losing the daemon is part of the conversation: the
            // fixture pins the terminal every open op then receives.
            Some("kill-daemon") => {
                tokio::time::sleep(Duration::from_millis(300)).await;
                daemon.kill();
            }
            Some("out") | None => {}
            Some(other) => panic!("unknown fixture direction `{other}`"),
        }
    }
    // Every request ends in exactly one reply or terminal, so the
    // conversation is complete when that many have arrived. Draining to
    // EOF instead would race stdin EOF against the items still queued.
    let requests = fixture
        .iter()
        .filter(|line| line["dir"] == json!("in"))
        .count();
    let mut transcript = Vec::new();
    let mut finals = 0;
    while finals < requests {
        let object = bridge.next().await;
        if is_final(&object) {
            finals += 1;
        }
        transcript.push(object);
    }
    let (_code, rest, stderr) = bridge.finish().await;
    assert_stderr_is_human_only(&stderr);
    // `finish` replays the whole transcript, so an equal length is
    // "nothing followed the last terminal".
    assert_eq!(
        rest.len(),
        transcript.len(),
        "`{name}` wrote {rest:?} after every id had its terminal",
    );

    let produced: Vec<Value> = transcript.iter().map(normalize).collect();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        let mut text = String::new();
        for line in &fixture {
            if line["dir"] == json!("out") {
                continue;
            }
            text.push_str(&line.to_string());
            text.push('\n');
        }
        for object in &produced {
            let mut line = serde_json::Map::new();
            line.insert("dir".to_owned(), json!("out"));
            line.extend(object.as_object().unwrap().clone());
            text.push_str(&Value::Object(line).to_string());
            text.push('\n');
        }
        std::fs::write(golden_path(name), text).unwrap();
        return;
    }

    let expected: Vec<Value> = fixture
        .iter()
        .filter(|line| line["dir"] == json!("out"))
        .map(|line| {
            let mut object = line.clone();
            object.as_object_mut().unwrap().remove("dir");
            object
        })
        .collect();
    let by_id = |objects: &[Value]| {
        let mut groups: std::collections::BTreeMap<String, Vec<Value>> =
            std::collections::BTreeMap::new();
        for object in objects {
            groups
                .entry(object["id"].to_string())
                .or_default()
                .push(object.clone());
        }
        groups
    };
    assert_eq!(
        by_id(&produced),
        by_id(&expected),
        "`{name}` diverged; regenerate with `UPDATE_GOLDEN=1` after checking the change is wanted",
    );
}

/// Two point requests and a bridge-local verb answered concurrently:
/// the fixture is order-insensitive across ids by construction, so it
/// passes whichever reply lands first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golden_concurrent_replies() {
    run_golden("concurrent-replies", quiet_factory()).await;
}

/// The clean terminal path: items, then exactly one `end`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golden_stream_ends() {
    run_golden("stream-ends", banner_factory()).await;
}

/// The failing terminal path: one `error` terminal and no item, so a
/// consumer never has to read the failure off EOF.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golden_failure_before_the_first_item() {
    run_golden("failure-before-first-item", quiet_factory()).await;
}

/// The canceled terminal path, racing the cancel against a live stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golden_cancel_a_live_stream() {
    run_golden("cancel-a-live-stream", quiet_factory()).await;
}

/// The lost-daemon terminal path: every open op is closed by the
/// bridge itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golden_daemon_loss() {
    run_golden("daemon-loss", quiet_factory()).await;
}

/// The schema's request grammar and the binary draw one line: a fixture
/// the bundle rejects is refused by a live bridge as
/// `malformed_request`, so a consumer that validates locally learns
/// nothing the daemon would have allowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_request_the_schema_rejects_is_refused_by_a_live_bridge() {
    let tmp = private_dir();
    let daemon = Daemon::start(&tmp, quiet_factory()).await;
    let mut bridge = Bridge::start(&daemon.socket);

    let fixtures = schema::invalid_fixtures("req-");
    for (name, line) in &fixtures {
        assert!(
            !schema::bridge_request_is_valid(line),
            "{name} is supposed to be an invalid request",
        );
        bridge.send_raw(&line.to_string()).await;
    }
    for (name, _) in &fixtures {
        let answer = bridge.next().await;
        assert_eq!(
            answer["error"]["kind"],
            json!("malformed_request"),
            "{name}: {answer}",
        );
    }

    let (code, _transcript, stderr) = bridge.finish().await;
    assert_eq!(code, 0, "stderr: {stderr}");
}

/// What a scripted daemon does once it has read a `SessionAttach`.
#[derive(Clone, Copy)]
enum AttachScript {
    /// Answer the refusal the daemon's own resolution would give.
    Refuse(felis_protocol::messages::AttachFailure),
    /// Hang up without writing anything.
    Hangup,
    /// Land the attach, then assert what the bridge puts on the wire
    /// after an input: the fence, and nothing else.
    AcceptAndFence,
}

/// A daemon whose roster answers name exactly one session, so a caller
/// that resolved the prefix first would see `found` and proceed, and
/// whose attach then runs `script`. Connections are served in a loop:
/// the bridge dials an auxiliary link beside its anchor.
async fn scripted_attach_daemon(socket: std::path::PathBuf, script: AttachScript) {
    use felis_protocol::codec;
    use felis_protocol::messages::{
        AttachTarget, Correlation, GridDims, GridMsg, InfoOutcome, OpsToClientMsg, OpsToDaemonMsg,
        SessionInfo, SessionToClientMsg, SessionToDaemonMsg,
    };
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::{
        Endpoint, FrameReader, FrameWriter, Listener,
        preface::{read_client_bootstrap, write_daemon_preface},
        server_split,
    };

    const fn one_session() -> SessionInfo {
        SessionInfo {
            id: 0x00ab_cdef,
            dims: GridDims {
                rows: 24,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            },
            title: None,
            cwd: None,
            idle_seconds: None,
            tags: Vec::new(),
            last_notification: None,
            foreground: None,
            last_exit_code: None,
            exited: false,
            attachments: Vec::new(),
            sequence: std::num::NonZeroU64::MIN,
        }
    }

    let listener = Listener::bind(&Endpoint::unix(socket)).expect("bind the scripted socket");
    loop {
        let stream = listener.accept().await.expect("accept a bridge connection");
        tokio::spawn(async move {
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
            let _hello = reader
                .next_frame()
                .await
                .expect("read Hello")
                .expect("the bridge sends Hello");
            writer
                .send(&ConnToClientMsg::Welcome { identity: None })
                .await
                .expect("write Welcome");

            while let Ok(Some(frame)) = reader.next_frame().await {
                match MessageKind::from_u16(frame.kind).expect("a known frame kind") {
                    MessageKind::Session => {
                        let msg = codec::decode::<SessionToDaemonMsg>(&frame.body)
                            .expect("decode the attach");
                        let SessionToDaemonMsg::Attach { target, .. } = msg else {
                            continue;
                        };
                        assert!(
                            matches!(target, AttachTarget::Prefix(_)),
                            "the attach itself must carry the prefix, so the daemon resolves \
                             it in the same step that takes the handle: {target:?}"
                        );
                        match script {
                            AttachScript::Refuse(reason) => {
                                writer
                                    .send(&SessionToClientMsg::AttachFailed {
                                        reason: felis_protocol::messages::AttachRefusal::Attach(
                                            reason,
                                        ),
                                        detail: "2 sessions match".to_owned(),
                                    })
                                    .await
                                    .expect("write the refusal");
                            }
                            AttachScript::Hangup => {}
                            AttachScript::AcceptAndFence => {
                                writer
                                    .send(&SessionToClientMsg::Attached {
                                        info: one_session(),
                                    })
                                    .await
                                    .expect("write the attach ack");
                                writer
                                    .send(&GridMsg::RehydrateEnd)
                                    .await
                                    .expect("close the rehydrate burst");
                                // The theme the bridge configures on
                                // attach rides ahead of the input.
                                let mut input = reader
                                    .next_frame()
                                    .await
                                    .expect("read the configured theme")
                                    .expect("the bridge configures its theme");
                                while MessageKind::from_u16(input.kind)
                                    == Some(MessageKind::Session)
                                {
                                    input = reader
                                        .next_frame()
                                        .await
                                        .expect("read the input")
                                        .expect("the bridge sends the input");
                                }
                                assert_eq!(
                                    MessageKind::from_u16(input.kind),
                                    Some(MessageKind::Input),
                                    "the input is what follows the attach",
                                );
                                let barrier = reader
                                    .next_frame()
                                    .await
                                    .expect("read the barrier")
                                    .expect("the bridge sends a barrier");
                                assert_eq!(
                                    MessageKind::from_u16(barrier.kind),
                                    Some(MessageKind::Session),
                                    "the barrier is a session fence, not a roster query",
                                );
                                assert_eq!(
                                    codec::decode::<SessionToDaemonMsg>(&barrier.body)
                                        .expect("decode the barrier"),
                                    SessionToDaemonMsg::InputFence,
                                );
                                let request = codec::peek_correlation(&barrier.body)
                                    .expect("a fence is correlated")
                                    .and_then(Correlation::request_id)
                                    .expect("a fence carries a request id");
                                writer
                                    .send_correlated(
                                        &SessionToClientMsg::InputAccepted,
                                        Correlation::request(request),
                                    )
                                    .await
                                    .expect("write the fence reply");
                                continue;
                            }
                        }
                        return;
                    }
                    // Answered as one match, so a caller that resolved
                    // before attaching would get this far: the attach
                    // is what has to carry the verdict.
                    MessageKind::Ops => {
                        let request = codec::peek_correlation(&frame.body)
                            .expect("an Ops verb is correlated")
                            .and_then(Correlation::request_id)
                            .expect("an Ops verb carries a request id");
                        let reply = match codec::decode::<OpsToDaemonMsg>(&frame.body)
                            .expect("decode Ops")
                        {
                            OpsToDaemonMsg::Info { .. } => OpsToClientMsg::InfoReply {
                                outcome: InfoOutcome::Found {
                                    session: Box::new(one_session()),
                                    short_id: "00abcdef".to_owned(),
                                },
                            },
                            _ => OpsToClientMsg::Listed {
                                sessions: vec![one_session()],
                            },
                        };
                        writer
                            .send_correlated(&reply, Correlation::request(request))
                            .await
                            .expect("write the Ops reply");
                    }
                    _ => {}
                }
            }
        });
    }
}

async fn await_socket(socket: &std::path::Path) {
    for _ in 0..400 {
        if socket.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the scripted daemon never bound its socket");
}

/// A second session matching the prefix appears between a preliminary
/// resolution and the attach. Resolving first and attaching by the id
/// that came back would land on the session picked before the race,
/// so the refusal is what proves the attach named the prefix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prefix_that_turns_ambiguous_before_the_attach_is_reported_ambiguous() {
    let tmp = private_dir();
    let socket = tmp.path().join("ambiguous.sock");
    let daemon = tokio::spawn(scripted_attach_daemon(
        socket.clone(),
        AttachScript::Refuse(felis_protocol::messages::AttachFailure::Ambiguous),
    ));
    await_socket(&socket).await;

    let mut bridge = Bridge::start(&socket);
    bridge
        .send(&request(
            "send",
            "sessions.send",
            &json!({"session": "00", "text": "hi"}),
        ))
        .await;
    let answer = bridge.next().await;
    assert_surface_version(&answer);
    assert_eq!(answer["id"], json!("send"));
    assert_eq!(
        answer["error"]["kind"],
        json!("ambiguous"),
        "the attach's own refusal is the verdict: {answer}"
    );

    let (_code, _transcript, stderr) = bridge.finish().await;
    assert_stderr_is_human_only(&stderr);
    daemon.abort();
}

/// The barrier `sessions.send` waits on is the typed fence: a roster
/// query would couple input admission to an unrelated verb, and a
/// daemon that answered no `Ops::List` would hang the send.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bridge_send_waits_for_the_input_fence() {
    let tmp = private_dir();
    let socket = tmp.path().join("fence.sock");
    let daemon = tokio::spawn(scripted_attach_daemon(
        socket.clone(),
        AttachScript::AcceptAndFence,
    ));
    await_socket(&socket).await;

    let mut bridge = Bridge::start(&socket);
    bridge
        .send(&request(
            "send",
            "sessions.send",
            &json!({"session": "00", "text": "hi"}),
        ))
        .await;
    let answer = bridge.next().await;
    assert_surface_version(&answer);
    assert_eq!(answer["id"], json!("send"));
    assert_eq!(
        answer["result"]["id"], "00000000000000000000000000abcdef",
        "the fence reply is what completes the send: {answer}"
    );

    let (_code, _transcript, stderr) = bridge.finish().await;
    assert_stderr_is_human_only(&stderr);
    daemon.abort();
}

/// A daemon that hangs up on the attach is the daemon going away, not
/// the daemon answering "no such session": those carry different exit
/// codes, and a caller that cannot tell them apart retries the wrong
/// one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_closing_on_the_attach_is_reported_as_a_lost_daemon() {
    let tmp = private_dir();
    let socket = tmp.path().join("hangup.sock");
    let daemon = tokio::spawn(scripted_attach_daemon(socket.clone(), AttachScript::Hangup));
    await_socket(&socket).await;

    let mut bridge = Bridge::start(&socket);
    bridge
        .send(&request(
            "capture",
            "sessions.capture",
            &json!({"session": "00"}),
        ))
        .await;
    let answer = bridge.next().await;
    assert_surface_version(&answer);
    assert_eq!(answer["id"], json!("capture"));
    assert_eq!(
        answer["error"]["kind"],
        json!("daemon_lost"),
        "a transport loss is not a domain refusal: {answer}"
    );

    let (_code, _transcript, stderr) = bridge.finish().await;
    assert_stderr_is_human_only(&stderr);
    daemon.abort();
}

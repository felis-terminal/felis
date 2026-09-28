//! `felis sessions` integration tests: a real daemon in-process, the
//! `felis` binary driven against it, exit codes per
//! docs/reference/ipc.md "CLI clients".

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use felis_daemon::{
    SessionPool,
    serve::{DaemonCaps, SessionFactory, serve_unix_with_factory},
};
use felis_protocol::messages::SpawnArgs;
use felis_pty::Command as PtyCommand;
use tempfile::TempDir;
use tokio::sync::Mutex;

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/schema.rs"]
mod schema;

use fixtures::{
    create_and_detach, private_dir, quiet_factory, spawn_daemon, spawn_daemon_with_caps,
};

/// The FHS pair keeps first claim; the runner's own `PATH` is the tail
/// that lets a host with no FHS `/bin` (NixOS links only `sh` there)
/// resolve anything. Without it `sh` runs the body with the program
/// missing and reports nothing, so the test passes having exercised
/// nothing.
fn fixture_path() -> std::ffi::OsString {
    let mut path = std::ffi::OsString::from("/bin:/usr/bin");
    if let Some(host) = std::env::var_os("PATH") {
        path.push(":");
        path.push(host);
    }
    path
}

/// `window retarget` rejects the global `--socket`, so the child
/// reaches this daemon through the stamp every shell inside a felis
/// session carries: no variable moves the default endpoint any more.
async fn spawn_default_socket_daemon(
    tmp: &TempDir,
    factory: SessionFactory,
) -> (
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
    Arc<Mutex<SessionPool>>,
    (&'static str, std::path::PathBuf),
) {
    let path = tmp.path().join("stamped").join("daemon.sock");
    let stamp = ("FELIS_SOCKET", path.clone());
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let server_path = path.clone();
    let server_pool = pool.clone();
    let handle = tokio::spawn(async move {
        drop(
            serve_unix_with_factory(&server_path, DaemonCaps::default(), server_pool, factory)
                .await,
        );
    });
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            return (handle, path, pool, stamp);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("daemon on {} never accepted a connection", path.display());
}

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
        let serve_path = socket.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("daemon runtime");
            runtime.spawn(async move {
                let pool = Arc::new(Mutex::new(SessionPool::new()));
                drop(
                    serve_unix_with_factory(&serve_path, DaemonCaps::default(), pool, factory)
                        .await,
                );
            });
            let _stopped = stop_rx.recv();
            runtime.shutdown_timeout(Duration::from_millis(100));
        });
        for _ in 0..400 {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                return Self {
                    socket,
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

impl Drop for KillableDaemon {
    fn drop(&mut self) {
        self.kill();
    }
}

fn cli_bin() -> std::path::PathBuf {
    let bin = std::env::var("CARGO_BIN_EXE_felis").expect(
        "CARGO_BIN_EXE_felis is set by cargo when running this \
         integration test from the felis-cli crate",
    );
    std::path::PathBuf::from(bin)
}

/// `FELIS_SOCKET` outranks the platform default, so a run started inside
/// a felis window would otherwise address the developer's own daemon.
/// `RUST_LOG` outranks machine mode's `"off"` fallback directive, so a
/// developer or runner with it exported would otherwise put log lines
/// on the channel the one-object assertions count.
fn cli_command() -> StdCommand {
    let mut cmd = StdCommand::new(cli_bin());
    cmd.env_remove("FELIS_SOCKET");
    cmd.env_remove("RUST_LOG");
    cmd
}

/// The hidden completion helper lists the daemon an explicit
/// `--socket` names (the shell overlays forward only that flag).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_sessions_lists_the_local_socket_roster() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .arg("__complete-sessions")
        .output()
        .expect("run felis __complete-sessions");
    assert!(
        out.status.success(),
        "non-zero exit: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id_hex = format!("{id:032x}");
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with(&format!("{id_hex}\t"))),
        "expected a `<id>\\t<description>` line for {id_hex}, got: {stdout:?}"
    );

    server.abort();
}

/// Verify completion is local-only: `--host` on the line exits 0 with no candidates
/// and never starts `ssh`. The child's default socket is a live daemon to ensure
/// an empty candidate list is deliberate rather than a missing socket fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_sessions_with_a_host_emits_nothing_and_never_runs_ssh() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    create_and_detach(&socket).await;
    let marker = tmp.path().join("ssh-ran");
    let ssh = tmp.path().join("ssh");
    let mut f = std::fs::File::create(&ssh).unwrap();
    writeln!(f, "#!/bin/sh\ntouch '{}'", marker.display()).unwrap();
    drop(f);
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = tmp.path().as_os_str().to_owned();
    path.push(":");
    path.push(fixture_path());

    for args in [
        vec!["--host", "nowhere", "__complete-sessions"],
        vec![
            "--host",
            "nowhere",
            "--ssh-arg=-p",
            "--ssh-arg=2222",
            "__complete-sessions",
        ],
    ] {
        let started = std::time::Instant::now();
        let out = cli_command()
            .env("PATH", &path)
            .env("FELIS_SOCKET", &socket)
            .args(&args)
            .output()
            .expect("run felis __complete-sessions");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{args:?} took {:?}",
            started.elapsed()
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: status={:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            out.stdout.is_empty(),
            "{args:?} offered candidates: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(!marker.exists(), "{args:?} spawned ssh");
    }

    server.abort();
}

/// The auto-spawn matrix as `ssh` sees it (docs/reference/cli.md
/// "Auto-spawning"): asserted on the literal relay argv, because the
/// verb's policy has to survive the whole dial. The `ssh` stand-in on
/// `PATH` records its arguments before exiting, so the command line is
/// on disk even though the handshake then fails.
#[test]
fn the_ssh_relay_command_asks_for_a_spawn_only_for_the_spawning_verb() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = private_dir();
    let calls = tmp.path().join("ssh-argv");
    let ssh = tmp.path().join("ssh");
    let mut f = std::fs::File::create(&ssh).unwrap();
    writeln!(
        f,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'",
        calls.display()
    )
    .unwrap();
    drop(f);
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = tmp.path().as_os_str().to_owned();
    path.push(":");
    path.push(fixture_path());

    for (verb, expected) in [
        (
            ["sessions", "list"],
            "nowhere felis-daemon relay --no-spawn",
        ),
        (["sessions", "spawn"], "nowhere felis-daemon relay"),
    ] {
        drop(std::fs::remove_file(&calls));
        let out = cli_command()
            .env("PATH", &path)
            .args(["--host", "nowhere"])
            .args(verb)
            .output()
            .expect("run felis over the ssh carrier");
        assert!(
            !out.status.success(),
            "{verb:?}: the stand-in answers no handshake, so the verb must fail"
        );
        let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
        assert_eq!(
            recorded.lines().collect::<Vec<_>>(),
            [expected],
            "{verb:?} dialed the wrong relay command"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_prints_one_session_and_exits_zero() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "list"])
        .output()
        .expect("run felis sessions list");
    assert!(
        out.status.success(),
        "non-zero exit: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id_hex = format!("{id:032x}");
    let short = &id_hex[..8];
    assert!(
        stdout.contains(short),
        "list output missing session prefix {short}: {stdout}",
    );

    let out_json = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "list", "--format", "json"])
        .output()
        .expect("run felis sessions list --format json");
    assert!(
        out_json.status.success(),
        "sessions list --format json must exit 0 (the plain form above \
         is checked; a json run that prints and then exits non-zero \
         would pass a stdout-only oracle)",
    );
    let stdout_json = String::from_utf8_lossy(&out_json.stdout);
    let object = parse_point(&stdout_json, "sessions list");
    let sessions = object["sessions"]
        .as_array()
        .expect("the roster is one array in one object");
    assert_eq!(sessions.len(), 1, "{stdout_json}");
    let item = &sessions[0];
    assert_eq!(item["id"], id_hex.as_str(), "the full id is the machine id");
    assert_eq!(
        item["short_id"], short,
        "the display prefix rides beside the id: {stdout_json}"
    );
    assert_eq!(
        item["attachments"],
        serde_json::json!([]),
        "a parked session must still carry an iterable attachments array"
    );
    server.abort();
}

fn parse_jsonl(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| {
                panic!("every stdout line is one JSON object: {line} ({err})")
            })
        })
        .collect()
}

fn assert_all_versioned(objects: &[serde_json::Value], verb: &str) {
    assert!(!objects.is_empty(), "{verb} emitted nothing");
    for object in objects {
        assert_eq!(
            object["v"], 1,
            "{verb} emitted an object with no v:1: {object}"
        );
        schema::assert_cli_object(object);
    }
}

fn parse_point(stdout: &str, verb: &str) -> serde_json::Value {
    let object: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("{verb} must emit one JSON object: {stdout} ({err})"));
    assert_eq!(object["v"], 1, "{verb} emitted an object with no v:1");
    schema::assert_cli_object(&object);
    object
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_against_cold_socket_exits_two_without_autospawn() {
    let tmp = private_dir();
    let socket = tmp.path().join("nonexistent.sock");
    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "list"])
        .output()
        .expect("run felis sessions list");
    assert_eq!(
        out.status.code(),
        Some(2),
        "cold socket must exit 2; got {:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !socket.exists(),
        "ops must not auto-spawn the daemon on a cold socket",
    );
}

/// Without it, "no daemon" and "nothing to say" are the same empty
/// stdout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_that_never_opens_still_emits_its_error_terminal() {
    let tmp = private_dir();
    let socket = tmp.path().join("nonexistent.sock");
    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "capture", "1a", "--format", "jsonl"])
        .output()
        .expect("run felis sessions capture --format jsonl");
    assert_eq!(
        out.status.code(),
        Some(2),
        "cold socket must exit 2; got {:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let objects = parse_jsonl(&stdout);
    assert_all_versioned(&objects, "sessions capture --format jsonl");
    assert_eq!(objects.len(), 1, "only the terminal: {stdout}");
    assert_eq!(objects[0]["event"], "error");
    assert_eq!(objects[0]["error"]["kind"], "daemon_unreachable");
    assert!(
        objects[0]["error"]["message"].is_string(),
        "the terminal carries a human message too: {stdout}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_point_verbs_failure_is_one_typed_error_on_stderr() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "info", "--format", "json", "deadbeef"])
        .output()
        .expect("run felis sessions info --format json");
    assert_eq!(
        out.status.code(),
        Some(1),
        "an unresolvable prefix is a domain failure: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "a failed point verb writes no result to stdout",
    );
    let error = parse_point(&String::from_utf8_lossy(&out.stderr), "sessions info");
    assert_eq!(error["error"]["kind"], "no_match");
    assert!(error["error"]["message"].is_string());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_prints_session_metadata() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");
    let prefix = &id_hex[..8];

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "info", prefix])
        .output()
        .expect("run felis sessions info");
    assert!(
        out.status.success(),
        "non-zero: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("id:"),
        "info output missing label: {stdout}"
    );
    assert!(
        stdout.contains(&id_hex),
        "info output missing full id: {stdout}",
    );
    assert!(stdout.contains("size:"), "info missing size label");

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_removes_session_and_second_call_exits_one() {
    let tmp = private_dir();
    let (server, socket, pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "kill", &id_hex])
        .output()
        .expect("run felis sessions kill");
    assert!(
        out.status.success(),
        "first kill must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        pool.lock().await.get(felis_daemon::SessionId(id)).is_none(),
        "session must be gone from the pool after kill",
    );

    let out2 = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "kill", &id_hex])
        .output()
        .expect("run felis sessions kill again");
    assert_eq!(
        out2.status.code(),
        Some(1),
        "second kill on missing id must exit 1: status={:?} stderr={}",
        out2.status,
        String::from_utf8_lossy(&out2.stderr),
    );
    let stderr = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr.contains("no session"),
        "missing-session stderr expected; got: {stderr}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_without_text_or_dash_is_a_usage_error_exit_two() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", "deadbeef"])
        .output()
        .expect("run felis sessions send with no payload");
    assert_eq!(
        out.status.code(),
        Some(2),
        "send with no <text>/`-` must exit 2 (usage): status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("<TEXT>"),
        "expected clap to name the missing argument; got: {stderr}",
    );

    // A refusal clap can state is stated by clap: argument parsing
    // precedes format selection, so `--format json` yields no machine
    // object.
    let framed = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", "--format", "json", "deadbeef"])
        .output()
        .expect("run felis sessions send --format json with no payload");
    assert_eq!(
        framed.status.code(),
        Some(2),
        "a usage error keeps its exit code under --format json: stderr={}",
        String::from_utf8_lossy(&framed.stderr),
    );
    assert!(
        framed.stdout.is_empty(),
        "a usage error writes nothing to stdout: {}",
        String::from_utf8_lossy(&framed.stdout),
    );
    let framed_stderr = String::from_utf8_lossy(&framed.stderr);
    for line in framed_stderr.lines() {
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_err(),
            "a usage error is a human message, never a machine object: {line}",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_empty_payload_still_resolves_the_session_id() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let ok = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", &id_hex, "-"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run felis sessions send - with empty stdin");
    assert_eq!(
        ok.status.code(),
        Some(0),
        "empty paste to a valid session stays 0: status={:?} stderr={}",
        ok.status,
        String::from_utf8_lossy(&ok.stderr),
    );

    let unknown = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", "deadbeef", "-"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run felis sessions send - unknown id");
    assert_eq!(
        unknown.status.code(),
        Some(1),
        "empty paste to an unknown session must still exit 1: status={:?} stderr={}",
        unknown.status,
        String::from_utf8_lossy(&unknown.stderr),
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_returns_zero_on_detached_session() {
    let tmp = private_dir();
    // Exit 0 alone cannot catch `send` quitting mid-rehydrate-burst,
    // where the daemon's pump dies on EPIPE before forwarding the
    // input; the echo proves delivery.
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args(["-c", "read x; printf 'got=%s' \"$x\"; read _y"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", &id_hex, "PROBE-12345\n", "--raw"])
        .output()
        .expect("run felis sessions send");
    assert!(
        out.status.success(),
        "send must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    // Let the parked drain land the echo in the grid.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let cap = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "capture", &id_hex])
        .output()
        .expect("run felis sessions capture");
    let stdout = String::from_utf8_lossy(&cap.stdout);
    assert!(
        stdout.contains("got=PROBE-12345"),
        "sent bytes must reach the shell; grid was: {stdout}",
    );

    server.abort();
}

/// Writing the frames is not delivering them. While a session's input
/// budget is full the daemon's pump parks mid-message, and a peer that
/// closes its socket there is noticed and its admitted-but-unwritten
/// input abandoned; a `send` that exits as soon as it has written
/// reports success for bytes no child ever sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_waits_for_admission_rather_than_dropping_its_input() {
    use felis_protocol::limits::MAX_PASTE_BYTES;
    use std::io::Write;
    use std::process::Stdio;

    // Longer than either send needs, so the second one is written and
    // parked while the budget is still full.
    const STALL_SECS: u64 = 8;
    // Past the 64 bytes the first payload leaves unreserved, so this one
    // cannot slip into the budget beside it.
    const MARKER: &str = "MARKER-0123456789-0123456789-0123456789-0123456789-\
0123456789-0123456789-0123456789";

    let tmp = private_dir();
    let received = tmp.path().join("received");
    let factory: SessionFactory = {
        let received = received.clone();
        Arc::new(move |_| {
            let mut cmd = PtyCommand::new("/bin/sh");
            cmd.args([
                "-c",
                &format!(
                    "stty raw -echo; sleep {STALL_SECS}; exec cat > {}",
                    received.display()
                ),
            ]);
            cmd.env_clear();
            cmd.env("PATH", fixture_path());
            cmd
        })
    };
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    // Fills the session's budget against a child that is not reading.
    let mut filler = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", &id_hex, "-", "--raw"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run felis sessions send -");
    let mut filler_stdin = filler.stdin.take().expect("piped stdin");
    let feed = std::thread::spawn(move || {
        drop(filler_stdin.write_all(&vec![b'x'; MAX_PASTE_BYTES]));
    });

    // Only once the budget is charged is the marker below the parked
    // case this test is about.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = cli_command()
            .arg("--socket")
            .arg(&socket)
            .args(["daemon", "status"])
            .output()
            .expect("run felis daemon status");
        let stdout = String::from_utf8_lossy(&status.stdout);
        let row = stdout
            .lines()
            .find(|line| line.starts_with("pty_input_bytes"))
            .unwrap_or_default()
            .to_owned();
        if !row.contains("0 B  /") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the filler never charged the budget: {row}",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let marker = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", &id_hex, MARKER, "--raw"])
        .output()
        .expect("run felis sessions send");
    assert!(
        marker.status.success(),
        "send must succeed: status={:?} stderr={}",
        marker.status,
        String::from_utf8_lossy(&marker.stderr),
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if std::fs::read(&received).is_ok_and(|bytes| bytes.ends_with(MARKER.as_bytes())) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "send reported success but the marker never reached the child",
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    drop(filler.wait());
    drop(feed.join());
    server.abort();
}

/// The admission wait is the backpressure, so `--timeout` has to bound
/// it: against a child that never reads its stdin the wait cannot end
/// on its own, and an unbounded confirmation would hang the agent that
/// asked for a bounded failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_wait_timeout_bounds_the_admission_of_a_wedged_child() {
    use felis_protocol::limits::MAX_PASTE_BYTES;
    use std::io::Write;
    use std::process::Stdio;

    // Past the 64 bytes the filler leaves unreserved, so this payload
    // cannot slip into the budget beside it.
    const PAYLOAD: &str = "PAYLOAD-0123456789-0123456789-0123456789-0123456789-\
0123456789-0123456789-0123456789";

    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        // Never reads its stdin: the budget it fills is never released.
        cmd.args(["-c", "stty raw -echo; sleep 3600"]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let mut filler = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", &id_hex, "-", "--raw"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run felis sessions send -");
    let mut filler_stdin = filler.stdin.take().expect("piped stdin");
    let feed = std::thread::spawn(move || {
        drop(filler_stdin.write_all(&vec![b'x'; MAX_PASTE_BYTES]));
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = cli_command()
            .arg("--socket")
            .arg(&socket)
            .args(["daemon", "status"])
            .output()
            .expect("run felis daemon status");
        let stdout = String::from_utf8_lossy(&status.stdout);
        let row = stdout
            .lines()
            .find(|line| line.starts_with("pty_input_bytes"))
            .unwrap_or_default()
            .to_owned();
        if !row.contains("0 B  /") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the filler never charged the budget: {row}",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let started = std::time::Instant::now();
    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "send",
            &id_hex,
            PAYLOAD,
            "--wait",
            "--timeout",
            "2",
        ])
        .output()
        .expect("run felis sessions send --wait --timeout 2");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a bounded wait against a wedged child exits 1: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("did not take the input"),
        "stderr must name the admission timeout: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the wait must end on its deadline, not on the child",
    );

    drop(filler.kill());
    drop(filler.wait());
    drop(feed.join());
    server.abort();
}

/// `--timeout 0` is a deadline that has already passed: the admission
/// confirmation gets no chance to complete, so the verb exits 1 under
/// `timeout` rather than treating `0` as unbounded or as a usage error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_wait_timeout_zero_gives_up_at_once() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        // Never reads its stdin, so nothing can satisfy the wait early.
        cmd.args(["-c", "stty raw -echo; sleep 3600"]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "send",
            &id_hex,
            "PAYLOAD",
            "--wait",
            "--timeout",
            "0",
            "--format",
            "json",
        ])
        .output()
        .expect("run felis sessions send --wait --timeout 0");
    assert_eq!(
        out.status.code(),
        Some(1),
        "--timeout 0 exits 1: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let error = parse_point(&String::from_utf8_lossy(&out.stderr), "sessions send");
    assert_eq!(error["error"]["kind"], "timeout");

    server.abort();
}

/// `read` returns only on a line end, so the banner proves the Enter
/// frame followed the paste.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_key_enter_delivers_text_and_enter_in_one_call() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args(["-c", "read x; printf 'got=%s' \"$x\"; read _y"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    // No newline in the text: the Enter comes from `--key`.
    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", &id_hex, "SUBMIT-99", "--key", "enter"])
        .output()
        .expect("run felis sessions send --key enter");
    assert!(
        out.status.success(),
        "send --key enter must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    tokio::time::sleep(Duration::from_millis(400)).await;
    let cap = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "capture", &id_hex])
        .output()
        .expect("run felis sessions capture");
    let stdout = String::from_utf8_lossy(&cap.stdout);
    assert!(
        stdout.contains("got=SUBMIT-99"),
        "--key enter must terminate the shell's read; grid was: {stdout}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_renders_visible_grid_rows() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf 'CAPTURE-LINE-ONE\\r\\nCAPTURE-LINE-TWO\\r\\n'; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    // Let the parked drain land the banner in the grid.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "capture", &id_hex])
        .output()
        .expect("run felis sessions capture");
    assert!(
        out.status.success(),
        "capture must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("CAPTURE-LINE-ONE"),
        "capture output missing line 1: {stdout}",
    );
    assert!(
        stdout.contains("CAPTURE-LINE-TWO"),
        "capture output missing line 2: {stdout}",
    );
    let line_count = stdout.lines().count();
    assert!(
        line_count >= 24,
        "capture must emit one line per grid row (≥24); got {line_count}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_scrollback_includes_offscreen_history() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        // 50 lines overflow the 24-row grid into scrollback.
        cmd.args([
            "-c",
            "i=1; while [ $i -le 50 ]; do printf 'SB-LINE-%03d\\r\\n' $i; i=$((i+1)); done; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    // Let the parked drain land all 50 lines.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let plain = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "capture", &id_hex])
        .output()
        .expect("run felis sessions capture");
    assert!(plain.status.success(), "plain capture must succeed");
    let plain_out = String::from_utf8_lossy(&plain.stdout);
    assert!(
        !plain_out.contains("SB-LINE-001"),
        "line 1 should have scrolled off the visible grid: {plain_out}",
    );

    let scrolled = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "capture", &id_hex, "--source", "scrollback"])
        .output()
        .expect("run felis sessions capture --source scrollback");
    assert!(
        scrolled.status.success(),
        "scrollback capture must succeed: status={:?} stderr={}",
        scrolled.status,
        String::from_utf8_lossy(&scrolled.stderr),
    );
    let scrolled_out = String::from_utf8_lossy(&scrolled.stdout);
    assert!(
        scrolled_out.contains("SB-LINE-001"),
        "scrollback capture missing the off-screen first line: {scrolled_out}",
    );
    assert!(
        scrolled_out.contains("SB-LINE-050"),
        "scrollback capture missing the last live line: {scrolled_out}",
    );
    let pos1 = scrolled_out.find("SB-LINE-001").unwrap();
    let pos50 = scrolled_out.find("SB-LINE-050").unwrap();
    assert!(pos1 < pos50, "scrollback must print oldest-first");

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_lines_tails_the_region_and_keeps_row_indices() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "i=1; while [ $i -le 50 ]; do printf 'SB-LINE-%03d\\r\\n' $i; i=$((i+1)); done; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let capture_json = |extra: Vec<&str>| {
        let mut args = vec!["sessions", "capture", &id_hex, "--format", "jsonl"];
        args.extend(extra);
        let out = cli_command()
            .arg("--socket")
            .arg(&socket)
            .args(&args)
            .output()
            .expect("run felis sessions capture --lines");
        assert!(
            out.status.success(),
            "capture --lines must succeed: status={:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // A tail inside the visible grid keeps live indices: a filter, not
    // a renumbering.
    let tail5 = capture_json(vec!["--source", "scrollback", "--lines", "5"]);
    let rows5 = tail5.lines().filter(|l| l.contains("\"text\"")).count();
    assert_eq!(rows5, 5, "tail 5 must emit 5 rows: {tail5}");
    for r in 19..=23 {
        assert!(
            tail5.contains(&format!("\"row\":{r},")),
            "tail 5 must keep live index {r}: {tail5}",
        );
    }
    assert!(
        tail5.contains("\"count\":5"),
        "terminator must count the tail, not the region: {tail5}",
    );

    // 30 rows = 24 live + 6 scrollback.
    let tail30 = capture_json(vec!["--source", "scrollback", "--lines", "30"]);
    let rows30 = tail30.lines().filter(|l| l.contains("\"text\"")).count();
    assert_eq!(rows30, 30, "tail 30 must emit 30 rows: {tail30}");
    assert!(
        tail30.contains("\"row\":-1,") && tail30.contains("\"row\":-6,"),
        "tail 30 must keep the negative scrollback indices: {tail30}",
    );
    assert!(
        !tail30.contains("\"row\":-7,"),
        "tail 30 must not ship scrollback beyond the cut: {tail30}",
    );

    let visible3 = capture_json(vec!["--lines", "3"]);
    let rows3 = visible3.lines().filter(|l| l.contains("\"text\"")).count();
    assert_eq!(rows3, 3, "visible tail must emit 3 rows: {visible3}");
    assert!(
        visible3.contains("\"row\":23,"),
        "visible tail must end at the bottom grid row: {visible3}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_ansi_reconstructs_sgr_color() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf '\\033[31mANSI-RED-LINE\\033[0m\\r\\n'; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let ansi = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "capture", &id_hex, "--ansi"])
        .output()
        .expect("run felis sessions capture --ansi");
    assert!(
        ansi.status.success(),
        "ansi capture must succeed: status={:?} stderr={}",
        ansi.status,
        String::from_utf8_lossy(&ansi.stderr),
    );
    let ansi_out = String::from_utf8_lossy(&ansi.stdout);
    assert!(
        ansi_out.contains("ANSI-RED-LINE"),
        "ansi capture missing the banner text: {ansi_out:?}",
    );
    assert!(
        ansi_out.contains("\x1b[31m"),
        "ansi capture missing the red SGR run: {ansi_out:?}",
    );

    let plain = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "capture", &id_hex])
        .output()
        .expect("run felis sessions capture");
    let plain_out = String::from_utf8_lossy(&plain.stdout);
    assert!(
        plain_out.contains("ANSI-RED-LINE"),
        "plain capture missing the banner text: {plain_out:?}",
    );
    assert!(
        !plain_out.contains('\x1b'),
        "plain capture must carry no escape bytes: {plain_out:?}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_ansi_json_carries_both_forms_per_row() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf '\\033[31mANSI-RED-LINE\\033[0m\\r\\n'; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions", "capture", &id_hex, "--ansi", "--format", "jsonl",
        ])
        .output()
        .expect("run felis sessions capture --ansi --format jsonl");
    assert!(
        out.status.success(),
        "capture --ansi --format jsonl must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let banner = stdout
        .lines()
        .find(|l| l.contains("ANSI-RED-LINE"))
        .unwrap_or_else(|| panic!("no row carried the banner: {stdout}"));
    // ESC is JSON-escaped as `\u001b`.
    let (text_half, ansi_half) = banner
        .split_once("\"ansi\":")
        .unwrap_or_else(|| panic!("row carries no ansi field: {banner}"));
    assert!(
        !text_half.contains("\\u001b"),
        "the text field must stay plain: {banner}",
    );
    assert!(
        ansi_half.contains("\\u001b[31m"),
        "the ansi field must carry the red SGR run: {banner}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_ansi_json_composes_on_a_mark_range_source() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf '\\033]133;A\\007$ \\033]133;B\\007echo hi\\r\\n\
             \\033]133;C\\007\\033[31mRED-133\\033[0m\\r\\n\\033]133;D;0\\007'; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "capture",
            &id_hex,
            "--ansi",
            "--format",
            "jsonl",
            "--source",
            "command-output",
        ])
        .output()
        .expect("run felis sessions capture --ansi --format jsonl --source command-output");
    assert!(
        out.status.success(),
        "the pair must succeed on a mark-range source: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"text\":\"RED-133\""),
        "plain text field must stay escape-free: {stdout:?}",
    );
    assert!(
        stdout.contains("\"ansi\":\"\\u001b["),
        "the ansi field must carry the SGR reconstruction: {stdout:?}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_source_command_output_returns_the_c_to_d_range() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf '\\033]133;A\\007$ \\033]133;B\\007echo hi\\r\\n\
             \\033]133;C\\007CMDOUT-133\\r\\n\\033]133;D;0\\007'; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    // Let the parked drain land the marks in the grid.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "capture", &id_hex, "--source", "command-output"])
        .output()
        .expect("run felis sessions capture --source command-output");
    assert!(
        out.status.success(),
        "capture --source command-output must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("CMDOUT-133"),
        "command-output must contain the C→D output: {stdout:?}",
    );
    assert!(
        !stdout.contains("echo hi"),
        "command-output must exclude the command line: {stdout:?}",
    );

    let info = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "info", &id_hex, "--format", "json"])
        .output()
        .expect("run felis sessions info --format json");
    let info_out = String::from_utf8_lossy(&info.stdout);
    assert!(
        info_out.contains("\"last_exit_code\":0"),
        "info --format json must surface the D mark's exit code: {info_out}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_json_semantic_source_emits_region_relative_rows() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf '\\033]133;A\\007$ \\033]133;B\\007echo hi\\r\\n\
             \\033]133;C\\007CMDOUT-133\\r\\n\\033]133;D;0\\007'; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "capture",
            &id_hex,
            "--source",
            "last-command",
            "--format",
            "jsonl",
        ])
        .output()
        .expect("run felis sessions capture --source last-command --format jsonl");
    assert!(
        out.status.success(),
        "capture --source last-command --format jsonl must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"row\":0"),
        "semantic jsonl must start at region-relative row 0: {stdout:?}",
    );
    assert!(
        stdout.contains("CMDOUT-133"),
        "semantic jsonl must carry the command output text: {stdout:?}",
    );
    assert!(
        stdout.contains("\"soft_wrap_continued\":false"),
        "stitched region rows are never continuations: {stdout:?}",
    );
    let done = stdout.lines().last().expect("at least the terminal line");
    assert!(
        done.starts_with("{\"v\":1,\"event\":\"end\",\"count\":") && !done.contains("\"count\":0"),
        "semantic jsonl must close with a non-empty end terminal: {done:?}",
    );
    assert!(
        done.contains("\"exit_code\":0"),
        "the terminal must carry the D;0 mark's exit code: {done:?}",
    );

    server.abort();
}

/// The printed `1` is `false`'s live D mark; the connection subscribes
/// before the input, so even an instant command cannot slip past.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_wait_prints_the_commands_exit_code() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf '\\033]133;A\\007$ '; read x; eval \"$x\"; \
             printf '\\033]133;D;%d\\007' \"$?\"; read _y",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "send",
            &id_hex,
            "false",
            "--key",
            "enter",
            "--wait",
            "--timeout",
            "30",
        ])
        .output()
        .expect("run felis sessions send --key enter --wait");
    assert!(
        out.status.success(),
        "send --wait must exit 0 on an observed D mark: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "1",
        "stdout must be `false`'s exit code",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_wait_json_emits_the_wait_shape() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf '\\033]133;A\\007$ '; read x; eval \"$x\"; \
             printf '\\033]133;D;%d\\007' \"$?\"; read _y",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "send",
            &id_hex,
            "false",
            "--key",
            "enter",
            "--wait",
            "--timeout",
            "30",
            "--format",
            "json",
        ])
        .output()
        .expect("run felis sessions send --key enter --wait --format json");
    assert!(
        out.status.success(),
        "send --wait --format json must exit 0: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let waited = parse_point(
        &String::from_utf8_lossy(&out.stdout),
        "sessions send --wait",
    );
    assert_eq!(waited["id"], id_hex.as_str());
    assert_eq!(
        waited["exit_code"], 1,
        "stdout must carry the awaited mark's code",
    );

    server.abort();
}

/// The rehydrate burst replays historical marks; a wait that adopted
/// one would return the previous command's code on every idle session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payload_less_wait_ignores_historical_marks_and_times_out() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args(["-c", "printf '\\033]133;D;7\\007'; read x"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    // Let the D;7 land in the grid so it is historical when the wait
    // attaches.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", &id_hex, "--wait", "--timeout", "1"])
        .output()
        .expect("run felis sessions send --wait --timeout 1");
    assert_eq!(
        out.status.code(),
        Some(1),
        "wait must time out (exit 1), not adopt the historical D;7: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no command completed"),
        "stderr must name the timeout",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_creates_session_and_id_is_addressable() {
    let tmp = private_dir();
    let (server, socket, pool) = spawn_daemon(&tmp, quiet_factory()).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "spawn", "--", "/bin/sh", "-c", "read x"])
        .output()
        .expect("run felis sessions spawn");
    assert!(
        out.status.success(),
        "spawn must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let id_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        id_hex.len(),
        32,
        "stdout must be a 32-char hex id; got `{id_hex}`",
    );

    let id = u128::from_str_radix(&id_hex, 16).expect("hex parse");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        pool.lock().await.get(felis_daemon::SessionId(id)).is_some(),
        "spawned session must be in the pool under the printed id",
    );

    let list = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "list"])
        .output()
        .expect("run felis sessions list");
    assert!(
        String::from_utf8_lossy(&list.stdout).contains(&id_hex[..8]),
        "list must include the spawned session's short id",
    );

    let kill = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "kill", &id_hex])
        .output()
        .expect("run felis sessions kill");
    assert!(
        kill.status.success(),
        "kill of spawned session must succeed"
    );

    server.abort();
}

/// 3000 fits `u16` and 70000 does not; both must reach the daemon's
/// REQ-605a admission point rather than die at argument parsing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_with_out_of_range_rows_is_refused_by_the_daemon() {
    let tmp = private_dir();
    let (server, socket, pool) = spawn_daemon(&tmp, quiet_factory()).await;

    for rows in ["3000", "70000"] {
        let out = cli_command()
            .arg("--socket")
            .arg(&socket)
            .args(["sessions", "spawn", "--rows", rows, "--cols", "80"])
            .output()
            .expect("run felis sessions spawn --rows");
        assert_eq!(
            out.status.code(),
            Some(1),
            "--rows {rows} is a typed refusal the daemon answered, so exit 1: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!(
                "rows {rows} is outside the supported range 1..=2048"
            )),
            "--rows {rows} must report the daemon's typed refusal; got `{stderr}`",
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "a refused spawn must print no session id",
        );
    }
    assert_eq!(
        pool.lock().await.ids().count(),
        0,
        "a refused spawn must not leave a session behind",
    );

    server.abort();
}

/// Geometry is a pair: a create asks for the daemon's default by
/// naming neither axis, so half a grid is refused at the flag layer,
/// before anything is dialed.
#[test]
fn spawn_with_only_one_geometry_flag_is_a_usage_error() {
    for argv in [
        ["sessions", "spawn", "--rows", "40"],
        ["sessions", "spawn", "--cols", "132"],
    ] {
        let out = cli_command()
            .arg("--socket")
            .arg("/tmp/felis-absent.sock")
            .args(argv)
            .output()
            .expect("run felis sessions spawn with half a geometry");
        assert_eq!(
            out.status.code(),
            Some(2),
            "{argv:?} is a usage error: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("required arguments were not provided"),
            "{argv:?} must name the missing flag; got `{}`",
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            out.stdout.is_empty(),
            "a refused spawn must print no session id",
        );
    }
}

/// Exit 1, not 2: the daemon answered "full", and the remedy (reap,
/// then retry) is a script's to take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_past_the_session_cap_is_typed_at_capacity() {
    let tmp = private_dir();
    let (server, socket, pool) = spawn_daemon_with_caps(
        &tmp,
        quiet_factory(),
        DaemonCaps {
            max_sessions: 1,
            ..DaemonCaps::default()
        },
    )
    .await;

    let first = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "spawn"])
        .output()
        .expect("run felis sessions spawn");
    assert!(
        first.status.success(),
        "the first spawn fills the cap: stderr={}",
        String::from_utf8_lossy(&first.stderr),
    );

    let refused = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "spawn", "--format", "json"])
        .output()
        .expect("run felis sessions spawn --format json");
    assert_eq!(
        refused.status.code(),
        Some(1),
        "a full daemon is a refused-but-well-formed request: stderr={}",
        String::from_utf8_lossy(&refused.stderr),
    );
    assert!(
        refused.stdout.is_empty(),
        "a refused spawn prints no id object",
    );
    let stderr = String::from_utf8(refused.stderr).unwrap();
    let line = stderr
        .lines()
        .find(|l| l.starts_with('{'))
        .expect("a machine error object");
    let object: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(object["error"]["kind"], "at_capacity");
    let message = object["error"]["message"].as_str().unwrap();
    assert!(message.contains("at 1 of 1 sessions"), "{message}");

    assert_eq!(
        pool.lock().await.ids().count(),
        1,
        "the refused spawn left the pool at the cap",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_tag_labels_the_session_at_creation() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions", "spawn", "--tag", "agent", "--tag", "build", "--", "/bin/sh", "-c",
            "read x",
        ])
        .output()
        .expect("run felis sessions spawn --tag");
    assert!(
        out.status.success(),
        "spawn --tag must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let id_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();

    let info = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "info", &id_hex, "--format", "json"])
        .output()
        .expect("run felis sessions info --format json");
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(
        stdout.contains("\"tags\":[\"agent\",\"build\"]"),
        "info --format json must carry the creation tags sorted; got {stdout}",
    );

    let hit = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "list", "--tag", "agent"])
        .output()
        .expect("run felis sessions list --tag agent");
    assert!(
        String::from_utf8_lossy(&hit.stdout).contains(&id_hex[..8]),
        "list --tag agent must include the spawned session",
    );
    let miss = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "list", "--tag", "other"])
        .output()
        .expect("run felis sessions list --tag other");
    assert!(
        !String::from_utf8_lossy(&miss.stdout).contains(&id_hex[..8]),
        "list --tag other must not include the spawned session",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_json_emits_id_object() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions", "spawn", "--format", "json", "--", "/bin/sh", "-c", "read x",
        ])
        .output()
        .expect("run felis sessions spawn --format json");
    assert!(
        out.status.success(),
        "spawn --format json must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let object: serde_json::Value =
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("spawn json {line:?}: {err}"));
    assert_eq!(object["v"], 1, "every machine object carries the version");
    let hex = object["id"].as_str().expect("id string");
    assert_eq!(hex.len(), 32, "id must be 32-char hex; got `{hex}`");
    u128::from_str_radix(hex, 16).expect("hex parse");
    assert!(
        object.get("short_id").is_none(),
        "a rosterless reply must not claim a unique prefix: {line:?}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tag_json_emits_id_and_tag_array_for_adds_and_removes() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let tag = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions", "tag", "--format", "json", &id_hex, "work", "agent",
        ])
        .output()
        .expect("run felis sessions tag --format json");
    assert!(
        tag.status.success(),
        "tag --format json must succeed: status={:?} stderr={}",
        tag.status,
        String::from_utf8_lossy(&tag.stderr),
    );
    let tag_line = String::from_utf8_lossy(&tag.stdout).trim().to_string();
    let tagged = parse_point(&tag_line, "sessions tag");
    assert_eq!(tagged["id"], id_hex.as_str());
    assert_eq!(tagged["tags"], serde_json::json!(["agent", "work"]));

    let relabel = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions", "tag", "--format", "json", &id_hex, "done", "--remove", "agent",
        ])
        .output()
        .expect("run felis sessions tag --format json with adds and removes");
    assert!(
        relabel.status.success(),
        "tag --format json add+remove must succeed: status={:?} stderr={}",
        relabel.status,
        String::from_utf8_lossy(&relabel.stderr),
    );
    let relabeled = parse_point(
        String::from_utf8_lossy(&relabel.stdout).trim(),
        "sessions tag add+remove",
    );
    assert_eq!(relabeled["id"], id_hex.as_str());
    assert_eq!(
        relabeled["tags"],
        serde_json::json!(["done", "work"]),
        "one call must apply the add and the remove together",
    );

    let untag = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions", "tag", "--format", "json", &id_hex, "--remove", "work", "--remove", "done",
        ])
        .output()
        .expect("run felis sessions tag --format json --remove");
    assert!(
        untag.status.success(),
        "tag --remove --format json must succeed: status={:?} stderr={}",
        untag.status,
        String::from_utf8_lossy(&untag.stderr),
    );
    let untagged = parse_point(
        String::from_utf8_lossy(&untag.stdout).trim(),
        "sessions tag --remove",
    );
    assert_eq!(untagged["id"], id_hex.as_str());
    assert_eq!(
        untagged["tags"],
        serde_json::json!([]),
        "emptied tag set must still emit a present `tags` array",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_and_evict_json_emit_their_typed_replies() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let evict = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "evict", "--format", "json", &id_hex])
        .output()
        .expect("run felis sessions evict --format json");
    assert!(evict.status.success(), "evict --format json must exit 0");
    let evicted = parse_point(&String::from_utf8_lossy(&evict.stdout), "sessions evict");
    assert_eq!(evicted["id"], id_hex.as_str());
    assert_eq!(evicted["was_attached"], false);

    let kill = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "kill", "--format", "json", &id_hex])
        .output()
        .expect("run felis sessions kill --format json");
    assert!(kill.status.success(), "kill --format json must exit 0");
    let killed = parse_point(&String::from_utf8_lossy(&kill.stdout), "sessions kill");
    assert_eq!(killed["id"], id_hex.as_str());
    assert!(
        killed.get("was_attached").is_none(),
        "kill has no attachment verdict to report: {killed}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_returns_hits_and_no_match_exits_one() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "i=0; while [ $i -lt 50 ]; do printf 'entry-%s-PROBE\\r\\n' $i; i=$((i+1)); done; read x",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    // Let the parked drain land the rows in grid and scrollback.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "search", &id_hex, "PROBE"])
        .output()
        .expect("run felis sessions search");
    assert!(
        out.status.success(),
        "search must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hits = stdout.lines().count();
    assert!(
        hits > 0,
        "expected at least one PROBE hit; stdout:\n{stdout}",
    );
    assert!(
        stdout.contains("PROBE"),
        "stdout must echo the matched text; got: {stdout}",
    );

    // Let the search's detach land before the next attach.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let out_miss = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "search", &id_hex, "XYZ-NOPE-NEVER"])
        .output()
        .expect("run felis sessions search no-match");
    assert_eq!(
        out_miss.status.code(),
        Some(1),
        "no-match must exit 1: status={:?} stdout={} stderr={}",
        out_miss.status,
        String::from_utf8_lossy(&out_miss.stdout),
        String::from_utf8_lossy(&out_miss.stderr),
    );

    tokio::time::sleep(Duration::from_millis(150)).await;

    let out_json = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "search", &id_hex, "PROBE", "--format", "jsonl"])
        .output()
        .expect("run felis sessions search --format jsonl");
    let json_out = String::from_utf8_lossy(&out_json.stdout);
    let objects = parse_jsonl(&json_out);
    assert_all_versioned(&objects, "sessions search");
    assert!(
        json_out.contains("\"line_index\":"),
        "jsonl output missing line_index: {json_out}",
    );
    let terminal = objects.last().expect("at least the terminal object");
    assert_eq!(
        terminal["event"], "end",
        "jsonl output missing the end terminal: {json_out}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_against_unknown_id_exits_one() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "send", "deadbeef", "anything", "--raw"])
        .output()
        .expect("run felis sessions send");
    assert_eq!(
        out.status.code(),
        Some(1),
        "send on unknown id must exit 1: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no session matches"),
        "expected resolver error; got: {stderr}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_pushes_reattach_to_attached_client() {
    use felis_client_core::Offer;
    use felis_client_core::connect;
    use felis_protocol::{MessageKind, codec, messages::PushMsg};

    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let target = create_and_detach(&socket).await;
    // The window-mode connection is the one the daemon pushes to.
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "switch",
            &format!("{target:032x}"),
            "--format",
            "json",
        ])
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis sessions switch");
    assert!(
        out.status.success(),
        "switch must exit 0: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result = parse_point(&stdout, "sessions switch");
    assert_eq!(
        result["queued"], 1,
        "the reply reports queue admission for the one window; got: {stdout}",
    );

    let pushed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before Reattach");
            if frame.kind == MessageKind::Push.as_u16()
                && let Ok(PushMsg::Reattach { id }) = codec::decode::<PushMsg>(&frame.body)
            {
                return id;
            }
        }
    })
    .await
    .expect("no Reattach push within 5s");
    assert_eq!(pushed, target, "push must name the resolved target");

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_with_no_attached_client_exits_one() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let from = create_and_detach(&socket).await;
    let target = create_and_detach(&socket).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "switch",
            &format!("{target:032x}"),
            "--from",
            &format!("{from:032x}"),
        ])
        .output()
        .expect("run felis sessions switch");
    assert_eq!(
        out.status.code(),
        Some(1),
        "switch with nobody attached must exit 1: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("has no window to move"),
        "expected the no-input-owner message; got: {stderr}",
    );

    server.abort();
}

/// Ids are never reused, so a stale one can only mean "gone", never a
/// redirect to a surviving window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_with_an_unknown_attachment_exits_one() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let from = create_and_detach(&socket).await;
    let target = create_and_detach(&socket).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "switch",
            &format!("{target:032x}"),
            "--from",
            &format!("{from:032x}"),
            "--attachment",
            "4096",
        ])
        .output()
        .expect("run felis sessions switch --attachment");
    assert_eq!(
        out.status.code(),
        Some(1),
        "an unknown attachment must exit 1: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("attachment 4096 is no longer on session"),
        "expected the stale-attachment message; got: {stderr}",
    );

    server.abort();
}

/// The post-condition already holds, so no window is needed to push to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_to_the_same_session_exits_zero() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    // Parked: the shape the default scope would otherwise deny.
    let id = create_and_detach(&socket).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "switch",
            &format!("{id:032x}"),
            "--from",
            &format!("{id:032x}"),
            "--format",
            "json",
        ])
        .output()
        .expect("run felis sessions switch onto itself");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a same-session switch must exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result = parse_point(&stdout, "sessions switch onto itself");
    assert_eq!(
        result["queued"], 0,
        "nothing was pushed, and the count must say so; got: {stdout}",
    );

    server.abort();
}

/// Human framing must name queue admission rather than let exit 0 read
/// as "the window moved".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_human_line_says_queued_and_not_landed() {
    use felis_client_core::Offer;
    use felis_client_core::connect;

    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let target = create_and_detach(&socket).await;
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "switch", &format!("{target:032x}")])
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis sessions switch");
    assert!(
        out.status.success(),
        "switch must exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("queued on 1 window") && stdout.contains("not landed"),
        "the human line must report admission and deny landing; got: {stdout}",
    );

    drop(gui);
    server.abort();
}

/// The post-condition already held, so nothing was pushed and the line
/// must not claim a queue admission that did not happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_session_switch_human_line_says_nothing_was_queued() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "switch",
            &format!("{id:032x}"),
            "--from",
            &format!("{id:032x}"),
        ])
        .output()
        .expect("run felis sessions switch onto itself");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a same-session switch must exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("already on session {id:032x}"))
            && stdout.contains("nothing queued"),
        "the human line must say nothing was pushed; got: {stdout}",
    );

    server.abort();
}

/// Exit 0 claims queue admission alone: a window that takes the push
/// and then drops leaves the result standing with nothing attached to
/// the session it named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_that_disconnects_after_queue_admission_keeps_the_switch_result() {
    use felis_client_core::Offer;
    use felis_client_core::connect;
    use felis_protocol::{MessageKind, codec, messages::PushMsg};

    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let target = create_and_detach(&socket).await;
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "switch",
            &format!("{target:032x}"),
            "--format",
            "json",
        ])
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis sessions switch");
    assert_eq!(
        out.status.code(),
        Some(0),
        "switch must exit 0: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result = parse_point(&stdout, "sessions switch");
    assert_eq!(result["queued"], 1, "one outbox took the push: {stdout}");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before Reattach");
            if frame.kind == MessageKind::Push.as_u16()
                && matches!(
                    codec::decode::<PushMsg>(&frame.body),
                    Ok(PushMsg::Reattach { .. })
                )
            {
                return;
            }
        }
    })
    .await
    .expect("no Reattach push within 5s");
    // The window takes the push and goes away without attaching.
    drop(gui);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let info = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "info",
            &format!("{target:032x}"),
            "--format",
            "json",
        ])
        .output()
        .expect("run felis sessions info");
    let info_stdout = String::from_utf8_lossy(&info.stdout);
    let info = parse_point(&info_stdout, "sessions info");
    assert_eq!(
        info["attachments"],
        serde_json::json!([]),
        "the landing never happened, and exit 0 never promised it would: {info_stdout}",
    );

    server.abort();
}

/// The retarget's landing runs against a daemon that holds no record of
/// the request, so `queued` is all the source daemon can report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_daemon_retarget_reports_admission_without_landing() {
    use felis_client_core::Offer;
    use felis_client_core::connect;
    use felis_protocol::{MessageKind, codec, messages::PushMsg};

    let tmp = private_dir();
    let elsewhere = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let (other_server, other_socket, _other_pool) = spawn_daemon(&elsewhere, quiet_factory()).await;
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .args(["window", "retarget"])
        .arg(&other_socket)
        .args(["--format", "json"])
        .env("FELIS_SOCKET", &socket)
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis window retarget");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a cross-daemon retarget must exit 0 on admission: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result = parse_point(&stdout, "window retarget");
    assert_eq!(result["queued"], 1, "one outbox took the push: {stdout}");

    let target = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before RetargetHost");
            if frame.kind == MessageKind::Push.as_u16()
                && let Ok(PushMsg::RetargetHost { target, .. }) =
                    codec::decode::<PushMsg>(&frame.body)
            {
                return target;
            }
        }
    })
    .await
    .expect("no RetargetHost push within 5s");
    assert_eq!(
        target.carrier,
        felis_protocol::messages::RetargetCarrier::LocalEndpoint(
            other_socket.display().to_string()
        ),
    );

    let roster = cli_command()
        .arg("--socket")
        .arg(&other_socket)
        .args(["sessions", "list", "--format", "json"])
        .output()
        .expect("run felis sessions list on the target daemon");
    let roster_stdout = String::from_utf8_lossy(&roster.stdout);
    let roster = parse_point(&roster_stdout, "sessions list");
    assert_eq!(
        roster["sessions"],
        serde_json::json!([]),
        "the target daemon holds nothing: the exit code reported admission on the source \
         daemon only: {roster_stdout}",
    );

    drop(gui);
    other_server.abort();
    server.abort();
}

/// The retarget's human line differs from the switch's: the window
/// re-dials rather than re-attaching on the same carrier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retarget_human_line_says_queued_and_not_landed() {
    use felis_client_core::Offer;
    use felis_client_core::connect;

    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .args(["ssh", "user@box"])
        .env("FELIS_SOCKET", &socket)
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis ssh");
    assert_eq!(
        out.status.code(),
        Some(0),
        "felis ssh must exit 0 on admission: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("queued on 1 window")
            && stdout.contains("not landed")
            && stdout.contains("re-dials"),
        "the human line must report admission and deny landing; got: {stdout}",
    );

    drop(gui);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn window_retarget_without_a_carrier_heads_for_the_local_daemon() {
    use felis_client_core::Offer;
    use felis_client_core::connect;
    use felis_protocol::{MessageKind, codec, messages::PushMsg};

    let tmp = private_dir();
    let (server, socket, _pool, (env_var, env_value)) =
        spawn_default_socket_daemon(&tmp, quiet_factory()).await;
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .args(["window", "retarget"])
        .env(env_var, &env_value)
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis window retarget");
    assert_eq!(
        out.status.code(),
        Some(0),
        "no carrier must mean the default local daemon: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );

    let target = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before RetargetHost");
            if frame.kind == MessageKind::Push.as_u16()
                && let Ok(PushMsg::RetargetHost { target, .. }) =
                    codec::decode::<PushMsg>(&frame.body)
            {
                return target;
            }
        }
    })
    .await
    .expect("no RetargetHost push within 5s");
    assert_eq!(
        target.carrier,
        felis_protocol::messages::RetargetCarrier::DefaultLocal,
    );

    server.abort();
}

/// The verb refuses the global `--socket` and nothing else in a child
/// names the window's daemon, so on a non-default socket the stamp is
/// the only way there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_in_window_retarget_reaches_the_stamped_daemon() {
    use felis_client_core::Offer;
    use felis_client_core::connect;
    use felis_protocol::{MessageKind, codec, messages::PushMsg};

    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let mut gui = connect(&socket, Offer::window(false)).await.unwrap();
    let from = gui.create_with(SpawnArgs::default()).await.unwrap().id;

    let out = cli_command()
        .args(["ssh", "user@box"])
        .env("FELIS_SOCKET", &socket)
        .env("FELIS_SESSION_ID", format!("{from:032x}"))
        .output()
        .expect("run felis ssh");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the stamp must name the daemon this window's session lives on: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );

    let target = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = gui
                .reader
                .next_frame()
                .await
                .expect("read frame")
                .expect("daemon closed before RetargetHost");
            if frame.kind == MessageKind::Push.as_u16()
                && let Ok(PushMsg::RetargetHost { target, .. }) =
                    codec::decode::<PushMsg>(&frame.body)
            {
                return target;
            }
        }
    })
    .await
    .expect("no RetargetHost push within 5s");
    assert_eq!(
        target.carrier,
        felis_protocol::messages::RetargetCarrier::Ssh {
            destination: "user@box".to_string(),
            ssh_args: Vec::new(),
        },
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicit_socket_outranks_the_stamp() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "list"])
        .env("FELIS_SOCKET", tmp.path().join("no-daemon-here.sock"))
        .output()
        .expect("run felis sessions list");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the flag must outrank the stamp: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );

    server.abort();
}

/// The stderr assertion pins the guard: exit 2 also spells
/// "unreachable", which every argv here would produce with the check
/// deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_global_carrier_on_a_retarget_is_a_usage_error() {
    for argv in [
        &["--socket", "/tmp/felis-absent.sock", "window", "retarget"][..],
        &["--host", "elsewhere", "ssh", "vm"],
    ] {
        let out = cli_command()
            .args(argv)
            .output()
            .expect("run the retarget verb");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{argv:?} must exit 2: stderr={stderr}"
        );
        assert!(
            stderr.contains("--host/--socket/--ssh-arg")
                && stderr.contains("addressed to the window's own daemon"),
            "{argv:?} must be refused by the global-carrier guard, not by a failed dial: \
             stderr={stderr}",
        );
    }
}

/// Refused before any lookup, so the command need not exist, and
/// `--config` is refused before it is resolved: a path that does not
/// exist still fails naming the command, never as a missing file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_global_before_an_external_command_is_a_usage_error() {
    for (argv, global) in [
        (
            &["--socket", "/tmp/felis-absent.sock", "absent"][..],
            "--socket",
        ),
        (&["--host", "elsewhere", "absent"], "--host"),
        (
            &["--host", "elsewhere", "--ssh-arg=-p", "absent"],
            "--host/--ssh-arg",
        ),
        (
            &["--config", "/nonexistent/work.toml", "absent"],
            "--config",
        ),
    ] {
        let out = cli_command()
            .args(argv)
            .output()
            .expect("run the external command");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{argv:?} must exit 2: stderr={stderr}"
        );
        assert!(
            stderr.contains(&format!("({global})")) && stderr.contains("felis-absent"),
            "{argv:?} must be refused by name: stderr={stderr}",
        );
    }
}

/// The two grammar-printing verbs dial nothing, so a carrier global is
/// refused before any output reaches stdout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_global_carrier_on_a_grammar_verb_is_a_usage_error() {
    let tmp = private_dir();
    let man = tmp.path().join("man");
    let man = man.to_str().expect("a utf-8 temp path");
    for (argv, verb) in [
        (
            &["--socket", "/tmp/felis-absent.sock", "completions", "fish"][..],
            "felis completions",
        ),
        (
            &["--host", "elsewhere", "completions", "fish"],
            "felis completions",
        ),
        (
            &["--host", "elsewhere", "--ssh-arg=-p", "__mangen", man],
            "felis __mangen",
        ),
        (
            &["--socket", "/tmp/felis-absent.sock", "__mangen", man],
            "felis __mangen",
        ),
    ] {
        let out = cli_command().args(argv).output().expect("run the verb");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{argv:?} must exit 2: stderr={stderr}"
        );
        assert!(
            out.stdout.is_empty(),
            "{argv:?} must print nothing: {out:?}"
        );
        assert!(
            stderr.contains("--host/--socket/--ssh-arg") && stderr.contains(verb),
            "{argv:?} must be refused by name: stderr={stderr}",
        );
    }
}

/// A `PATH` holding only `dir`, so the lookup sees the fixtures and
/// nothing ambient.
fn external_command(dir: &std::path::Path) -> StdCommand {
    let mut cmd = cli_command();
    cmd.env("PATH", dir);
    cmd
}

fn place_script(dir: &std::path::Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write the script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make the script executable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_external_command_receives_the_remaining_arguments_verbatim() {
    let tmp = private_dir();
    place_script(
        tmp.path(),
        "felis-echo",
        "#!/bin/sh\nprintf '%s\\n' \"$@\"\n",
    );
    let out = external_command(tmp.path())
        .args(["echo", "a b", "--format", "json", "--trace-perf"])
        .output()
        .expect("run the external command");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "a b\n--format\njson\n--trace-perf\n"
    );

    let out = external_command(tmp.path())
        .args(["help", "echo", "sub"])
        .output()
        .expect("run help for the external command");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "sub\n--help\n");

    let out = external_command(tmp.path())
        .args(["--trace-perf", "echo"])
        .output()
        .expect("run the external command");
    assert_eq!(
        out.status.code(),
        Some(2),
        "root --trace-perf must be an unknown argument: {out:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_external_command_is_a_usage_error_naming_the_similar_verb() {
    let tmp = private_dir();
    let out = external_command(tmp.path())
        .args(["session", "list"])
        .output()
        .expect("run the misspelled verb");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr={stderr}");
    assert!(
        stderr.contains("no `felis-session` beside felis or on PATH")
            && stderr.contains("a similar subcommand exists: 'sessions'"),
        "stderr={stderr}",
    );
}

/// An execute bit this process holds no right to (here: others only,
/// on a file the test owns) is `EACCES` at exec time, and the lookup
/// moves on to the next directory as `execvp` would.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_external_command_this_process_may_not_run_yields_to_a_later_one() {
    use std::os::unix::fs::PermissionsExt;
    let denied = private_dir();
    let allowed = private_dir();
    place_script(denied.path(), "felis-echo", "#!/bin/sh\necho denied\n");
    std::fs::set_permissions(
        denied.path().join("felis-echo"),
        std::fs::Permissions::from_mode(0o001),
    )
    .expect("leave only the others' execute bit");
    place_script(allowed.path(), "felis-echo", "#!/bin/sh\necho allowed\n");
    let path = std::env::join_paths([denied.path(), allowed.path()]).expect("join PATH");
    let out = cli_command()
        .env("PATH", path)
        .arg("echo")
        .output()
        .expect("run the external command");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "allowed\n");
}

/// `exec` of a script whose interpreter is missing fails with `ENOENT`
/// too; the lookup already found the command, so this is a launch
/// failure, not an unknown verb.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_external_command_that_fails_to_launch_is_not_an_unknown_verb() {
    let tmp = private_dir();
    place_script(tmp.path(), "felis-broken", "#!/nonexistent/interpreter\n");
    let out = external_command(tmp.path())
        .arg("broken")
        .output()
        .expect("run the broken command");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr={stderr}");
    assert!(
        stderr.contains(&tmp.path().join("felis-broken").display().to_string())
            && !stderr.contains("unrecognized subcommand"),
        "stderr={stderr}",
    );
}

/// The shell re-emits every second: an observer sees only events
/// published after it subscribed, so a one-shot emission would race the
/// CLI's connect + resolve + subscribe preamble.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_once_waits_for_a_live_notification() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "while :; do sleep 1; printf '\\033]9;task done\\007'; done",
        ]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
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
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "while :; do sleep 1; printf '\\033]9;still here\\007'; done",
        ]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());
        cmd
    });
    let mut daemon = KillableDaemon::start(&tmp, factory).await;
    let id = create_and_detach(&daemon.socket).await;
    let id_hex = format!("{id:032x}");

    // Read while it runs, so the daemon can be killed under a producing
    // stream.
    let mut child = tokio::process::Command::new(cli_bin())
        .env_remove("FELIS_SOCKET")
        .arg("--socket")
        .arg(&daemon.socket)
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
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "notifications",
            "subscribe",
            "--session",
            &id_hex,
            "--once",
            "--timeout",
            "1",
        ])
        .output()
        .expect("run felis notifications subscribe --once --timeout");
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
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "notifications",
            "subscribe",
            "--session",
            &id_hex,
            "--once",
            "--timeout",
            "0",
            "--format",
            "jsonl",
        ])
        .output()
        .expect("run felis notifications subscribe --once --timeout 0");
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

/// Named keys travel outside the bracketed-paste quoting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_key_enter_runs_the_pasted_command() {
    let tmp = private_dir();
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args([
            "-c",
            "printf '\\033]133;A\\007$ '; read x; eval \"$x\"; \
             printf '\\033]133;D;%d\\007' \"$?\"; read _y",
        ]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let (server, socket, _pool) = spawn_daemon(&tmp, factory).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "sessions",
            "send",
            &id_hex,
            "false",
            "--key",
            "enter",
            "--wait",
            "--timeout",
            "30",
        ])
        .output()
        .expect("run felis sessions send --key enter --wait");
    assert!(
        out.status.success(),
        "send --key enter --wait must exit 0: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "1",
        "the command ran, so its exit code must be observed",
    );

    server.abort();
}

/// A daemon that completes the handshake and then answers the roster
/// query with a window-only push, an arm an `Ops` link may never
/// receive (REQ-114). It is the protocol class of the failure matrix
/// below: the peer answered, but not in this protocol.
async fn rogue_daemon(socket: std::path::PathBuf) {
    use felis_protocol::MessageKind;
    use felis_protocol::messages::{ConnToClientMsg, PushMsg};
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::{
        Endpoint, FrameReader, FrameWriter, Listener,
        preface::{read_client_bootstrap, write_daemon_preface},
        server_split,
    };

    let listener = Listener::bind(&Endpoint::unix(socket)).expect("bind the rogue socket");
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
    let hello = reader
        .next_frame()
        .await
        .expect("read Hello")
        .expect("the CLI sends Hello");
    assert_eq!(hello.kind, MessageKind::Conn.as_u16());
    writer
        .send(&ConnToClientMsg::Welcome { identity: None })
        .await
        .expect("write Welcome");
    let _request = reader
        .next_frame()
        .await
        .expect("read the roster query")
        .expect("the CLI queries the roster");
    writer
        .send(&PushMsg::Reattach { id: 1 })
        .await
        .expect("write the rogue push");
    // Holding the socket open keeps a hang distinguishable from the
    // refusal the verb owes.
    tokio::time::sleep(Duration::from_secs(30)).await;
}

/// A daemon answering `Hello` with `Welcome`, the subscribe ack and one
/// notification in a single write, so `FrameReader` takes all three in
/// one `read_buf`. Answering after reading `Subscribe` leaves the event
/// in the kernel, where an unready socket times the wait out early.
async fn eager_notify_daemon(socket: std::path::PathBuf) {
    use felis_protocol::MessageKind;
    use felis_protocol::messages::{
        ConnToClientMsg, Correlation, Notification, NotifyToClientMsg, StreamId, Urgency,
    };
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR};
    use felis_transport::{
        CheckedFrame, Endpoint, FrameReader, FrameWriter, Listener,
        preface::{read_client_bootstrap, write_daemon_preface},
        server_split,
    };

    // Both ends number streams from 1, and this connection opens
    // exactly one, so the ack can be addressed before the request that
    // opens it arrives. The assertion below holds the daemon to it.
    let stream_id = StreamId::new(1).expect("1 is a stream id");

    let listener = Listener::bind(&Endpoint::unix(socket)).expect("bind the eager socket");
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
    let daemon = tokio::spawn(eager_notify_daemon(socket.clone()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the eager daemon never bound its socket",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args([
            "notifications",
            "subscribe",
            "--once",
            "--timeout",
            "0",
            "--format",
            "jsonl",
        ])
        .output()
        .expect("run felis notifications subscribe --once --timeout 0");
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

/// A failure the verb meets before its body still wears the verb's
/// framing: one error object on stderr, nothing on stdout, and the
/// kind's exit code (docs/reference/cli.md "Machine output").
#[test]
fn a_failure_before_the_verb_body_is_framed_as_the_verb_would() {
    let tmp = private_dir();

    // Resolving a relative `--config` reads the current directory,
    // which a deleted one denies.
    let gone = tmp.path().join("gone");
    std::fs::create_dir(&gone).unwrap();
    let script = format!(
        "cd {dir} && rmdir {dir} && exec {felis} --config felis.toml config path --format json",
        dir = gone.display(),
        felis = cli_bin().display(),
    );
    let unresolvable = StdCommand::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .env_remove("FELIS_SOCKET")
        .env_remove("RUST_LOG")
        .output()
        .expect("run felis with an unresolvable --config");
    assert_framed_failure(&unresolvable, "usage", 2);

    // A directory as stdin: `-` asks for a payload the process cannot
    // read, which is its own input channel failing rather than an
    // empty payload.
    let sink = std::fs::File::open(tmp.path()).unwrap();
    let unreadable = cli_command()
        .arg("--socket")
        .arg(tmp.path().join("daemon.sock"))
        .args(["sessions", "send", "deadbeef", "-", "--format", "json"])
        .stdin(std::process::Stdio::from(sink))
        .output()
        .expect("run felis sessions send - with an unreadable stdin");
    assert_framed_failure(&unreadable, "input_failed", 2);
}

fn assert_framed_failure(out: &std::process::Output, kind: &str, code: i32) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(code), "stderr={stderr}");
    assert!(
        out.stdout.is_empty(),
        "a failure writes no result: {}",
        String::from_utf8_lossy(&out.stdout),
    );
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "stderr is one object: {stderr}");
    let object = parse_point(lines[0], "the failure object");
    assert_eq!(object["error"]["kind"], kind, "{stderr}");
    assert!(object["error"]["message"].is_string(), "{stderr}");
}

/// The machine contract's failure classes, one per row: what the exit
/// code is, and that stderr carries exactly one parseable object. The
/// exception is a refusal clap states, which precedes any framing and
/// stays human (docs/reference/cli.md "Machine output").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_machine_failure_class_is_one_object_and_one_code() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let cold = tmp.path().join("no-daemon.sock");
    let rogue = tmp.path().join("rogue.sock");
    let rogue_task = tokio::spawn(rogue_daemon(rogue.clone()));
    for _ in 0..400 {
        if rogue.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let socket_arg = socket.display().to_string();
    let cold_arg = cold.display().to_string();
    let rogue_arg = rogue.display().to_string();

    // A refusal clap can state: human, exit 2, no framing chosen yet.
    let clap_usage = cli_command()
        .args([
            "--socket",
            &socket_arg,
            "sessions",
            "send",
            "--format",
            "json",
            "deadbeef",
        ])
        .output()
        .expect("run felis sessions send with no payload");
    assert_eq!(clap_usage.status.code(), Some(2));
    assert_eq!(clap_usage.stdout, Vec::<u8>::new());
    assert!(
        serde_json::from_str::<serde_json::Value>(
            String::from_utf8_lossy(&clap_usage.stderr).trim()
        )
        .is_err(),
        "a clap error is a human message: {}",
        String::from_utf8_lossy(&clap_usage.stderr),
    );

    for (argv, code, kind) in [
        // Usage after a successful parse: the verb's own framing.
        (
            vec![
                "--socket",
                &socket_arg,
                "window",
                "retarget",
                "--format",
                "json",
            ],
            2,
            "usage",
        ),
        // A typed refusal the daemon answered on the merits.
        (
            vec![
                "--socket",
                &socket_arg,
                "sessions",
                "spawn",
                "--rows",
                "9999",
                "--cols",
                "80",
                "--format",
                "json",
            ],
            1,
            "invalid_request",
        ),
        // Transport: nothing answered at the endpoint.
        (
            vec![
                "--socket", &cold_arg, "sessions", "info", "1a", "--format", "json",
            ],
            2,
            "daemon_unreachable",
        ),
        // Protocol: the peer answered, but not in this protocol.
        (
            vec![
                "--socket", &rogue_arg, "sessions", "info", "1a", "--format", "json",
            ],
            2,
            "protocol",
        ),
    ] {
        let out = cli_command().args(&argv).output().expect("run the verb");
        assert_eq!(
            out.status.code(),
            Some(code),
            "{argv:?}: stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            out.stdout.is_empty(),
            "{argv:?} wrote to stdout: {}",
            String::from_utf8_lossy(&out.stdout),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "{argv:?} stderr is one object: {stderr}");
        let object = parse_point(lines[0], "the failure object");
        assert_eq!(object["error"]["kind"], kind, "{argv:?}: {stderr}");
        assert!(object["error"]["message"].is_string(), "{argv:?}: {stderr}");
    }

    rogue_task.abort();
    server.abort();
}

/// A machine format silences the console log: with `felis-daemon`
/// unreachable the autospawn path logs at INFO, which would make a
/// point verb's one error object the second line on stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_mode_keeps_the_log_off_the_error_channel() {
    let tmp = private_dir();
    // Copied out of the cargo target directory so the sibling lookup
    // finds no `felis-daemon`, and run with an empty PATH so the spawn
    // fails at once instead of waiting on a real daemon.
    let bin = tmp.path().join("felis");
    std::fs::copy(cli_bin(), &bin).unwrap();
    let socket = tmp.path().join("no-daemon.sock");

    let out = StdCommand::new(&bin)
        .env_remove("FELIS_SOCKET")
        .env_remove("RUST_LOG")
        .env("PATH", "")
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "spawn", "--format", "json"])
        .output()
        .expect("run felis sessions spawn --format json");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "one object, no log line: {stderr}");
    let object = parse_point(lines[0], "the failure object");
    assert_eq!(object["error"]["kind"], "daemon_unreachable", "{stderr}");
}

/// Human output keeps the autospawn's INFO and DEBUG progress lines off
/// stderr by default, leaving only the failure itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_mode_logs_nothing_below_warn_by_default() {
    let tmp = private_dir();
    let bin = tmp.path().join("felis");
    std::fs::copy(cli_bin(), &bin).unwrap();
    let socket = tmp.path().join("no-daemon.sock");

    let out = StdCommand::new(&bin)
        .env_remove("FELIS_SOCKET")
        .env_remove("RUST_LOG")
        .env("PATH", "")
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "spawn"])
        .output()
        .expect("run felis sessions spawn");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.is_empty(), "the failure is still reported");
    assert!(
        !stderr.contains("INFO") && !stderr.contains("DEBUG"),
        "no progress log lines: {stderr}"
    );
}

/// `sessions info` resolves and shortens through one `Ops::Info`, so
/// the documented always-present `short_id` must still be there, must
/// still be a prefix of the id, and must still be at least the floor
/// length.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_json_still_carries_a_short_id_without_a_roster_fetch() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "info", &id_hex[..8], "--format", "json"])
        .output()
        .expect("run felis sessions info --format json");
    assert!(
        out.status.success(),
        "non-zero: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let object = parse_point(&stdout, "sessions info");
    assert_eq!(object["id"], id_hex.as_str());
    let short = object["short_id"].as_str().expect("short_id is a string");
    assert!(short.len() >= 8, "short_id below the floor: {stdout}");
    assert!(
        id_hex.starts_with(short),
        "short_id must be a prefix of the id: {stdout}"
    );

    server.abort();
}

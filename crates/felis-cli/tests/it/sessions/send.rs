use std::io::Write;
use std::process::Stdio;

use felis_protocol::limits::MAX_PASTE_BYTES;

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_without_text_or_dash_is_a_usage_error_exit_two() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");

    let out = felis_out(&socket, &["sessions", "send", "deadbeef"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "send with no <text>/`-` must exit 2 (usage): status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        out.stdout.is_empty(),
        "a usage error writes nothing to stdout: {}",
        String::from_utf8_lossy(&out.stdout),
    );

    // A refusal clap can state is stated by clap: argument parsing
    // precedes format selection, so `--format json` yields no machine
    // object.
    let framed = felis_out(
        &socket,
        &["sessions", "send", "--format", "json", "deadbeef"],
    );
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
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let ok = cli_on(&socket)
        .args(["sessions", "send", &id_hex, "-"])
        .stdin(Stdio::null())
        .output()
        .expect("run felis sessions send - with empty stdin");
    assert_eq!(
        ok.status.code(),
        Some(0),
        "empty paste to a valid session stays 0: status={:?} stderr={}",
        ok.status,
        String::from_utf8_lossy(&ok.stderr),
    );

    let unknown = cli_on(&socket)
        .args(["sessions", "send", "deadbeef", "-"])
        .stdin(Stdio::null())
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
    let factory = sh_factory("read x; printf 'got=%s' \"$x\"; read _y");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = felis_out(
        &socket,
        &["sessions", "send", &id_hex, "PROBE-12345\n", "--raw"],
    );
    assert!(
        out.status.success(),
        "send must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    poll_until(
        &socket,
        &["sessions", "capture", &id_hex],
        "got=PROBE-12345",
    )
    .await;

    server.abort();
}

/// Writing the frames is not delivering them. While a session's input
/// budget is full the daemon's pump parks mid-message, and a peer that
/// closes its socket there is noticed and its admitted-but-unwritten
/// input abandoned; a `send` that exits as soon as it has written
/// reports success for bytes no child ever sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_waits_for_admission_rather_than_dropping_its_input() {
    // Past the 64 bytes the first payload leaves unreserved, so this one
    // cannot slip into the budget beside it.
    const MARKER: &str = "MARKER-0123456789-0123456789-0123456789-0123456789-\
0123456789-0123456789-0123456789";

    let tmp = private_dir();
    let received = tmp.path().join("received");
    let gate = tmp.path().join("gate");
    let mkfifo = StdCommand::new("mkfifo")
        .arg(&gate)
        .env("PATH", fixture_path())
        .status()
        .expect("run mkfifo");
    assert!(mkfifo.success(), "mkfifo failed: {mkfifo:?}");
    let factory = sh_factory(format!(
        "stty raw -echo; read _ < {}; exec cat > {}",
        gate.display(),
        received.display()
    ));
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    // Fills the session's budget against a child that is not reading.
    let mut filler = cli_on(&socket)
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
        let status = felis_out(&socket, &["daemon", "status"]);
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

    let mut marker = cli_on(&socket)
        .args(["sessions", "send", &id_hex, MARKER, "--raw"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run felis sessions send");
    // The child is still gated, so a send that exits inside this window
    // returned before its input was admitted.
    let parked_until = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < parked_until {
        assert!(
            marker.try_wait().expect("poll marker send").is_none(),
            "send exited while the budget was still full",
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::write(&gate, b"\n").expect("release the child");
    let marker = marker.wait_with_output().expect("wait for marker send");
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
    // Past the 64 bytes the filler leaves unreserved, so this payload
    // cannot slip into the budget beside it.
    const PAYLOAD: &str = "PAYLOAD-0123456789-0123456789-0123456789-0123456789-\
0123456789-0123456789-0123456789";

    let tmp = private_dir();
    // Never reads its stdin: the budget it fills is never released.
    let factory = sh_factory("stty raw -echo; sleep 3600");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let mut filler = cli_on(&socket)
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
        let status = felis_out(&socket, &["daemon", "status"]);
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
    let out = felis_out(
        &socket,
        &[
            "sessions",
            "send",
            &id_hex,
            PAYLOAD,
            "--wait",
            "--timeout",
            "1",
        ],
    );
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
    // Never reads its stdin, so nothing can satisfy the wait early.
    let factory = sh_factory("stty raw -echo; sleep 3600");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = felis_out(
        &socket,
        &[
            "sessions",
            "send",
            &id_hex,
            "PAYLOAD",
            "--wait",
            "--timeout",
            "0",
            "--format",
            "json",
        ],
    );
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
    let factory = sh_factory("read x; printf 'got=%s' \"$x\"; read _y");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    // No newline in the text: the Enter comes from `--key`.
    let out = felis_out(
        &socket,
        &["sessions", "send", &id_hex, "SUBMIT-99", "--key", "enter"],
    );
    assert!(
        out.status.success(),
        "send --key enter must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    poll_until(&socket, &["sessions", "capture", &id_hex], "got=SUBMIT-99").await;

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_against_unknown_id_exits_one() {
    let tmp = private_dir();
    let (server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());

    let out = felis_out(
        &socket,
        &["sessions", "send", "deadbeef", "anything", "--raw"],
    );
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

/// Named keys travel outside the bracketed-paste quoting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_key_enter_runs_the_pasted_command() {
    let tmp = private_dir();
    let factory = sh_factory(
        "printf '\\033]133;A\\007$ '; read x; eval \"$x\"; \
             printf '\\033]133;D;%d\\007' \"$?\"; read _y",
    );
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    let out = felis_out(
        &socket,
        &[
            "sessions",
            "send",
            &id_hex,
            "false",
            "--key",
            "enter",
            "--wait",
            "--timeout",
            "30",
        ],
    );
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

/// A send past the step cap is the request's fault wherever it is
/// pointed: it fails as `invalid_request` with no daemon to dial.
#[test]
fn a_send_past_the_step_cap_is_refused_with_no_daemon_running() {
    let tmp = private_dir();
    let cold = tmp.path().join("no-daemon.sock");
    let steps = felis_protocol::limits::MAX_SEND_STEPS + 1;
    let out = cli_on(&cold)
        .args(["sessions", "send", "deadbeef", "--format", "json"])
        .args(std::iter::repeat_n(["--key", "a"], steps).flatten())
        .output()
        .expect("run felis sessions send past the step cap");
    assert_framed_failure(&out, "invalid_request", 1);
    assert!(!cold.exists(), "no daemon was spawned to refuse it");
}

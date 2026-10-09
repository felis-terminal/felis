//! `felis daemon` integration tests (docs/reference/cli.md
//! "Daemon status", "Daemon stop", "Daemon upgrade").

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::process::Command as StdCommand;
use std::time::Duration;

use felis_daemon::serve::DaemonCaps;

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/schema.rs"]
mod schema;

use fixtures::{
    create_and_detach, private_dir, quiet_factory, spawn_daemon, spawn_daemon_with_caps,
};

/// `FELIS_SOCKET` is cleared so a run started inside a felis window
/// cannot address the developer's own daemon.
fn cli_command() -> StdCommand {
    let bin = std::env::var("CARGO_BIN_EXE_felis").expect("cargo sets CARGO_BIN_EXE_felis");
    let mut cmd = StdCommand::new(bin);
    cmd.env_remove("FELIS_SOCKET");
    cmd
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_json_reports_every_resource_with_limit_and_usage() {
    let tmp = private_dir();
    let (_server, socket, _pool) = spawn_daemon_with_caps(
        &tmp,
        quiet_factory(),
        DaemonCaps {
            max_sessions: 9,
            ..DaemonCaps::default()
        },
    )
    .await;
    create_and_detach(&socket).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "status", "--format", "json"])
        .output()
        .expect("run felis daemon status");
    assert!(
        out.status.success(),
        "non-zero exit: {:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON object");
    schema::assert_cli_object(&object);
    assert_eq!(object["v"], 1);
    assert_eq!(
        object["version"].as_str(),
        Some(felis_daemon::build_identity().to_string().as_str()),
        "the report names the build the handshake reported: {stdout}"
    );
    assert_eq!(
        object["protocol"]["major"].as_u64(),
        Some(u64::from(felis_protocol::preface::PROTOCOL_MAJOR))
    );
    assert_eq!(
        object["protocol"]["minor"].as_u64(),
        Some(u64::from(felis_protocol::preface::PROTOCOL_MINOR)),
        "the daemon's own minor, not the connection's effective one: {stdout}"
    );
    assert!(
        object["worker_threads"].as_u64().is_some_and(|n| n > 0),
        "the daemon's worker count must be reported: {stdout}"
    );

    let rows = object["resources"].as_array().expect("a resources array");
    let row = |name: &str| {
        rows.iter()
            .find(|r| r["resource"] == name)
            .unwrap_or_else(|| panic!("no {name} row in {stdout}"))
    };
    for name in [
        "connections",
        "sessions",
        "image_store_bytes",
        "in_flight_decodes",
        "in_flight_decode_bytes",
        "subscriber_queue_bytes",
        "pty_input_bytes",
    ] {
        let r = row(name);
        assert!(
            r["total_used"].as_u64().is_some(),
            "{name} reports no total"
        );
        assert!(
            r["per_subject_limit"].as_u64().is_some() || r["global_limit"].as_u64().is_some(),
            "{name} reports no ceiling"
        );
        assert!(r["scope"].as_str().is_some(), "{name} reports no scope");
        assert!(r["unit"].as_str().is_some(), "{name} reports no unit");
    }
    // The field-presence contract, one row per subject scope: a daemon
    // row omits the subject dimensions entirely rather than repeating
    // its total under a second name.
    assert_eq!(row("sessions")["total_used"], 1);
    assert_eq!(row("sessions")["global_limit"], 9);
    assert_eq!(row("sessions")["scope"], "daemon");
    assert!(row("sessions")["max_subject_used"].is_null());
    assert!(row("sessions")["per_subject_limit"].is_null());
    assert!(row("sessions")["used"].is_null(), "the old key is gone");
    assert!(row("sessions")["limit"].is_null(), "the old key is gone");

    let images = row("image_store_bytes");
    assert_eq!(images["scope"], "session");
    assert!(images["max_subject_used"].as_u64().is_some());
    assert!(images["per_subject_limit"].as_u64().is_some());
    assert!(
        images["global_limit"].is_null(),
        "no daemon-wide image budget exists to report: {stdout}"
    );

    let outbox = row("subscriber_queue_bytes");
    assert_eq!(outbox["scope"], "subscriber");
    assert!(outbox["max_subject_used"].as_u64().is_some());
    assert!(outbox["per_subject_limit"].as_u64().is_some());

    let input = row("pty_input_bytes");
    assert_eq!(input["scope"], "session");
    assert_eq!(input["unit"], "bytes");
    assert!(input["max_subject_used"].as_u64().is_some());
    assert_eq!(
        input["per_subject_limit"],
        serde_json::json!(felis_protocol::limits::PTY_INPUT_BUDGET),
    );
    assert!(
        input["global_limit"].is_null(),
        "input is budgeted per session only: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_human_names_the_build_and_every_row() {
    let tmp = private_dir();
    let (_server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "status"])
        .output()
        .expect("run felis daemon status");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("version:"), "{stdout}");
    assert!(stdout.contains("wire:"), "{stdout}");
    assert!(stdout.contains("workers:"), "{stdout}");
    for name in [
        "sessions",
        "image_store_bytes",
        "subscriber_queue_bytes",
        "pty_input_bytes",
    ] {
        assert!(stdout.contains(name), "{name} missing from {stdout}");
    }
    assert!(stdout.contains("max session"), "{stdout}");
    assert!(stdout.contains("256 MiB per session"), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("pty_input_bytes")
                && line.contains("max session")
                && line.contains("16 MiB per session")),
        "the input row must name its own per-session denominator: {stdout}"
    );
    // The ratio that would be wrong: a daemon-wide total over a ceiling
    // charged per session.
    for line in stdout.lines() {
        let Some((head, _)) = line.split_once('\u{00b7}') else {
            continue;
        };
        assert!(
            !head.contains("per session") && !head.contains("per subscriber"),
            "a total is divided by a per-subject ceiling: {line}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_against_no_daemon_exits_two_with_a_typed_error() {
    let tmp = private_dir();
    let socket = tmp.path().join("nothing.sock");

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "status", "--format", "json"])
        .output()
        .expect("run felis daemon status");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a failure writes no result object");
    let stderr = String::from_utf8(out.stderr).unwrap();
    let line = stderr
        .lines()
        .find(|l| l.starts_with('{'))
        .expect("a machine error object");
    let object: serde_json::Value = serde_json::from_str(line).unwrap();
    schema::assert_cli_object(&object);
    assert_eq!(object["v"], 1);
    assert_eq!(object["error"]["kind"], "daemon_unreachable");
}

#[test]
fn status_refuses_the_stream_framing_as_a_usage_error() {
    let out = cli_command()
        .args(["daemon", "status", "--format", "jsonl"])
        .output()
        .expect("run felis daemon status");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("[possible values: human, json]"),
        "{stderr}"
    );
}

/// Exit 1 with `at_capacity`, not exit 2: a full daemon answered, and
/// the remedy (free a connection, retry) is the caller's to take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verb_dialing_a_full_daemon_is_typed_at_capacity() {
    use felis_daemon::serve::ConnectionAdmission;

    let tmp = private_dir();
    // A cap of zero admits nothing, so the refusal needs no racing
    // second dial to provoke.
    let (_server, socket, _pool) = spawn_daemon_with_caps(
        &tmp,
        quiet_factory(),
        DaemonCaps {
            admission: ConnectionAdmission::with_refusal_slots(0, 4),
            ..DaemonCaps::default()
        },
    )
    .await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "status", "--format", "json"])
        .output()
        .expect("run felis daemon status");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a full daemon is a refused-but-well-formed request: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    let line = stderr
        .lines()
        .find(|l| l.starts_with('{'))
        .expect("a machine error object");
    let object: serde_json::Value = serde_json::from_str(line).unwrap();
    schema::assert_cli_object(&object);
    assert_eq!(object["error"]["kind"], "at_capacity");
    let message = object["error"]["message"].as_str().unwrap();
    assert!(message.contains("at 0 of 0 connections"), "{message}");
}

/// A default stop over a daemon holding a session is a typed refusal:
/// exit `1`, the count in the error object, and the daemon still there
/// to answer the next verb (docs/reference/cli.md "Daemon stop").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_refuses_while_sessions_remain_and_reports_the_count() {
    let tmp = private_dir();
    let (_server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    create_and_detach(&socket).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "stop", "--format", "json"])
        .output()
        .expect("run felis daemon stop");
    assert_eq!(out.status.code(), Some(1), "a typed refusal exits 1");
    let stderr = String::from_utf8(out.stderr).unwrap();
    let object: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("one JSON error object");
    schema::assert_cli_object(&object);
    assert_eq!(object["v"], 1);
    assert_eq!(object["error"]["kind"], "refused");
    assert_eq!(
        object["error"]["sessions"], 1,
        "the count a caller decides --force against: {stderr}"
    );

    let after = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "status", "--format", "json"])
        .output()
        .expect("run felis daemon status");
    assert!(after.status.success(), "the refused daemon still answers");
    let status: serde_json::Value =
        serde_json::from_slice(after.stdout.trim_ascii()).expect("one JSON object");
    assert_eq!(status["draining"], false, "a refused stop starts no drain");
}

/// An empty daemon stops, says so with the mode it took, and is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_on_an_empty_daemon_reports_stopping_and_ends_it() {
    let tmp = private_dir();
    let (_server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;

    let out = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "stop", "--format", "json"])
        .output()
        .expect("run felis daemon stop");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON object");
    schema::assert_cli_object(&object);
    assert_eq!(object["outcome"], "stopping");
    assert_eq!(object["mode"], "if_empty");

    // REQ-009d: exit unlinks nothing, so the path survives and the
    // connect is what says the daemon is gone.
    for _ in 0..200 {
        if std::os::unix::net::UnixStream::connect(&socket).is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&socket)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::ConnectionRefused,
        "the stopped daemon stops answering"
    );
    assert!(
        std::fs::symlink_metadata(&socket).is_ok(),
        "and leaves its socket path for the next start to replace"
    );

    // And a second stop finds nothing to talk to: exit 2, never a
    // daemon started in order to be stopped.
    let again = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "stop", "--format", "json"])
        .output()
        .expect("run felis daemon stop");
    assert_eq!(again.status.code(), Some(2));
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&socket)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::ConnectionRefused,
        "the stop autospawned no daemon"
    );
}

/// A `felis-daemon` for the successor lookup to find on `PATH`. The
/// file is never run: a daemon that refuses the upgrade does so before
/// exec, and the lookup only asks that the file exist.
fn path_with_a_successor(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let bin = tmp.path().join("successor-bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("felis-daemon"), b"").unwrap();
    bin
}

/// A refused upgrade is a typed refusal: exit `1`, the daemon's reason
/// token beside the message, and the daemon still there to answer the
/// next verb (docs/reference/cli.md "Daemon upgrade").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_upgrade_reports_the_reason_and_leaves_the_daemon_serving() {
    let tmp = private_dir();
    let (_server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    create_and_detach(&socket).await;

    let out = cli_command()
        .env("PATH", path_with_a_successor(&tmp))
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "upgrade", "--format", "json"])
        .output()
        .expect("run felis daemon upgrade");
    assert_eq!(out.status.code(), Some(1), "a typed refusal exits 1");
    let stderr = String::from_utf8(out.stderr).unwrap();
    let object: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("one JSON error object");
    schema::assert_cli_object(&object);
    assert_eq!(object["error"]["kind"], "refused");
    assert!(
        object["error"]["reason"].is_string(),
        "the refusal carries the daemon's reason: {stderr}"
    );
    assert!(
        object["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("felis daemon stop --when-empty")),
        "the message names the drain-and-restart remedy: {stderr}"
    );

    let after = cli_command()
        .arg("--socket")
        .arg(&socket)
        .args(["sessions", "list", "--format", "json"])
        .output()
        .expect("run felis sessions list");
    assert!(after.status.success(), "the refused daemon still answers");
    let roster: serde_json::Value =
        serde_json::from_slice(after.stdout.trim_ascii()).expect("one JSON object");
    assert_eq!(
        roster["sessions"].as_array().map(Vec::len),
        Some(1),
        "the session survived the refusal"
    );
}

/// No daemon is no upgrade: exit `2`, and nothing started in order to
/// be replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upgrade_with_no_daemon_running_starts_none() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");

    let out = cli_command()
        .env("PATH", path_with_a_successor(&tmp))
        .arg("--socket")
        .arg(&socket)
        .args(["daemon", "upgrade", "--format", "json"])
        .output()
        .expect("run felis daemon upgrade");
    assert_eq!(out.status.code(), Some(2));
    let object: serde_json::Value =
        serde_json::from_slice(out.stderr.trim_ascii()).expect("one JSON error object");
    assert_eq!(object["error"]["kind"], "daemon_unreachable");
    assert!(
        std::os::unix::net::UnixStream::connect(&socket).is_err(),
        "the upgrade autospawned no daemon"
    );
}

/// Over `--host` the upgrade runs the remote host's own `felis`; an
/// ssh that cannot run is the daemon being unreachable, exit `2`.
#[test]
fn an_upgrade_over_host_without_ssh_is_unreachable() {
    let tmp = private_dir();
    let out = cli_command()
        .env("PATH", tmp.path())
        .args(["--host", "user@devbox.invalid", "daemon", "upgrade"])
        .args(["--format", "json"])
        .output()
        .expect("run felis daemon upgrade");
    assert_eq!(out.status.code(), Some(2));
    let object: serde_json::Value =
        serde_json::from_slice(out.stderr.trim_ascii()).expect("one JSON error object");
    schema::assert_cli_object(&object);
    assert_eq!(object["error"]["kind"], "daemon_unreachable");
}

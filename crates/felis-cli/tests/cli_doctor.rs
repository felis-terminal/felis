//! `felis doctor` integration tests (docs/reference/cli.md "Doctor").
//! The verdicts depend on the machine running the suite, so only the
//! row set, the status vocabulary, the envelope, and the exit codes are
//! pinned.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command as StdCommand;

use tempfile::TempDir;

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/schema.rs"]
mod schema;

use fixtures::private_dir;

fn cli(home: &TempDir) -> StdCommand {
    let mut cmd = cli_resolving(home);
    cmd.arg("--socket").arg(home.path().join("cold.sock"));
    cmd
}

/// Without `--socket`, so the child resolves the endpoint itself.
fn cli_resolving(home: &TempDir) -> StdCommand {
    let bin = std::env::var("CARGO_BIN_EXE_felis").expect("cargo sets CARGO_BIN_EXE_felis");
    let mut cmd = StdCommand::new(bin);
    cmd.env_remove("FELIS_SOCKET");
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join("config"));
    cmd
}

/// Selected with `--config` rather than written to the discovered
/// location: Windows resolves `%APPDATA%` through the known-folder API,
/// which reads no environment variable, so a temp profile cannot
/// isolate the child (`docs/reference/testing.md` "CI shape").
fn selected_config(home: &TempDir) -> std::path::PathBuf {
    home.path().join("config.toml")
}

fn write_config(home: &TempDir, text: &str) {
    std::fs::write(selected_config(home), text).unwrap();
}

fn cli_with_config(home: &TempDir) -> StdCommand {
    let mut cmd = cli(home);
    cmd.arg("--config").arg(selected_config(home));
    cmd
}

/// The endpoint is the uid's, whatever the environment says:
/// `XDG_RUNTIME_DIR` and `TMPDIR` take an absolute path, a relative
/// one, an empty one, and no value at all, and the stamped `daemon`
/// note names the same default every time. In-process the matrix is
/// unreachable: this workspace forbids `std::env::set_var`.
#[cfg(unix)]
#[test]
fn the_default_endpoint_is_the_uids_under_every_environment() {
    use std::os::unix::fs::MetadataExt as _;

    let home = private_dir();
    let uid = std::fs::metadata(home.path()).unwrap().uid();
    let expected = format!("/tmp/felis.{uid}/daemon.sock");
    let stamp = home.path().join("stamped").join("daemon.sock");

    for (runtime_dir, tmpdir) in [
        (Some("/run/user/99999"), None),
        (Some("relative/runtime"), Some("/var/tmp")),
        (Some(""), Some("")),
        (None, Some("relative/tmp")),
        (None, None),
    ] {
        let mut cmd = cli_resolving(&home);
        cmd.env("FELIS_SOCKET", &stamp);
        match runtime_dir {
            Some(value) => cmd.env("XDG_RUNTIME_DIR", value),
            None => cmd.env_remove("XDG_RUNTIME_DIR"),
        };
        match tmpdir {
            Some(value) => cmd.env("TMPDIR", value),
            None => cmd.env_remove("TMPDIR"),
        };
        let out = cmd.args(["doctor", "--format", "json"]).output().unwrap();

        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let daemon = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["check"] == "daemon")
            .expect("the daemon row is always reported");
        let detail = daemon["detail"].as_str().unwrap();
        assert!(
            detail.contains(&expected),
            "XDG_RUNTIME_DIR={runtime_dir:?} TMPDIR={tmpdir:?}: {detail}"
        );
    }
}

#[test]
fn the_checklist_reports_every_row_with_a_known_status() {
    let home = private_dir();
    let out = cli(&home)
        .args(["doctor", "--format", "json"])
        .output()
        .unwrap();
    // Never 2: an unreachable daemon is a finding, not a failure.
    assert!(
        matches!(out.status.code(), Some(0 | 1)),
        "{:?}",
        out.status.code()
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    schema::assert_cli_object(&object);
    assert_eq!(object["v"], 1);

    let checks = object["checks"].as_array().unwrap();
    let names: Vec<&str> = checks
        .iter()
        .map(|c| c["check"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "daemon",
            "config",
            "terminfo",
            "gpu",
            "clipboard",
            "remote_helper"
        ]
    );
    for check in checks {
        let status = check["status"].as_str().unwrap();
        assert!(
            ["ok", "warn", "fail", "skipped"].contains(&status),
            "{status}"
        );
        assert_ne!(check["detail"].as_str().unwrap(), "");
    }
    let failed = checks.iter().filter(|c| c["status"] == "fail").count();
    assert_eq!(object["failed"].as_u64().unwrap(), failed as u64);
    assert_eq!(out.status.code(), Some(i32::from(failed > 0)));
}

#[test]
fn a_cold_socket_warns_rather_than_failing_the_run() {
    let home = private_dir();
    let out = cli(&home)
        .args(["doctor", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    schema::assert_cli_object(&object);
    let daemon = object["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "daemon")
        .unwrap();
    assert_eq!(daemon["status"], "warn");
    assert!(
        daemon["detail"].as_str().unwrap().contains("not running"),
        "{daemon}"
    );
}

/// The row's minor is `min(this build, the daemon's)`; unlabeled, it
/// would read as the daemon's own and hide the skew.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_row_names_the_negotiated_minor_as_negotiated() {
    use std::sync::Arc;
    use std::time::Duration;

    use felis_daemon::{
        SessionPool,
        serve::{DaemonCaps, SessionFactory, serve_unix_with_factory},
    };
    use felis_pty::Command as PtyCommand;
    use tokio::sync::Mutex;

    let home = private_dir();
    let socket = home.path().join("daemon.sock");
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args(["-c", "read x"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let server_path = socket.clone();
    let server = tokio::spawn(async move {
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        drop(serve_unix_with_factory(&server_path, DaemonCaps::default(), pool, factory).await);
    });
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let bin = std::env::var("CARGO_BIN_EXE_felis").unwrap();
    let out = StdCommand::new(bin)
        .env_remove("FELIS_SOCKET")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .arg("--socket")
        .arg(&socket)
        .args(["doctor", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    schema::assert_cli_object(&object);
    let daemon = object["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "daemon")
        .unwrap();
    assert_eq!(daemon["status"], "ok", "{daemon}");
    let detail = daemon["detail"].as_str().unwrap();
    assert!(detail.contains("negotiated wire"), "{detail}");
    assert!(
        detail.contains("felis daemon status"),
        "the row points at the verb that reports the daemon's own minor: {detail}"
    );

    server.abort();
}

#[test]
fn a_broken_config_fails_its_row_and_points_at_config_check() {
    let home = private_dir();
    write_config(&home, "[font\n");
    let out = cli_with_config(&home)
        .args(["doctor", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    schema::assert_cli_object(&object);
    let config = object["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "config")
        .unwrap();
    assert_eq!(config["status"], "fail");
    let detail = config["detail"].as_str().unwrap();
    assert!(detail.contains("felis config check"), "{detail}");
}

/// `doctor` reads the file `--config` selected, so its verdict matches
/// what `felis config check` would report on the same document.
#[test]
fn a_missing_selected_file_fails_the_config_row() {
    let home = private_dir();
    let out = cli_with_config(&home)
        .args(["doctor", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    schema::assert_cli_object(&object);
    let config = object["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "config")
        .unwrap();
    assert_eq!(config["status"], "fail", "{config}");
    let detail = config["detail"].as_str().unwrap();
    assert!(
        detail.contains(&selected_config(&home).display().to_string()),
        "the row names the selected file: {detail}"
    );
}

#[test]
fn a_config_warning_does_not_fail_its_row() {
    let home = private_dir();
    write_config(&home, "[font]\nsizee = 14.0\n");
    let out = cli_with_config(&home)
        .args(["doctor", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    schema::assert_cli_object(&object);
    let config = object["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "config")
        .unwrap();
    assert_eq!(config["status"], "warn");
    assert!(
        config["detail"].as_str().unwrap().contains("warning"),
        "{config}"
    );
}

#[test]
fn the_human_framing_is_one_line_per_check() {
    let home = private_dir();
    let out = cli(&home).arg("doctor").output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 6, "{stdout}");
    assert!(lines[0].starts_with("warn"), "{stdout}");
    assert!(lines[0].contains("daemon"), "{stdout}");
}

#[test]
fn the_stream_framing_is_a_usage_error() {
    let home = private_dir();
    let out = cli(&home)
        .args(["doctor", "--format", "jsonl"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(out.stdout, Vec::<u8>::new());
}

/// A full daemon is running, so the row must not read "not running"
/// and must not advise the window launch the same cap would refuse.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_row_says_at_capacity_rather_than_not_running() {
    use std::sync::Arc;
    use std::time::Duration;

    use felis_daemon::{
        SessionPool,
        serve::{ConnectionAdmission, DaemonCaps, SessionFactory, serve_unix_with_factory},
    };
    use felis_pty::Command as PtyCommand;
    use tokio::sync::Mutex;

    let home = private_dir();
    let socket = home.path().join("full.sock");
    let factory: SessionFactory = Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args(["-c", "read x"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    });
    let server_path = socket.clone();
    // A cap of zero admits nothing, so the refusal needs no racing
    // second dial to provoke.
    let caps = DaemonCaps {
        admission: ConnectionAdmission::with_refusal_slots(0, 4),
        ..DaemonCaps::default()
    };
    let server = tokio::spawn(async move {
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        drop(serve_unix_with_factory(&server_path, caps, pool, factory).await);
    });
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let bin = std::env::var("CARGO_BIN_EXE_felis").unwrap();
    let out = StdCommand::new(bin)
        .env_remove("FELIS_SOCKET")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .arg("--socket")
        .arg(&socket)
        .args(["doctor", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let object: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    schema::assert_cli_object(&object);
    let daemon = object["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "daemon")
        .unwrap();
    // Warn, not fail: a peer disconnecting fixes it.
    assert_eq!(daemon["status"], "warn", "{daemon}");
    let detail = daemon["detail"].as_str().unwrap();
    assert!(detail.contains("at 0 of 0 connections"), "{detail}");
    assert!(!detail.contains("not running"), "{detail}");

    server.abort();
}

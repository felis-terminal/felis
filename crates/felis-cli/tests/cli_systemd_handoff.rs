//! The systemd hand-off against a real user manager, opt-in through
//! `FELIS_SYSTEMD_TESTS=1` (`docs/reference/testing.md` "Contributor
//! environment variables"). Every socket path here is unique, so the
//! units these tests load never collide with the developer's own daemon.

#![cfg(target_os = "linux")]
// The Drop guard reports a leaked unit while a test is already
// panicking, where an assertion would abort the process instead.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use tempfile::TempDir;

#[path = "common/fixtures.rs"]
mod fixtures;

use fixtures::private_dir;

/// Above the launcher's own cumulative bound (the helper budgets, the
/// unit's `TimeoutStartSec`, and both connect windows), so a breach here
/// means the hand-off wedged rather than that it was still working.
const BUDGET: Duration = Duration::from_secs(60);

fn enabled() -> bool {
    std::env::var_os("FELIS_SYSTEMD_TESTS").is_some_and(|value| value == "1")
}

fn cli_bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_felis").expect("cargo sets CARGO_BIN_EXE_felis"))
}

/// The unit name the launcher derives, restated here so a change to the
/// naming cannot pass unnoticed by renaming both sides at once.
fn unit_name(socket: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in socket.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("felis-daemon-{hash:016x}")
}

fn felis(socket: &Path) -> Command {
    let mut cmd = Command::new(cli_bin());
    cmd.env_remove("FELIS_SOCKET")
        .env_remove("RUST_LOG")
        .arg("--socket")
        .arg(socket);
    cmd
}

fn start(mut command: Command) -> Child {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start a helper process")
}

fn await_exit(mut child: Child, what: &str) -> Output {
    let deadline = Instant::now() + BUDGET;
    while child.try_wait().expect("poll a helper process").is_none() {
        if Instant::now() >= deadline {
            // Killed and reaped before the panic: a dropped `Child`
            // leaves the process running and this test's temp socket
            // with it.
            drop(child.kill());
            drop(child.wait());
            panic!("{what} did not exit within {BUDGET:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().expect("collect a helper process")
}

fn run(command: Command, what: &str) -> Output {
    await_exit(start(command), what)
}

fn show(unit: &str, property: &str) -> String {
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "show", "-p", property, "--value", unit]);
    let out = run(cmd, "systemctl --user show");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A `$` in the path is the case the launcher must pass through the
/// manager's argument expansion unharmed.
fn cold_socket(tmp: &TempDir, name: &str) -> PathBuf {
    let dir = tmp.path().join(format!("felis${name}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("daemon.sock")
}

/// Holds the socket and the unit its daemon lands in, so a failing
/// assertion cannot leave a transient unit loaded on the developer's
/// manager. Installed before anything starts a daemon.
struct Handed {
    socket: PathBuf,
    unit: String,
}

impl Handed {
    fn new(socket: PathBuf) -> Self {
        let unit = unit_name(&socket);
        Self { socket, unit }
    }
}

impl Drop for Handed {
    fn drop(&mut self) {
        let mut stop = felis(&self.socket);
        stop.args(["daemon", "stop", "--force"]);
        let stopped = run(stop, "felis daemon stop --force").status.success();
        if !stopped {
            // `RefuseManualStop` blocks `systemctl stop`, not a signal:
            // the kill is what takes down a daemon that stopped
            // answering its own verb.
            let mut kill = Command::new("systemctl");
            kill.args(["--user", "kill", &self.unit]);
            drop(run(kill, "systemctl --user kill"));
        }
        let load_state = show(&self.unit, "LoadState");
        if std::thread::panicking() {
            eprintln!("unit {} left in LoadState={load_state}", self.unit);
        } else {
            assert_eq!(
                load_state, "not-found",
                "--collect must leave the unit name reusable"
            );
        }
    }
}

#[test]
fn a_spawn_from_a_user_manager_unit_lands_in_its_own_transient_service() {
    if !enabled() {
        return;
    }
    let tmp = private_dir();
    let handed = Handed::new(cold_socket(&tmp, "handoff"));

    let mut spawn = felis(&handed.socket);
    spawn.args([
        "sessions", "spawn", "--format", "json", "--", "/bin/sh", "-c", "read x",
    ]);
    let out = run(spawn, "felis sessions spawn");
    assert!(
        out.status.success(),
        "spawn failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "one result object, nothing else: {stdout}");
    let object: serde_json::Value = serde_json::from_str(lines[0]).expect("a JSON result object");
    assert_eq!(object["v"], 1, "{stdout}");

    assert_eq!(show(&handed.unit, "ActiveState"), "active");
    assert_eq!(show(&handed.unit, "OOMPolicy"), "continue");
    assert_eq!(show(&handed.unit, "Slice"), "app.slice");
    assert_eq!(show(&handed.unit, "RefuseManualStop"), "yes");
}

#[test]
fn two_launchers_on_one_cold_socket_end_up_on_one_daemon() {
    if !enabled() {
        return;
    }
    let tmp = private_dir();
    let handed = Handed::new(cold_socket(&tmp, "concurrent"));

    let launchers: Vec<Child> = (0..2)
        .map(|_| {
            let mut spawn = felis(&handed.socket);
            spawn.args([
                "sessions", "spawn", "--format", "json", "--", "/bin/sh", "-c", "read x",
            ]);
            start(spawn)
        })
        .collect();
    for launcher in launchers {
        let out = await_exit(launcher, "a concurrent felis sessions spawn");
        assert!(
            out.status.success(),
            "a concurrent launcher failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    assert_eq!(show(&handed.unit, "ActiveState"), "active");
    let mut list = felis(&handed.socket);
    list.args(["sessions", "list", "--format", "json"]);
    let listed = run(list, "felis sessions list");
    let sessions: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&listed.stdout)).expect("a JSON list");
    assert_eq!(
        sessions["sessions"].as_array().map(Vec::len),
        Some(2),
        "both launchers must have reached the same daemon: {sessions}"
    );

    // The daemon the manager started holds both sessions, so the unit
    // that survives every fork race is the one under test here: a loser
    // that forked would have lost the bind and left no second unit.
    let mut units = Command::new("systemctl");
    units.args([
        "--user",
        "list-units",
        "--all",
        "--plain",
        "--no-legend",
        "felis-daemon-*",
    ]);
    let listed = run(units, "systemctl --user list-units");
    let text = String::from_utf8_lossy(&listed.stdout);
    let mine = text
        .lines()
        .filter(|line| line.contains(&handed.unit))
        .count();
    assert_eq!(mine, 1, "one unit for one socket: {text}");
}

//! `felis --version` and `felis version` integration tests. Only the
//! daemon row is pinned by content: the cli and client identities
//! depend on the build the suite runs from
//! (`docs/reference/cli.md` "Version reporting").

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::process::{Command as StdCommand, Output};

use felis_daemon::serve::{ConnectionAdmission, DaemonCaps};
use tempfile::TempDir;

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/schema.rs"]
mod schema;

use fixtures::{private_dir, quiet_factory, spawn_daemon, spawn_daemon_with_caps};

fn felis(home: &TempDir, args: &[&std::ffi::OsStr]) -> Output {
    let bin = std::env::var("CARGO_BIN_EXE_felis").expect("cargo sets CARGO_BIN_EXE_felis");
    StdCommand::new(bin)
        .env_remove("FELIS_SOCKET")
        .env("HOME", home.path())
        .args(args)
        .output()
        .expect("run felis")
}

fn version_verb(socket: &std::path::Path, home: &TempDir, extra: &[&str]) -> String {
    let mut args: Vec<&std::ffi::OsStr> =
        vec!["--socket".as_ref(), socket.as_ref(), "version".as_ref()];
    args.extend(extra.iter().copied().map(std::ffi::OsStr::new));
    String::from_utf8(felis(home, &args).stdout).unwrap()
}

fn daemon_row(socket: &std::path::Path, home: &TempDir) -> String {
    version_verb(socket, home, &[])
        .lines()
        .find(|l| l.starts_with("daemon"))
        .expect("a daemon row")
        .to_owned()
}

/// The one criterion `--version` exists to meet: it answers where
/// nothing else on the machine does. An empty `PATH` removes the GUI
/// client, and the socket path stays absent, so nothing was dialed and
/// nothing was spawned to dial.
#[test]
fn the_self_report_needs_no_daemon_no_client_and_no_path() {
    let home = private_dir();
    let socket = home.path().join("cold.sock");
    let bin = std::env::var("CARGO_BIN_EXE_felis").unwrap();
    // Through the environment stamp rather than `--socket`: the flag
    // is refused beside `--version`, and this test is about what the
    // report touches, not about where the socket was named.
    let out = StdCommand::new(bin)
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "")
        .env("FELIS_SOCKET", &socket)
        .arg("--version")
        .output()
        .expect("run felis --version");

    assert_eq!(out.status.code(), Some(0));
    assert!(!socket.exists(), "--version created a socket");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line = stdout.trim();
    let (name, identity) = line.split_once(' ').expect("`felis <identity>`");
    assert_eq!(name, "felis");
    let (semver, revision) = identity.split_once(' ').expect("`<semver> (<revision>)`");
    assert_eq!(semver, env!("CARGO_PKG_VERSION"));
    let revision = revision
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .expect("the revision is parenthesized");
    let revision = revision.strip_suffix("-dirty").unwrap_or(revision);
    assert!(
        revision == "unknown" || revision.chars().all(|c| c.is_ascii_hexdigit()),
        "{line}"
    );
}

/// `--version` is exclusive, as the standard flag is everywhere else:
/// a subcommand silently ignored beside it hides a typo behind an exit
/// 0. clap's usage errors exit 2 (`docs/reference/cli.md` "Exit
/// codes").
#[test]
fn the_self_report_refuses_to_share_the_line_with_a_command() {
    let home = private_dir();
    for args in [
        vec!["--version", "sessions", "list"],
        vec!["--version", "--", "htop"],
    ] {
        let argv: Vec<&std::ffi::OsStr> = args.iter().copied().map(std::ffi::OsStr::new).collect();
        let out = felis(&home, &argv);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.contains(
                "`--version` reports this build and runs nothing else; drop it to run the \
                 command, or use `felis version` to compare builds"
            ),
            "{stderr}"
        );
    }
}

/// The globals select what a verb reads or dials, and this flag does
/// neither: dropping them silently would let a user believe they had
/// asked about the remote build (`docs/reference/cli.md` "Global
/// options").
#[test]
fn the_self_report_refuses_the_globals_it_could_not_honor() {
    let home = private_dir();
    for args in [
        vec!["--config", "work.toml", "--version"],
        vec!["--socket", "/tmp/felis-x.sock", "--version"],
        vec!["--host", "vm", "--ssh-arg=-p", "--version"],
    ] {
        let argv: Vec<&std::ffi::OsStr> = args.iter().copied().map(std::ffi::OsStr::new).collect();
        let out = felis(&home, &argv);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.contains("it reads no config.toml and dials no daemon"),
            "{stderr}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_row_reads_not_running_only_when_nothing_answers() {
    let home = private_dir();
    let row = daemon_row(&home.path().join("cold.sock"), &home);
    assert!(row.contains("not running"), "{row}");
}

/// A full daemon is the daemon whose build this row exists to report;
/// "not running" would deny it is there at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_row_says_at_capacity_rather_than_not_running() {
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

    let home = private_dir();
    let row = daemon_row(&socket, &home);
    assert!(row.contains("at capacity"), "{row}");
    assert!(!row.contains("not running"), "{row}");
}

/// The machine form carries semver, revision and dirty state as three
/// fields, not one string to scrape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_machine_form_carries_a_typed_identity_per_process() {
    let tmp = private_dir();
    let (_server, socket, _pool) = spawn_daemon(&tmp, quiet_factory()).await;
    let home = private_dir();

    let out = version_verb(&socket, &home, &["--format", "json"]);
    let json: serde_json::Value = serde_json::from_str(&out).expect("one JSON object");
    schema::assert_cli_object(&json);
    assert_eq!(json["v"], 1);
    assert_eq!(json["cli"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(json["cli"]["revision"].is_string());
    assert!(json["cli"]["dirty"].is_boolean());
    assert_eq!(json["daemon_status"], "ok");
    assert_eq!(json["daemon"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["daemon"]["revision"], json["cli"]["revision"]);
    assert_eq!(json["daemon"]["dirty"], json["cli"]["dirty"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_machine_form_reports_an_absent_daemon_as_a_null_row() {
    let home = private_dir();
    let out = version_verb(&home.path().join("cold.sock"), &home, &["--format", "json"]);
    let json: serde_json::Value = serde_json::from_str(&out).expect("one JSON object");
    schema::assert_cli_object(&json);
    assert_eq!(json["daemon_status"], "not_running");
    assert!(json["daemon"].is_null());
}

//! `felis --version` and `felis version` integration tests. Only the
//! daemon row is pinned by content: the cli and client identities
//! depend on the build the suite runs from
//! (`docs/reference/cli.md` "Version reporting").

use std::process::{Command as StdCommand, Output};

use tempfile::TempDir;

use crate::common::{
    cli::{cli_bin, cli_command, cli_on},
    fixtures, schema,
};

use fixtures::{full_daemon, private_dir, quiet_factory, spawn_daemon};

fn felis(home: &TempDir, args: &[&str]) -> Output {
    cli_command()
        .env("HOME", home.path())
        .args(args)
        .output()
        .expect("run felis")
}

fn version_verb(socket: &std::path::Path, home: &TempDir, extra: &[&str]) -> Output {
    cli_on(socket)
        .env("HOME", home.path())
        .arg("version")
        .args(extra)
        .output()
        .expect("run felis version")
}

fn daemon_row(socket: &std::path::Path, home: &TempDir) -> String {
    String::from_utf8(version_verb(socket, home, &[]).stdout)
        .unwrap()
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
    // Through the environment stamp rather than `--socket`: the flag
    // is refused beside `--version`, and this test is about what the
    // report touches, not about where the socket was named.
    let out = StdCommand::new(cli_bin())
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
        let out = felis(&home, &args);
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

/// The globals select what a verb reads, dials, or opens, and this flag
/// does none of it: dropping them silently would let a user believe they had
/// asked about the remote build (`docs/reference/cli.md` "Global
/// options").
#[test]
fn the_self_report_refuses_the_globals_it_could_not_honor() {
    let home = private_dir();
    for args in [
        vec!["--config", "work.toml", "--version"],
        vec!["--socket", "/tmp/felis-x.sock", "--version"],
        vec!["--host", "vm", "--ssh-arg=-p", "--version"],
        vec!["--class", "scratch", "--version"],
    ] {
        let out = felis(&home, &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.contains("it reads no config.toml, dials no daemon, and opens no window"),
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
    let (_server, socket, _pool) = full_daemon(&tmp);

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
    let (_server, socket, _pool) = spawn_daemon(&tmp, quiet_factory());
    let home = private_dir();

    let json = schema::stdout_object(&version_verb(&socket, &home, &["--format", "json"]));
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
    let json = schema::stdout_object(&out);
    assert_eq!(json["daemon_status"], "not_running");
    assert!(json["daemon"].is_null());
}

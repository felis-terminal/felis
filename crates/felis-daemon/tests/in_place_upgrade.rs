//! The real `felis-daemon` replacing itself in place
//! (`docs/explanation/architecture/overview.md` "In-place upgrade").

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command as StdCommand};
use std::time::Duration;

use felis_client_core::{Offer, connect};
use felis_protocol::messages::{SpawnArgs, UpgradeOutcome};
use tempfile::TempDir;

mod common;

fn private_dir() -> TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = TempDir::new().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

struct DaemonProc(Child);

impl Drop for DaemonProc {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

fn spawn_real_daemon(tmp: &Path, socket: &Path) -> DaemonProc {
    let child = StdCommand::new(common::daemon_bin())
        .args(["serve", "--socket"])
        .arg(socket)
        .env("XDG_STATE_HOME", tmp.join("state"))
        .env("RUST_LOG", "off")
        .env_remove("NOTIFY_SOCKET")
        .spawn()
        .expect("spawn felis-daemon");
    DaemonProc(child)
}

/// An upgrade keeps the daemon's pid, the session's id and its running
/// shell: the shell goes on writing after the successor took over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upgrade_keeps_the_daemon_pid_and_the_running_session() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");
    let ticks = tmp.path().join("ticks");
    let mut daemon = spawn_real_daemon(tmp.path(), &socket);
    common::wait_connectable(&socket).await;

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    let info = conn
        .spawn_session(SpawnArgs {
            command: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                format!(
                    "while :; do echo tick >> '{}'; sleep 0.05; done",
                    ticks.display()
                ),
            ],
            ..SpawnArgs::default()
        })
        .await
        .unwrap();
    common::wait_for_more_lines(&ticks, 0).await;

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    let outcome = conn
        .daemon_upgrade(common::daemon_bin())
        .await
        .expect("the upgrade is answered");
    assert_eq!(outcome, UpgradeOutcome::Upgrading);
    drop(conn);

    common::wait_connectable(&socket).await;
    let mut after = connect(&socket, Offer::ops()).await.unwrap();
    let sessions = after.list_sessions().await.unwrap();
    assert_eq!(
        sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![info.id],
        "the successor serves the same session under the same id"
    );
    assert_eq!(
        daemon.0.try_wait().unwrap(),
        None,
        "the daemon process the test started is still the one serving"
    );
    let before = common::lines(&ticks);
    common::wait_for_more_lines(&ticks, before).await;
}

/// Grid frames from an attach until the rehydrate burst ends, mirrored
/// the way a window mirrors them.
/// What the session printed before the upgrade is on its screen after
/// it, and so is what the program set: the successor restores the grid
/// rather than starting it blank.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upgrade_keeps_the_screen_a_session_drew() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");
    let _daemon = spawn_real_daemon(tmp.path(), &socket);
    common::wait_connectable(&socket).await;

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    let info = conn
        .spawn_session(SpawnArgs {
            command: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf 'drawn before the upgrade\\r\\n\\033[1mbold\\033]2;kept-title\\007'; \
                 exec sleep 600"
                    .to_owned(),
            ],
            ..SpawnArgs::default()
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sessions = conn.list_sessions().await.unwrap();
        if sessions[0].title.as_deref() == Some("kept-title") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session never drew"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    let outcome = conn.daemon_upgrade(common::daemon_bin()).await.unwrap();
    assert_eq!(outcome, UpgradeOutcome::Upgrading);
    drop(conn);
    common::wait_connectable(&socket).await;

    let shadow = common::rehydrated_screen(&socket, info.id).await;
    assert!(
        common::shadow_contains(&shadow, "drawn before the upgrade"),
        "{:?}",
        common::shadow_rows(&shadow)
    );
    assert_eq!(shadow.title(), Some("kept-title"));
    let mut after = connect(&socket, Offer::ops()).await.unwrap();
    let sessions = after.list_sessions().await.unwrap();
    assert_eq!(sessions[0].title.as_deref(), Some("kept-title"));
}

/// A program that reported it is blocked, or how far it has got, is
/// still listed so after an upgrade: it has no reason to report again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upgrade_keeps_the_program_status_and_progress_a_session_reported() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");
    let _daemon = spawn_real_daemon(tmp.path(), &socket);
    common::wait_connectable(&socket).await;

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    conn.spawn_session(SpawnArgs {
        command: "/bin/sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            "printf '\\033]7501;state=blocked:kind=question\\033\\\\\\033]9;4;1;40\\007'; exec sleep 600"
                .to_owned(),
        ],
        ..SpawnArgs::default()
    })
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let before = loop {
        let sessions = conn.list_sessions().await.unwrap();
        if !sessions[0].program_status.is_empty() && sessions[0].terminal_progress.is_some() {
            break (
                sessions[0].program_status.clone(),
                sessions[0].terminal_progress.clone(),
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the program's report never landed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(before.0[0].state, "blocked");
    assert_eq!(before.1.as_ref().and_then(|p| p.value), Some(40));

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    let outcome = conn.daemon_upgrade(common::daemon_bin()).await.unwrap();
    assert_eq!(outcome, UpgradeOutcome::Upgrading);
    drop(conn);
    common::wait_connectable(&socket).await;

    let mut after = connect(&socket, Offer::ops()).await.unwrap();
    let sessions = after.list_sessions().await.unwrap();
    assert_eq!(
        (
            sessions[0].program_status.clone(),
            sessions[0].terminal_progress.clone()
        ),
        before
    );
}

/// A successor that cannot answer the probe refuses the upgrade, and
/// the daemon keeps serving its sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successor_that_fails_the_probe_leaves_the_daemon_serving() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");
    let _daemon = spawn_real_daemon(tmp.path(), &socket);
    common::wait_connectable(&socket).await;

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    let info = conn.spawn_session(SpawnArgs::default()).await.unwrap();
    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    let outcome = conn
        .daemon_upgrade("/bin/false".to_owned())
        .await
        .expect("the refusal is answered");
    assert!(
        matches!(outcome, UpgradeOutcome::Refused { .. }),
        "{outcome:?}"
    );

    let mut after = connect(&socket, Offer::ops()).await.unwrap();
    let sessions = after.list_sessions().await.unwrap();
    assert_eq!(
        sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![info.id]
    );
    let created = after.spawn_session(SpawnArgs::default()).await;
    assert!(created.is_ok(), "the refused daemon admits creates again");
}

/// An idle notification subscriber does not hold the upgrade off: its
/// stream ends at the exec, which the design accepts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_notification_subscriber_does_not_block_the_upgrade() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");
    let _daemon = spawn_real_daemon(tmp.path(), &socket);
    common::wait_connectable(&socket).await;

    let mut observer = connect(&socket, Offer::observer()).await.unwrap();
    observer.subscribe_notifications(None).await.unwrap();

    let mut conn = connect(&socket, Offer::ops()).await.unwrap();
    let outcome = conn
        .daemon_upgrade(common::daemon_bin())
        .await
        .expect("the upgrade is answered");
    assert_eq!(outcome, UpgradeOutcome::Upgrading);
    drop(observer);
}

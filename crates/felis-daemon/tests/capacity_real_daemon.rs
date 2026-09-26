//! Real-binary capacity and robustness probes spawning `felis-daemon serve` directly.
//!
//! Tests daemon fd limits against its own process instead of the test runner.
//! Both tests are ignored; run on demand on macOS with `--run-ignored all`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Child, Command as StdCommand};
use std::time::Duration;

use felis_client_core::{Offer, connect};
use felis_protocol::messages::SpawnArgs;
use tempfile::TempDir;

mod common;

/// A socket parent must be a `0700` directory this uid owns
/// (REQ-107), and `TempDir` follows the process umask.
fn private_dir() -> TempDir {
    let tmp = TempDir::new().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    tmp
}

struct DaemonProc(Child);

impl DaemonProc {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn is_alive(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

async fn spawn_real_daemon(socket: &Path, nofile: Option<u64>) -> DaemonProc {
    let bin = common::daemon_bin();
    // `exec` replaces the shell image, so the `Child` is the daemon's
    // own PID and kill / id target it directly.
    let inner = format!("exec '{bin}' serve --socket '{}'", socket.display());
    let script = match nofile {
        Some(n) => format!("ulimit -n {n}; {inner}"),
        None => inner,
    };
    let child = StdCommand::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .env("PATH", "/bin:/usr/bin")
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm-256color")
        .env("RUST_LOG", "off")
        .spawn()
        .expect("spawn felis-daemon");
    let proc = DaemonProc(child);
    for _ in 0..400 {
        if socket.exists() {
            return proc;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("daemon socket {} never appeared", socket.display());
}

fn fd_count(pid: u32) -> usize {
    let out = StdCommand::new("/usr/sbin/lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .expect("run lsof");
    // lsof prints one column-header line, then one line per fd.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .count()
        .saturating_sub(1)
}

/// The session is registered in the pool at create time, so a second
/// ops connection can destroy it by id immediately.
async fn churn_once(socket: &Path) {
    let mut conn = connect(socket, Offer::ops()).await.unwrap();
    let id = conn.create_with(SpawnArgs::default()).await.unwrap().id;
    drop(conn);
    let mut killer = connect(socket, Offer::ops()).await.unwrap();
    let resolved = killer
        .destroy_session(format!("{}", felis_protocol::SessionHex(id)))
        .await
        .unwrap();
    assert_eq!(
        resolved,
        felis_protocol::messages::ResolvedId::Ok { id },
        "the just-created session must exist to destroy",
    );
}

/// Pins serve.rs's contract that a factory spawn failure drops only
/// the offending connection: fd exhaustion is a per-connection error,
/// never a hang or a daemon crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns the real felis-daemon under a low fd ceiling; run with --run-ignored"]
async fn fd_exhaustion_degrades_gracefully_without_killing_the_daemon() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");
    // 128: per-session PTYs exhaust the table within a few hundred
    // creates, yet the daemon still boots.
    let mut daemon = spawn_real_daemon(&socket, Some(128)).await;

    let mut created = 0usize;
    let mut hit_graceful_error = false;
    for _ in 0..400 {
        let Ok(mut conn) = connect(&socket, Offer::ops()).await else {
            hit_graceful_error = true;
            break;
        };
        if conn.create_with(SpawnArgs::default()).await.is_ok() {
            created += 1;
            drop(conn);
        } else {
            hit_graceful_error = true;
            break;
        }
    }
    eprintln!("fd-exhaustion: created {created} sessions before the ceiling");

    assert!(
        created >= 1,
        "the daemon must serve at least one session before the ceiling",
    );
    assert!(
        hit_graceful_error,
        "the fd ceiling must surface as a graceful error within 400 attempts, not a hang; \
         created={created}",
    );
    assert!(
        daemon.is_alive(),
        "daemon must survive fd exhaustion (accept loop logs and continues)",
    );
}

/// Pins that create/destroy churn returns every descriptor (no
/// un-closed PTY master on teardown).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns the real felis-daemon and samples fds via lsof; run with --run-ignored"]
async fn session_churn_does_not_leak_file_descriptors() {
    let tmp = private_dir();
    let socket = tmp.path().join("daemon.sock");
    let mut daemon = spawn_real_daemon(&socket, None).await;
    let pid = daemon.pid();

    // Sample the baseline only after tokio workers and the first PTY
    // cycle are up, or it is understated.
    churn_once(&socket).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let baseline = fd_count(pid);

    for _ in 0..80 {
        churn_once(&socket).await;
    }
    // The session task reaps after DestroySession returns; let the
    // last cycles' fds drain before sampling.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = fd_count(pid);
    eprintln!("churn fd count: baseline={baseline} after={after}");

    assert!(daemon.is_alive(), "daemon must survive the churn");
    // A per-cycle leak would add ~80 fds; +8 absorbs sampling noise.
    assert!(
        after <= baseline + 8,
        "fd count grew from {baseline} to {after} over 80 create/destroy cycles — \
         likely a per-session descriptor leak",
    );
}

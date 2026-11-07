//! A refused `--class` ends the real `felis-client` with exit 2 before
//! it dials: the daemon it names gains no session. Neither case reaches
//! window creation, so no GPU or display is needed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use felis_client_core::{Offer, connect};

struct Daemon {
    child: Child,
    endpoint: PathBuf,
    dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn sibling_bin(stem: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_felis-client"))
        .with_file_name(format!("{stem}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "{} is built by a workspace test run",
        path.display()
    );
    path
}

#[cfg(unix)]
fn endpoint(dir: &Path) -> PathBuf {
    dir.join("daemon.sock")
}

#[cfg(windows)]
fn endpoint(_dir: &Path) -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\felis-test-window-class-{}",
        std::process::id()
    ))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn spawn_daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    // A socket parent must be a `0700` directory this uid owns, and
    // `TempDir` follows the process umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let endpoint = endpoint(dir.path());
    let child = Command::new(sibling_bin("felis-daemon"))
        .args(["serve", "--socket"])
        .arg(&endpoint)
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("RUST_LOG", "off")
        .env_remove("NOTIFY_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn felis-daemon");
    let daemon = Daemon {
        child,
        endpoint,
        dir,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while session_count(&daemon).is_none() {
        assert!(Instant::now() < deadline, "the daemon never answered");
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon
}

fn session_count(daemon: &Daemon) -> Option<usize> {
    runtime().block_on(async {
        let mut conn = connect(&daemon.endpoint, Offer::ops()).await.ok()?;
        conn.list_sessions().await.ok().map(|s| s.len())
    })
}

fn launch(daemon: &Daemon, class: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_felis-client"))
        .arg("--socket")
        .arg(&daemon.endpoint)
        .args(["--class", class])
        .env("XDG_STATE_HOME", daemon.dir.path().join("state"))
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .output()
        .expect("run felis-client")
}

fn assert_refused_without_a_session(class: &str, reason: &str) {
    let daemon = spawn_daemon();
    assert_eq!(session_count(&daemon), Some(0));
    let out = launch(&daemon, class);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{class:?}: {stderr}");
    assert!(stderr.contains(reason), "{class:?}: {stderr}");
    assert_eq!(session_count(&daemon), Some(0), "{class:?} left a session");
}

#[test]
fn a_class_with_a_control_character_is_refused_before_the_dial() {
    assert_refused_without_a_session("a\u{1b}b", "control characters");
}

#[cfg(windows)]
#[test]
fn a_registered_win32_class_is_refused_before_the_dial() {
    assert_refused_without_a_session("Button", "already a registered window class");
    assert_refused_without_a_session("winit thread event target", "event loop");
}

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(not(feature = "daemon-tests"), allow(unused))]

use felis_protocol::messages::{GridDims, SessionInfo};

/// A running 80x24 session with nothing reported about it.
pub(crate) fn session_info(id: u128) -> SessionInfo {
    SessionInfo {
        id,
        dims: GridDims {
            rows: 24,
            cols: 80,
            pixel_w: 0,
            pixel_h: 0,
        },
        title: None,
        cwd: None,
        spawn_cwd: None,
        terminal_progress: None,
        idle_seconds: None,
        tags: Vec::new(),
        last_notification: None,
        foreground: None,
        exited: false,
        last_exit_code: None,
        attachments: Vec::new(),
        sequence: std::num::NonZeroU64::MIN,
        program_status: Vec::new(),
    }
}

/// A socket parent must be a `0700` directory this uid owns
/// (REQ-107), and `TempDir` follows the process umask.
#[cfg(all(unix, feature = "native"))]
pub(crate) fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

/// An in-process daemon, bound before it is spawned so a connect the
/// caller makes next is queued rather than refused.
#[cfg(all(unix, feature = "daemon-tests"))]
pub(crate) fn serve_bound(
    path: &std::path::Path,
    caps: felis_daemon::serve::DaemonCaps,
    pool: std::sync::Arc<tokio::sync::Mutex<felis_daemon::SessionPool>>,
    factory: felis_daemon::serve::SessionFactory,
) -> tokio::task::JoinHandle<()> {
    let serving = felis_daemon::serve::bind_unix(path, caps, pool, factory)
        .expect("the test daemon binds its socket");
    tokio::spawn(async move { drop(serving.await) })
}

/// `/bin/sh -c script` under a pinned `PATH`. Park the shell with the
/// `read` builtin, not `sleep`: a host with no FHS `/bin` resolves no
/// `sleep` under this `PATH`.
#[cfg(all(unix, feature = "daemon-tests"))]
pub(crate) fn sh_factory(script: &'static str) -> felis_daemon::serve::SessionFactory {
    std::sync::Arc::new(move |_| {
        let mut cmd = felis_pty::Command::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    })
}

/// An in-process daemon whose sessions run `script`, with the pool it
/// serves so a test can watch the daemon's side of a session.
#[cfg(all(unix, feature = "daemon-tests"))]
pub(crate) fn daemon_with_pool(
    script: &'static str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::sync::Arc<tokio::sync::Mutex<felis_daemon::SessionPool>>,
    tokio::task::JoinHandle<()>,
) {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = std::sync::Arc::new(tokio::sync::Mutex::new(felis_daemon::SessionPool::new()));
    let server = serve_bound(
        &path,
        felis_daemon::serve::DaemonCaps::default(),
        pool.clone(),
        sh_factory(script),
    );
    (tmp, path, pool, server)
}

#[cfg(all(unix, feature = "daemon-tests"))]
pub(crate) async fn daemon_running(
    script: &'static str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    tokio::task::JoinHandle<()>,
) {
    let (tmp, path, _pool, server) = daemon_with_pool(script);
    (tmp, path, server)
}

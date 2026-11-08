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

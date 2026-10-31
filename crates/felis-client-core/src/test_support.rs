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

/// Waits for an in-process daemon to bind `path`, failing the test if it
/// never does.
#[cfg(all(unix, feature = "native"))]
pub(crate) async fn await_socket(path: &std::path::Path) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !path.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the test daemon never bound {}",
            path.display(),
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(not(feature = "daemon-tests"), allow(unused))]

use std::path::Path;
use std::time::Duration;

use tempfile::TempDir;

/// A socket parent must be a `0700` directory this uid owns
/// (REQ-107), and `TempDir` follows the process umask.
pub(crate) fn private_dir() -> TempDir {
    let tmp = TempDir::new().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    tmp
}

/// Waits for an in-process daemon to bind `path`, failing the test if it
/// never does.
pub(crate) async fn await_socket(path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the test daemon never bound {}",
            path.display(),
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

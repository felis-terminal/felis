#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use felis_daemon::{
    SessionPool,
    serve::{DaemonCaps, SessionFactory, serve_unix_with_factory},
};
#[cfg(unix)]
use felis_protocol::messages::SpawnArgs;
#[cfg(unix)]
use felis_pty::Command as PtyCommand;
use tempfile::TempDir;
#[cfg(unix)]
use tokio::sync::Mutex;

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

#[cfg(unix)]
pub(crate) fn quiet_factory() -> SessionFactory {
    Arc::new(|_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args(["-c", "read x"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        cmd
    })
}

#[cfg(unix)]
pub(crate) async fn spawn_daemon(
    tmp: &TempDir,
    factory: SessionFactory,
) -> (
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
    Arc<Mutex<SessionPool>>,
) {
    spawn_daemon_with_caps(tmp, factory, DaemonCaps::default()).await
}

#[cfg(unix)]
pub(crate) async fn spawn_daemon_with_caps(
    tmp: &TempDir,
    factory: SessionFactory,
    caps: DaemonCaps,
) -> (
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
    Arc<Mutex<SessionPool>>,
) {
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let server_path = path.clone();
    let server_pool = pool.clone();
    let handle = tokio::spawn(async move {
        drop(serve_unix_with_factory(&server_path, caps, server_pool, factory).await);
    });
    // Readiness is an accepted connection, not an existing path:
    // `bind` publishes the inode before `listen`, so a path probe can
    // hand the CLI ECONNREFUSED.
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            return (handle, path, pool);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("daemon on {} never accepted a connection", path.display());
}

#[cfg(unix)]
pub(crate) async fn create_and_detach(socket: &std::path::Path) -> u128 {
    use felis_client_core::{Offer, connect};
    let mut conn = connect(socket, Offer::ops()).await.unwrap();
    let id = conn.create_with(SpawnArgs::default()).await.unwrap().id;
    drop(conn);
    let mut lister = connect(socket, Offer::ops()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sessions = lister.list_sessions().await.unwrap();
        let session = sessions
            .iter()
            .find(|s| s.id == id)
            .expect("created session is listed");
        if session.idle_seconds.is_some() {
            return id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the creator's attachment never went away",
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Reruns a read verb until its stdout contains `needle`, returning that
/// stdout: the parked drain lands a child's output on its own schedule.
#[cfg(unix)]
pub(crate) async fn poll_until(socket: &std::path::Path, args: &[&str], needle: &str) -> String {
    let bin = std::env::var_os("CARGO_BIN_EXE_felis").expect("cargo sets the felis bin path");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let out = std::process::Command::new(&bin)
            .env_remove("FELIS_SOCKET")
            .env_remove("RUST_LOG")
            .arg("--socket")
            .arg(socket)
            .args(args)
            .output()
            .expect("run felis");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if stdout.contains(needle) {
            return stdout;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "`felis {}` never printed {needle:?}; last stdout: {stdout}",
            args.join(" "),
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

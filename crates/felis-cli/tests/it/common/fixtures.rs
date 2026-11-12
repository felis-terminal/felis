#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use super::cli::felis_out;
#[cfg(unix)]
use felis_daemon::{
    SessionPool,
    serve::{ConnectionAdmission, DaemonCaps, SessionFactory, bind_unix},
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

/// The FHS pair keeps first claim; the runner's own `PATH` is the tail
/// that lets a host with no FHS `/bin` (NixOS links only `sh` there)
/// resolve anything. Without it `sh` runs the body with the program
/// missing and reports nothing, so the test passes having exercised
/// nothing.
#[cfg(unix)]
pub(crate) fn fixture_path() -> std::ffi::OsString {
    let mut path = std::ffi::OsString::from("/bin:/usr/bin");
    if let Some(host) = std::env::var_os("PATH") {
        path.push(":");
        path.push(host);
    }
    path
}

/// Every session runs `script` as an `sh -c` body in a cleared
/// environment.
#[cfg(unix)]
pub(crate) fn sh_factory(script: impl Into<String>) -> SessionFactory {
    let script = script.into();
    Arc::new(move |_| {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args(["-c", &script]);
        cmd.env_clear();
        cmd.env("PATH", fixture_path());
        cmd
    })
}

#[cfg(unix)]
pub(crate) fn quiet_factory() -> SessionFactory {
    sh_factory("read x")
}

#[cfg(unix)]
pub(crate) fn spawn_daemon(
    tmp: &TempDir,
    factory: SessionFactory,
) -> (
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
    Arc<Mutex<SessionPool>>,
) {
    spawn_daemon_with_caps(tmp, factory, DaemonCaps::default())
}

#[cfg(unix)]
pub(crate) fn spawn_daemon_with_caps(
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
    // Bound before it is spawned: a path probe would race `listen`, and
    // a connect probe would take one of the admission permits `caps` sets.
    let serving =
        bind_unix(&path, caps, pool.clone(), factory).expect("the test daemon binds its socket");
    let handle = tokio::spawn(async move { drop(serving.await) });
    (handle, path, pool)
}

/// A daemon whose admission cap is zero: the refusal needs no racing
/// second dial to provoke.
#[cfg(unix)]
pub(crate) fn full_daemon(
    tmp: &TempDir,
) -> (
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
    Arc<Mutex<SessionPool>>,
) {
    let caps = DaemonCaps {
        admission: ConnectionAdmission::with_refusal_slots(0, 4),
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(tmp, quiet_factory(), caps)
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let out = felis_out(socket, args);
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

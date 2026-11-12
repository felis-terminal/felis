//! Fixtures the crate's unit tests share.
#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Duration;

/// The `PATH` this crate's shell fixtures hand their children. The FHS
/// pair keeps first claim; the runner's own `PATH` is the tail that
/// lets a host without an FHS `/bin` resolve anything (NixOS links only
/// `sh` there). Without it `sh` runs every fixture body with its
/// externals missing and reports no failure.
pub(crate) fn fixture_path() -> OsString {
    let mut path = OsString::from("/bin:/usr/bin");
    if let Some(host) = std::env::var_os("PATH") {
        path.push(":");
        path.push(host);
    }
    path
}

/// `/bin/sh -c body` with nothing in its environment but `path`.
pub(crate) fn sh(body: &str, path: impl AsRef<OsStr>) -> felis_pty::Command {
    let mut cmd = felis_pty::Command::new("/bin/sh");
    cmd.args(["-c", body]);
    cmd.env_clear();
    cmd.env("PATH", path);
    cmd
}

/// The pid a fixture shell wrote to `path` before `exec`ing, so the
/// process the daemon must reap.
pub(crate) async fn read_pid(path: &Path) -> i32 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Ok(pid) = raw.trim().parse()
        {
            return pid;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the fixture child never reported its pid to {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A zombie still answers the signal, so an exit the daemon has not
/// reaped reads as alive.
pub(crate) fn pid_alive(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

/// `false` when `pid` is still alive (or unreaped) after five seconds.
pub(crate) async fn waits_for_exit(pid: i32) -> bool {
    for _ in 0..200 {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// A 4-byte object under `name`, slash-prefixed the way a producer
/// creates it (mpv's `/mpv-kitty-<ptr>`). Sized by `ftruncate`, not
/// `write`: macOS shm fds accept only mmap/ftruncate/fstat/close.
pub(crate) fn create_shm(name: &str) {
    let fd = rustix::shm::open(
        name,
        rustix::shm::OFlags::CREATE | rustix::shm::OFlags::EXCL | rustix::shm::OFlags::RDWR,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .expect("create test shm");
    rustix::fs::ftruncate(&fd, 4).expect("size test shm");
}

pub(crate) fn shm_exists(name: &str) -> bool {
    rustix::shm::open(name, rustix::shm::OFlags::RDONLY, rustix::fs::Mode::empty()).is_ok()
}

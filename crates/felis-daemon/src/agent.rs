//! Stable `SSH_AUTH_SOCK` link for session children across reconnects.
//!
//! Children receive a daemon-owned symlink that repoints to the newest live
//! client forwarding path, surviving window detachment.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use felis_transport::local::Endpoint;
use tracing::warn;

pub const AGENT_ENV: &str = "SSH_AUTH_SOCK";

struct Registration {
    id: u64,
    target: PathBuf,
}

pub struct AgentLink {
    path: PathBuf,
    live: Mutex<Vec<Registration>>,
    next_id: AtomicU64,
}

#[expect(
    clippy::missing_fields_in_debug,
    reason = "the omission is the point; see the comment below"
)]
impl std::fmt::Debug for AgentLink {
    // Deliberately partial: a registration's target comes from a peer's
    // environment, and `Debug` is what a `?caps` in a tracing call
    // reaches for.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLink")
            .field("path", &self.path)
            .field("live", &self.lock().len())
            .finish()
    }
}

/// A live registration. Releasing it (on drop, when the relay
/// connection ends) restores the next-newest live target.
pub struct AgentLease {
    link: std::sync::Arc<AgentLink>,
    id: u64,
}

impl Drop for AgentLease {
    fn drop(&mut self) {
        self.link.release(self.id);
    }
}

impl AgentLink {
    /// Derive the link for a daemon serving `endpoint`.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::InvalidInput`] when the endpoint is relative,
    /// which would resolve differently across child working directories.
    #[cfg(unix)]
    pub fn for_endpoint(endpoint: &Endpoint) -> io::Result<Option<Self>> {
        let socket = endpoint.path();
        if !socket.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "daemon endpoint must be absolute to derive a stable {AGENT_ENV} path, got \
                     {}",
                    socket.display()
                ),
            ));
        }
        Ok(Some(Self {
            path: agent_path_for(socket),
            live: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        }))
    }

    #[cfg(windows)]
    #[expect(
        clippy::unnecessary_wraps,
        clippy::missing_const_for_fn,
        reason = "one signature with the Unix branch, which can fail and is not const"
    )]
    pub fn for_endpoint(_endpoint: &Endpoint) -> io::Result<Option<Self>> {
        Ok(None)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Point the link at `target` and hold it there until the returned
    /// lease drops.
    pub fn register(self: &std::sync::Arc<Self>, target: PathBuf) -> AgentLease {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut live = self.lock();
            live.push(Registration { id, target });
            self.repoint(&live);
        }
        AgentLease {
            link: std::sync::Arc::clone(self),
            id,
        }
    }

    fn release(&self, id: u64) {
        let mut live = self.lock();
        live.retain(|reg| reg.id != id);
        self.repoint(&live);
    }

    /// Point the link at the newest live target, or remove it when none is left.
    ///
    /// The list change and link write stay in one critical section so concurrent
    /// connection drops cannot race and leave the link pointing to a stale path.
    fn repoint(&self, live: &[Registration]) {
        let newest = live.last().map(|reg| reg.target.as_path());
        if let Err(err) = self.write_link(newest) {
            // The target is not logged: it came from a peer's environment.
            warn!(
                link = %self.path.display(),
                repointed = newest.is_some(),
                "could not update the stable agent link: {err}"
            );
        }
    }

    #[cfg(unix)]
    fn write_link(&self, target: Option<&Path>) -> io::Result<()> {
        let Some(target) = target else {
            return remove_if_present(&self.path);
        };
        // Staged and renamed over, not removed and recreated: a child may
        // resolve `SSH_AUTH_SOCK` in the gap. The staging name needs no
        // uniqueness because `repoint` holds the registration lock; one a
        // crash leaves behind is swept by `clear_stale`.
        let staging = staging_path(&self.path);
        remove_if_present(&staging)?;
        std::os::unix::fs::symlink(target, &staging)?;
        std::fs::rename(&staging, &self.path).inspect_err(|_| {
            drop(std::fs::remove_file(&staging));
        })
    }

    /// Remove a link a previous daemon on this endpoint left behind.
    /// Called at startup: the daemon has no shutdown hook, and a child
    /// resolving a stale link hangs against a dead peer rather than
    /// failing the way an unset `SSH_AUTH_SOCK` does.
    #[cfg(unix)]
    pub fn clear_stale(&self) {
        for path in [self.path.clone(), staging_path(&self.path)] {
            if let Err(err) = remove_if_present(&path) {
                warn!(
                    link = %path.display(),
                    "could not clear a stale agent link: {err}"
                );
            }
        }
    }

    #[cfg(windows)]
    #[expect(
        clippy::unused_self,
        clippy::missing_const_for_fn,
        reason = "one signature with the Unix branch, which reads the receiver and is not const"
    )]
    pub fn clear_stale(&self) {}

    #[cfg(windows)]
    #[expect(
        clippy::unnecessary_wraps,
        clippy::unused_self,
        clippy::missing_const_for_fn,
        reason = "one signature with the Unix branch, which reads the receiver, can fail and is not const"
    )]
    fn write_link(&self, _target: Option<&Path>) -> io::Result<()> {
        Ok(())
    }

    /// A poisoned lock still yields its data: the list is a plain vector
    /// of paths, and refusing agent forwarding for the daemon's remaining
    /// lifetime is the worse failure.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Registration>> {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(unix)]
fn agent_path_for(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_os_string();
    name.push(".agent");
    PathBuf::from(name)
}

/// A sibling of the link: `rename` is atomic only within one
/// filesystem.
#[cfg(unix)]
fn staging_path(link: &Path) -> PathBuf {
    let mut name = link.as_os_str().to_os_string();
    name.push(".new");
    PathBuf::from(name)
}

#[cfg(unix)]
fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;

    fn link_in(dir: &TempDir) -> Arc<AgentLink> {
        let endpoint = Endpoint::unix(dir.path().join("daemon.sock"));
        Arc::new(AgentLink::for_endpoint(&endpoint).unwrap().unwrap())
    }

    /// A child resolves `SSH_AUTH_SOCK` against its own cwd, so the
    /// derived path must be absolute.
    #[test]
    fn the_path_is_derived_from_the_endpoint_and_absolute() {
        let dir = TempDir::new().unwrap();
        let link = link_in(&dir);
        assert!(link.path().is_absolute());
        assert_eq!(link.path(), dir.path().join("daemon.sock.agent"));
    }

    #[test]
    fn a_relative_endpoint_is_rejected_at_startup() {
        let err = AgentLink::for_endpoint(&Endpoint::unix("daemon.sock")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    }

    #[test]
    fn the_newest_live_registration_wins() {
        let dir = TempDir::new().unwrap();
        let link = link_in(&dir);
        let _first = link.register(dir.path().join("agent.1"));
        assert_eq!(
            std::fs::read_link(link.path()).unwrap(),
            dir.path().join("agent.1")
        );
        let _second = link.register(dir.path().join("agent.2"));
        assert_eq!(
            std::fs::read_link(link.path()).unwrap(),
            dir.path().join("agent.2")
        );
    }

    /// A short-lived headless relay's teardown must not take a window's
    /// agent down with it.
    #[test]
    fn a_release_restores_the_next_newest() {
        let dir = TempDir::new().unwrap();
        let link = link_in(&dir);
        let window = link.register(dir.path().join("agent.window"));
        let headless = link.register(dir.path().join("agent.headless"));
        assert_eq!(
            std::fs::read_link(link.path()).unwrap(),
            dir.path().join("agent.headless")
        );
        drop(headless);
        assert_eq!(
            std::fs::read_link(link.path()).unwrap(),
            dir.path().join("agent.window"),
            "the window's registration comes back, it was never lost"
        );
        drop(window);
        assert!(
            !link.path().exists(),
            "with nothing live the link is removed, not left dangling"
        );
    }

    /// The list and the link move together under one lock.
    #[test]
    fn a_racing_teardown_never_strands_a_live_registration() {
        use std::sync::atomic::AtomicBool;

        let dir = TempDir::new().unwrap();
        let link = link_in(&dir);
        let keeper = dir.path().join("agent.keeper");
        let churn = dir.path().join("agent.churn");
        let _keeper = link.register(keeper.clone());

        let stop = Arc::new(AtomicBool::new(false));
        let observer = {
            let (link, stop) = (Arc::clone(&link), Arc::clone(&stop));
            let (keeper, churn) = (keeper.clone(), churn.clone());
            std::thread::spawn(move || {
                let mut seen_wrong = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    match std::fs::read_link(link.path()) {
                        Ok(target) if target == keeper || target == churn => {}
                        Ok(target) => seen_wrong.push(target.display().to_string()),
                        // APFS fails a lookup racing a `rename` over a
                        // symlink with `EINVAL`, a state no link write can
                        // leave behind; a stranded link reads as `ENOENT`.
                        #[cfg(target_os = "macos")]
                        Err(err) if err.raw_os_error() == Some(libc::EINVAL) => {}
                        Err(err) => seen_wrong.push(format!("no link: {err}")),
                    }
                }
                seen_wrong
            })
        };

        for _ in 0..2_000 {
            drop(link.register(churn.clone()));
        }
        stop.store(true, Ordering::Relaxed);
        let seen_wrong = observer.join().unwrap();
        assert!(
            seen_wrong.is_empty(),
            "the link left a live registration behind: {seen_wrong:?}"
        );
        assert_eq!(std::fs::read_link(link.path()).unwrap(), keeper);
    }

    /// A stale link would make a child hang against a dead peer instead
    /// of failing like an unset `SSH_AUTH_SOCK`.
    #[test]
    fn a_stale_link_is_cleared_at_startup() {
        let dir = TempDir::new().unwrap();
        let link = link_in(&dir);
        std::os::unix::fs::symlink(dir.path().join("agent.dead"), link.path()).unwrap();
        std::os::unix::fs::symlink(dir.path().join("agent.dead"), staging_path(link.path()))
            .unwrap();

        link.clear_stale();
        // `read_link`, not `exists`: `exists` follows the dangling link and
        // reports false either way.
        assert!(std::fs::read_link(link.path()).is_err());
        assert!(std::fs::read_link(staging_path(link.path())).is_err());
    }

    /// An older lease's drop must not overwrite a newer registration.
    #[test]
    fn an_older_releases_leaves_a_newer_registration_alone() {
        let dir = TempDir::new().unwrap();
        let link = link_in(&dir);
        let older = link.register(dir.path().join("agent.old"));
        let _newer = link.register(dir.path().join("agent.new"));
        drop(older);
        assert_eq!(
            std::fs::read_link(link.path()).unwrap(),
            dir.path().join("agent.new")
        );
    }
}

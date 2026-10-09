//! In-place daemon upgrade (`docs/explanation/architecture/overview.md`
//! "In-place upgrade").

#[cfg(unix)]
mod carrier;
pub mod dump;
#[cfg(unix)]
mod orchestrate;
#[cfg(unix)]
pub mod probe;
#[cfg(unix)]
pub mod restore;

use std::sync::Arc;
#[cfg(unix)]
use std::{path::PathBuf, sync::Mutex};

use tokio::sync::{RwLock, RwLockReadGuard, watch};

#[cfg(unix)]
pub use orchestrate::{Prepared, prepare};

/// Why an upgrade did not happen. Every refusal leaves the daemon
/// serving as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Unsupported(String),
    DumpVersion(String),
    ProtocolMajor(String),
    Timeout(String),
    ProbeFailed(String),
    Busy,
    Draining,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(detail)
            | Self::DumpVersion(detail)
            | Self::ProtocolMajor(detail)
            | Self::Timeout(detail)
            | Self::ProbeFailed(detail) => f.write_str(detail),
            Self::Busy => f.write_str("an upgrade is already running"),
            Self::Draining => f.write_str("the daemon is draining toward stop"),
        }
    }
}

/// The wire form of a refusal.
#[must_use]
pub fn refused(refusal: &Refusal) -> felis_protocol::messages::UpgradeOutcome {
    use felis_protocol::messages::UpgradeRefusal as Reason;
    let reason = match refusal {
        Refusal::Unsupported(_) => Reason::Unsupported,
        Refusal::DumpVersion(_) => Reason::DumpVersion,
        Refusal::ProtocolMajor(_) => Reason::ProtocolMajor,
        Refusal::Timeout(_) => Reason::Timeout,
        Refusal::ProbeFailed(_) => Reason::ProbeFailed,
        Refusal::Busy => Reason::Busy,
        Refusal::Draining => Reason::Draining,
    };
    felis_protocol::messages::UpgradeOutcome::Refused {
        reason,
        detail: refusal.to_string(),
    }
}

/// What the upgrade needs from the serving loop, filled once it is
/// listening.
#[derive(Debug, Default)]
pub struct UpgradeState {
    pub gate: UpgradeGate,
    /// Only the `felis-daemon` binary may exec its successor: a process
    /// embedding the daemon library, such as a test, would be replaced
    /// by a daemon it never meant to become.
    #[cfg(unix)]
    replaceable: bool,
    /// A duplicate of the listening socket, so the upgrade owns what it
    /// hands across the exec without borrowing the serving loop's.
    #[cfg(unix)]
    listener: Mutex<Option<(std::os::fd::OwnedFd, PathBuf)>>,
}

impl UpgradeState {
    /// A daemon that refuses every upgrade as unsupported.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The `felis-daemon` process's own state: an upgrade execs over it.
    #[must_use]
    pub fn replaceable() -> Arc<Self> {
        #[cfg(unix)]
        return Arc::new(Self {
            replaceable: true,
            ..Self::default()
        });
        #[cfg(not(unix))]
        Self::new()
    }

    /// Records the listening socket until [`Self::clear_listener`].
    #[cfg(unix)]
    pub fn set_listener(
        &self,
        fd: std::os::fd::BorrowedFd<'_>,
        socket: PathBuf,
    ) -> std::io::Result<()> {
        if !self.replaceable {
            return Ok(());
        }
        let held = (fd.try_clone_to_owned()?, socket);
        *self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(held);
        Ok(())
    }

    /// Closes the held copy: a daemon that stopped accepting must not
    /// leave dials queueing on a socket nobody accepts.
    #[cfg(unix)]
    pub fn clear_listener(&self) {
        self.listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    #[cfg(unix)]
    fn listener(&self) -> Option<(std::os::fd::OwnedFd, PathBuf)> {
        let held = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (fd, socket) = held.as_ref()?;
        Some((fd.try_clone().ok()?, socket.clone()))
    }
}

/// The barrier: closed, it stops the accept loop and every connection
/// from dispatching another frame. Session-bound commands are sent
/// under a dispatch permit, so closing waits for the ones already
/// being sent.
#[derive(Debug)]
pub struct UpgradeGate {
    closed: watch::Sender<bool>,
    dispatch: RwLock<()>,
}

impl Default for UpgradeGate {
    fn default() -> Self {
        Self {
            closed: watch::Sender::new(false),
            dispatch: RwLock::new(()),
        }
    }
}

impl UpgradeGate {
    #[must_use]
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    pub async fn wait_open(&self) {
        let mut rx = self.closed.subscribe();
        let _open = rx.wait_for(|closed| !closed).await;
    }

    pub async fn wait_closed(&self) {
        let mut rx = self.closed.subscribe();
        let _closed = rx.wait_for(|closed| *closed).await;
    }

    /// Held while a frame is handed to a session; never granted while
    /// the gate is closed.
    pub async fn dispatch(&self) -> RwLockReadGuard<'_, ()> {
        loop {
            self.wait_open().await;
            let permit = self.dispatch.read().await;
            if !self.is_closed() {
                return permit;
            }
        }
    }

    /// `false` when it was already closed. Returns once no dispatch
    /// permit is outstanding.
    pub async fn close(&self) -> bool {
        let mut was_open = false;
        self.closed.send_if_modified(|closed| {
            was_open = !*closed;
            *closed = true;
            was_open
        });
        if was_open {
            drop(self.dispatch.write().await);
        }
        was_open
    }

    pub fn open(&self) {
        self.closed
            .send_if_modified(|closed| std::mem::replace(closed, false));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn closing_waits_for_a_dispatch_already_under_way() {
        let gate = Arc::new(UpgradeGate::default());
        let permit = gate.dispatch().await;
        let closer = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.close().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !closer.is_finished(),
            "close waits for the outstanding permit"
        );
        drop(permit);
        assert!(closer.await.unwrap());
    }

    #[tokio::test]
    async fn no_dispatch_is_granted_while_closed() {
        let gate = Arc::new(UpgradeGate::default());
        assert!(gate.close().await);
        assert!(
            !gate.close().await,
            "a second close reports the gate already shut"
        );
        let waiter = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move {
                drop(gate.dispatch().await);
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        gate.open();
        waiter.await.unwrap();
    }
}

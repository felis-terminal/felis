//! In-place daemon upgrade (`docs/explanation/architecture/overview.md`
//! "In-place upgrade").

#[cfg(unix)]
mod carrier;
#[cfg(any(windows, test))]
mod channel;
#[doc(hidden)]
pub mod dump;
#[cfg(windows)]
mod handover;
mod orchestrate;
#[doc(hidden)]
pub mod probe;
#[doc(hidden)]
pub mod restore;
#[doc(hidden)]
#[cfg(windows)]
pub mod takeover;

#[cfg(unix)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::{RwLock, RwLockReadGuard, watch};

#[doc(hidden)]
#[cfg(windows)]
pub use handover::{Ready, prepare};
#[doc(hidden)]
#[cfg(unix)]
pub use orchestrate::{Prepared, prepare};

/// Why an upgrade did not happen. Every refusal leaves the daemon
/// serving as before.
#[doc(hidden)]
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

impl Refusal {
    /// The specifics the reason does not already say.
    fn detail(&self) -> &str {
        match self {
            Self::Unsupported(detail)
            | Self::DumpVersion(detail)
            | Self::ProtocolMajor(detail)
            | Self::Timeout(detail)
            | Self::ProbeFailed(detail) => detail,
            Self::Busy | Self::Draining => "",
        }
    }
}

/// The wire form of a refusal.
#[doc(hidden)]
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
        detail: refusal.detail().to_owned(),
    }
}

/// What the upgrade needs from the serving loop, filled once it is
/// listening.
#[derive(Debug, Default)]
pub struct UpgradeState {
    #[doc(hidden)]
    pub gate: UpgradeGate,
    /// Only the `felis-daemon` binary may hand over to its successor: a
    /// process embedding the daemon library, such as a test, would be
    /// replaced by a daemon it never meant to become.
    replaceable: bool,
    /// A duplicate of the listening socket, so the upgrade owns what it
    /// hands across the exec without borrowing the serving loop's.
    #[cfg(unix)]
    listener: Mutex<Option<(std::os::fd::OwnedFd, PathBuf)>>,
    /// The pipe name the serving loop listens on, which the successor
    /// joins.
    #[cfg(windows)]
    listener: Mutex<Option<String>>,
    /// The daemon that created the pipe name, held by each successor
    /// after it. Every instance reports that process as its server for
    /// as long as any instance lives, and a client opens it by pid to
    /// check who serves, so the handle keeps the pid from being reused.
    #[cfg(windows)]
    creator: Mutex<Option<felis_pty::handover::Process>>,
    /// Held by one upgrade from before its first probe until its exec
    /// or refusal.
    claimed: std::sync::atomic::AtomicBool,
}

impl UpgradeState {
    /// A daemon that refuses every upgrade as unsupported.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The `felis-daemon` process's own state: an upgrade execs over it.
    #[doc(hidden)]
    #[must_use]
    pub fn replaceable() -> Arc<Self> {
        Arc::new(Self {
            replaceable: true,
            ..Self::default()
        })
    }

    /// Records the listening socket until [`Self::clear_listener`].
    #[doc(hidden)]
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

    /// Holds the pipe name's creator for this process's lifetime.
    #[cfg(windows)]
    fn set_creator(&self, creator: felis_pty::handover::Process) {
        *self
            .creator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(creator);
    }

    /// The pipe name's creator, duplicated into `successor`: this
    /// process unless it took the name over itself.
    #[cfg(windows)]
    fn creator_into(&self, successor: &felis_pty::handover::Process) -> std::io::Result<u64> {
        match &*self
            .creator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            Some(creator) => creator.share_into(successor),
            None => felis_pty::handover::Process::current_into(successor),
        }
    }

    /// Records the pipe name until [`Self::clear_listener`].
    #[doc(hidden)]
    #[cfg(windows)]
    pub fn set_listener(&self, pipe_name: &str) {
        if self.replaceable {
            *self
                .listener
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pipe_name.to_owned());
        }
    }

    #[cfg(windows)]
    fn listener(&self) -> Option<String> {
        self.listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Closes the held copy: a daemon that stopped accepting must not
    /// leave dials queueing on a socket nobody accepts.
    #[doc(hidden)]
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
#[doc(hidden)]
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

    /// The client prints the reason itself; a detail repeating it would
    /// say it twice.
    #[test]
    fn a_busy_or_draining_refusal_carries_no_detail() {
        use felis_protocol::messages::UpgradeOutcome;
        for refusal in [Refusal::Busy, Refusal::Draining] {
            let UpgradeOutcome::Refused { detail, .. } = refused(&refusal) else {
                panic!("a refusal is refused");
            };
            assert_eq!(detail, "", "{refusal:?}");
        }
        let UpgradeOutcome::Refused { detail, .. } =
            refused(&Refusal::DumpVersion("reads 3, got 2".into()))
        else {
            panic!("a refusal is refused");
        };
        assert_eq!(detail, "reads 3, got 2");
    }

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

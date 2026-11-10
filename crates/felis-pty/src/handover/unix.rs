use std::{io, os::fd::OwnedFd, os::unix::process::ExitStatusExt as _, process::ExitStatus};

use crate::{ByteSink, ChildHandle, PtyError, PtyParts, Resizer, poison, session_over, unix};

/// A session's master and child, read in one snapshot: a duplicate of
/// the master, which the caller keeps open across `execve`, and the
/// child's pid with its exit status if it is already reaped.
pub struct Handover {
    master: OwnedFd,
    pid: i32,
    exit_status: Option<ExitStatus>,
}

impl Handover {
    /// The duplicate is close-on-exec; the caller clears that on the
    /// copy it means to carry.
    pub fn of(resizer: &Resizer, child: &ChildHandle) -> io::Result<Self> {
        let master = resizer.master.as_fd().try_clone_to_owned()?;
        let mut child = child.child.lock().map_err(poison)?;
        let pid = child
            .process_id()
            .ok_or_else(|| io::Error::other("the child's pid does not fit a pid_t"))?;
        let exit_status = child.try_wait()?;
        Ok(Self {
            master,
            pid,
            exit_status,
        })
    }

    #[must_use]
    pub const fn pid(&self) -> i32 {
        self.pid
    }

    /// `Some` once this process has reaped the child: the receiving
    /// side must then never signal or wait on the pid, which may be
    /// someone else's by then.
    #[must_use]
    pub const fn exit_status(&self) -> Option<ExitStatus> {
        self.exit_status
    }

    #[must_use]
    pub fn into_master(self) -> OwnedFd {
        self.master
    }
}

/// A carried session rebuilt in the receiving process but not running:
/// no thread reads its output or writes its input. Dropping it closes
/// the master, which hangs the child up if no other copy is open, and
/// never signals or waits on the child itself.
pub struct Adopted {
    master: unix::Master,
    child: unix::Child,
}

impl Adopted {
    /// Rebuilds a session from a master fd this process inherited and
    /// the child it is already the parent of. `exit_status` is the raw
    /// wait status of a child the handing-over side already reaped.
    pub fn inherited(
        master: OwnedFd,
        pid: i32,
        exit_status: Option<i32>,
    ) -> Result<Self, PtyError> {
        let master = unix::adopt_master(master)
            .map_err(|e| PtyError::OpenPty(format!("adopt master: {e}")))?;
        let child = unix::Child::adopted(pid, exit_status.map(ExitStatus::from_raw))
            .map_err(|e| PtyError::Spawn(format!("adopt child: {e}")))?;
        Ok(Self { master, child })
    }

    /// Starts the session's threads: from here it is this process's
    /// like any it spawned.
    pub fn start(self, sink: ByteSink) -> Result<PtyParts, PtyError> {
        session_over(self.master, self.child, sink)
    }
}

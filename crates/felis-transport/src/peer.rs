//! Peer-identity verification per `docs/explanation/security-model.md`
//! "Daemon IPC". Windows verifies the pipe client's token SID and
//! integrity level in `crate::windows` instead.

use thiserror::Error;

#[cfg(unix)]
use std::num::NonZeroU32;

#[cfg(unix)]
use std::os::fd::{AsFd as _, BorrowedFd};
#[cfg(unix)]
use tokio::net::UnixStream;

#[derive(Debug, Error)]
pub enum PeerError {
    #[error("read peer credentials: {0}")]
    Sockopt(#[from] std::io::Error),
    #[cfg(unix)]
    #[error("peer UID {peer} does not match expected UID {expected}")]
    UidMismatch { peer: u32, expected: u32 },
    #[cfg(windows)]
    #[error("peer SID {peer} does not match expected SID {expected}")]
    SidMismatch {
        /// Reported peer SID, in SDDL string form (`S-1-5-…`).
        peer: String,
        expected: String,
    },
    #[cfg(windows)]
    #[error("peer integrity level {peer} does not match expected integrity level {expected}")]
    IntegrityMismatch {
        /// Reported peer mandatory label, in SDDL string form (`S-1-16-…`).
        peer: String,
        expected: String,
    },
    #[error("peer-credential lookup is not implemented for this platform")]
    Unsupported,
}

#[cfg(target_os = "linux")]
pub fn peer_uid(fd: BorrowedFd<'_>) -> Result<u32, PeerError> {
    Ok(peercred(fd)?.uid)
}

/// Not rustix's `socket_peercred`: its `UCred.pid` is a non-zero type,
/// and the kernel reports pid 0 for a peer outside this pid namespace.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
fn peercred(fd: BorrowedFd<'_>) -> std::io::Result<libc::ucred> {
    use std::os::fd::AsRawFd as _;

    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = libc::socklen_t::try_from(size_of::<libc::ucred>())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `SO_PEERCRED` writes at most `len` bytes of a `ucred`
    // through the out-pointer, which points at a live, initialized,
    // unaliased stack `ucred` of exactly that size; `len` is a live
    // `socklen_t`. `fd` is borrowed for the whole call.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred)
}

#[cfg(felis_getpeereid)]
#[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
pub fn peer_uid(fd: BorrowedFd<'_>) -> Result<u32, PeerError> {
    use std::os::fd::AsRawFd as _;

    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `getpeereid` writes one `uid_t` and one `gid_t` through the
    // out-pointers and reads nothing else; both point at live, unaliased
    // stack slots. `fd` is borrowed for the whole call. On failure it
    // returns -1 and the branch below discards both slots.
    let rc = unsafe { libc::getpeereid(fd.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if rc != 0 {
        return Err(PeerError::Sockopt(std::io::Error::last_os_error()));
    }
    Ok(uid)
}

#[cfg(all(unix, not(target_os = "linux"), not(felis_getpeereid)))]
pub const fn peer_uid(_fd: BorrowedFd<'_>) -> Result<u32, PeerError> {
    Err(PeerError::Unsupported)
}

/// The process that connected the peer socket, read once from the
/// kernel: not whoever holds the socket now. `None` where the platform
/// has no such query, on a failed lookup, and for a peer whose pid is
/// not visible from here (Linux reports 0 across pid namespaces).
#[cfg(target_os = "linux")]
#[must_use]
pub fn peer_pid(fd: BorrowedFd<'_>) -> Option<NonZeroU32> {
    let pid = peercred(fd).ok()?.pid;
    NonZeroU32::new(u32::try_from(pid).ok()?)
}

#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
#[must_use]
pub fn peer_pid(fd: BorrowedFd<'_>) -> Option<NonZeroU32> {
    use std::os::fd::AsRawFd as _;

    let mut pid: libc::pid_t = 0;
    let mut len = libc::socklen_t::try_from(size_of::<libc::pid_t>()).ok()?;
    // SAFETY: `LOCAL_PEERPID` writes at most `len` bytes of a `pid_t`
    // through the out-pointer, which points at a live, unaliased stack
    // `pid_t` of exactly that size; `len` is a live `socklen_t`. `fd` is
    // borrowed for the whole call.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    NonZeroU32::new(u32::try_from(pid).ok()?)
}

#[cfg(all(unix, not(target_os = "linux"), not(target_vendor = "apple")))]
#[must_use]
pub const fn peer_pid(_fd: BorrowedFd<'_>) -> Option<NonZeroU32> {
    None
}

/// REQ-106 is symmetric, so this takes a descriptor rather than a
/// `UnixStream`: the daemon checks an accepted `tokio` stream, the
/// dialer checks the stream it just connected, and the startup probe
/// checks a synchronous `std` one.
#[cfg(unix)]
pub fn verify_peer_uid_of(fd: BorrowedFd<'_>, expected: u32) -> Result<(), PeerError> {
    let peer = peer_uid(fd)?;
    if peer == expected {
        Ok(())
    } else {
        Err(PeerError::UidMismatch { peer, expected })
    }
}

#[cfg(unix)]
pub fn verify_peer_uid(stream: &UnixStream, expected: u32) -> Result<(), PeerError> {
    verify_peer_uid_of(stream.as_fd(), expected)
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[cfg(any(target_os = "linux", felis_getpeereid))]
    #[tokio::test]
    async fn verify_peer_uid_passes_for_self_and_fails_for_other() {
        let (a, _b) = UnixStream::pair().unwrap();
        let me = rustix::process::geteuid().as_raw();
        verify_peer_uid(&a, me).expect("self-uid match");
        let bogus = if me == 0 { 65534 } else { 0 };
        match verify_peer_uid(&a, bogus) {
            Err(PeerError::UidMismatch { peer, expected }) => {
                assert_eq!(peer, me);
                assert_eq!(expected, bogus);
            }
            other => panic!("expected UidMismatch, got {other:?}"),
        }
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[tokio::test]
    async fn peer_pid_names_the_connecting_process() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(
            peer_pid(a.as_fd()).map(NonZeroU32::get),
            Some(std::process::id())
        );
    }
}

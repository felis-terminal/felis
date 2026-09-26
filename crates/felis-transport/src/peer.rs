//! Peer-identity verification per `docs/explanation/security-model.md`
//! "Daemon IPC". Windows verifies the pipe client's token SID in
//! `crate::windows` instead.

use thiserror::Error;

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
    #[error("peer-credential lookup is not implemented for this platform")]
    Unsupported,
}

#[cfg(target_os = "linux")]
pub fn peer_uid(fd: BorrowedFd<'_>) -> Result<u32, PeerError> {
    let cred = rustix::net::sockopt::socket_peercred(fd).map_err(std::io::Error::from)?;
    Ok(cred.uid.as_raw())
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
}

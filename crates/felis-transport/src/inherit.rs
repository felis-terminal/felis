//! Descriptors that cross an in-place upgrade's `execve`
//! (`docs/explanation/architecture/overview.md` "In-place upgrade").

use std::{
    io,
    os::fd::{BorrowedFd, FromRawFd as _, OwnedFd, RawFd},
};

use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};

/// Clears or restores close-on-exec on `fd`.
pub fn set_inheritable(fd: BorrowedFd<'_>, inheritable: bool) -> io::Result<()> {
    let flags = fcntl_getfd(fd).map_err(io::Error::from)?;
    let flags = if inheritable {
        flags.difference(FdFlags::CLOEXEC)
    } else {
        flags.union(FdFlags::CLOEXEC)
    };
    fcntl_setfd(fd, flags).map_err(io::Error::from)
}

/// Takes ownership of a descriptor this process inherited across its
/// own `execve`, and sets close-on-exec on it again. The caller names
/// each number at most once.
#[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
pub fn take_inherited(raw: RawFd) -> io::Result<OwnedFd> {
    if raw < 0 {
        return Err(io::Error::from_raw_os_error(
            rustix::io::Errno::BADF.raw_os_error(),
        ));
    }
    // SAFETY: the borrow lives only for the `fcntl` probe below, which
    // fails with `EBADF` rather than touching memory when `raw` names no
    // open descriptor.
    let probe = unsafe { BorrowedFd::borrow_raw(raw) };
    fcntl_getfd(probe).map_err(io::Error::from)?;
    // SAFETY: `raw` is open (probed above) and was carried across the
    // exec for this process to own. Nothing else here wraps it: before
    // the restore takes them, inherited numbers are only integers in the
    // dump, and the caller takes each number once.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    set_inheritable(std::os::fd::AsFd::as_fd(&owned), false)?;
    Ok(owned)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::fd::{AsFd as _, AsRawFd as _, IntoRawFd as _};

    use super::*;

    #[test]
    fn set_inheritable_toggles_close_on_exec() {
        let (read, _write) = rustix::pipe::pipe().unwrap();
        set_inheritable(read.as_fd(), true).unwrap();
        assert!(!fcntl_getfd(&read).unwrap().contains(FdFlags::CLOEXEC));
        set_inheritable(read.as_fd(), false).unwrap();
        assert!(fcntl_getfd(&read).unwrap().contains(FdFlags::CLOEXEC));
    }

    #[test]
    fn take_inherited_owns_an_open_descriptor_and_restores_close_on_exec() {
        let (read, _write) = rustix::pipe::pipe().unwrap();
        set_inheritable(read.as_fd(), true).unwrap();
        let raw = read.into_raw_fd();
        let owned = take_inherited(raw).unwrap();
        assert_eq!(owned.as_raw_fd(), raw);
        assert!(fcntl_getfd(&owned).unwrap().contains(FdFlags::CLOEXEC));
    }

    #[test]
    fn take_inherited_refuses_a_number_naming_no_descriptor() {
        let (read, _write) = rustix::pipe::pipe().unwrap();
        let raw = read.as_raw_fd();
        drop(read);
        assert!(take_inherited(raw).is_err());
        assert!(take_inherited(-1).is_err());
    }
}

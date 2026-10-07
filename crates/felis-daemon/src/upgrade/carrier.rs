//! The anonymous memory object the dump crosses `execve` in, never named
//! on a filesystem, so a session's screen never reaches a disk
//! (`docs/explanation/architecture/overview.md` "In-place upgrade"). It
//! leads with an 8-byte little-endian length: macOS rounds a shared-memory
//! object's size up to a page, so the size cannot say where the dump ends.

use std::{
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
};

const HEADER: usize = 8;

/// Writes `bytes` into a fresh anonymous memory object.
pub(super) fn create(bytes: &[u8]) -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    {
        create_memfd(bytes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        create_shm(bytes)
    }
}

/// Reads the dump back, wherever the descriptor's offset stands.
pub(super) fn read(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        read_pread(fd)
    }
    #[cfg(not(target_os = "linux"))]
    {
        read_mmap(fd)
    }
}

/// The probe reads the dump from its standard input: a duplicate of the
/// carrier, without the close-on-exec flag.
pub(super) fn probe_stdin(fd: &OwnedFd) -> io::Result<std::process::Stdio> {
    Ok(std::process::Stdio::from(fd.as_fd().try_clone_to_owned()?))
}

fn framed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + bytes.len());
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

/// The dump inside `object`, refusing a length the object cannot hold.
fn unframed(object: &[u8]) -> io::Result<&[u8]> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let header: [u8; HEADER] = object
        .get(..HEADER)
        .and_then(|header| header.try_into().ok())
        .ok_or_else(|| invalid("the upgrade carrier is shorter than its header"))?;
    let len = usize::try_from(u64::from_le_bytes(header))
        .map_err(|_| invalid("the upgrade carrier names an impossible length"))?;
    object
        .get(HEADER..)
        .and_then(|rest| rest.get(..len))
        .ok_or_else(|| invalid("the upgrade carrier is shorter than the length it names"))
}

#[cfg(target_os = "linux")]
fn create_memfd(bytes: &[u8]) -> io::Result<OwnedFd> {
    let fd = rustix::fs::memfd_create("felis-upgrade", rustix::fs::MemfdFlags::CLOEXEC)?;
    let framed = framed(bytes);
    let mut written = 0;
    while written < framed.len() {
        match rustix::io::write(&fd, &framed[written..]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => written += n,
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(fd)
}

#[cfg(target_os = "linux")]
fn read_pread(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    let mut object = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match rustix::io::pread(fd, &mut buf, object.len() as u64) {
            Ok(0) => break,
            Ok(n) => object.extend_from_slice(&buf[..n]),
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
    unframed(&object).map(<[u8]>::to_vec)
}

/// A POSIX shared-memory object, unlinked before anything is written to
/// it. Compiled on every Unix so Linux tests exercise the macOS path.
#[cfg(unix)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn create_shm(bytes: &[u8]) -> io::Result<OwnedFd> {
    create_shm_named(bytes).map(|(fd, _name)| fd)
}

#[cfg(unix)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn create_shm_named(bytes: &[u8]) -> io::Result<(OwnedFd, String)> {
    use rustix::{
        fs::Mode,
        shm::{self, OFlags},
    };

    let framed = framed(bytes);
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(|err| io::Error::other(err.to_string()))?;
    // macOS caps a shared-memory name at 31 bytes.
    let name = format!("/felis-up-{:016x}", u64::from_ne_bytes(suffix));
    let fd = shm::open(
        name.as_str(),
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
        Mode::RUSR | Mode::WUSR,
    )?;
    shm::unlink(name.as_str())?;
    felis_transport::inherit::set_inheritable(fd.as_fd(), false)?;
    rustix::fs::ftruncate(&fd, framed.len() as u64)?;
    let mut mapping = Mapping::new(fd.as_fd(), framed.len(), true)?;
    mapping
        .bytes_mut()
        .ok_or_else(|| io::Error::other("a writable mapping refused writes"))?
        .copy_from_slice(&framed);
    Ok((fd, name))
}

#[cfg(unix)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn read_mmap(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    let size = rustix::fs::fstat(fd)?.st_size;
    let size = usize::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative carrier size"))?;
    if size == 0 {
        return unframed(&[]).map(<[u8]>::to_vec);
    }
    let mapping = Mapping::new(fd, size, false)?;
    unframed(mapping.bytes()).map(<[u8]>::to_vec)
}

/// The first `len` bytes of a descriptor, mapped shared; unmapped on
/// drop. Only a mapping made writable hands out mutable bytes.
#[cfg(unix)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
struct Mapping {
    ptr: *mut core::ffi::c_void,
    len: usize,
    writable: bool,
}

#[cfg(unix)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[allow(unsafe_code)]
impl Mapping {
    fn new(fd: BorrowedFd<'_>, len: usize, writable: bool) -> io::Result<Self> {
        use rustix::mm::{MapFlags, ProtFlags};

        if len == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let prot = if writable {
            ProtFlags::READ | ProtFlags::WRITE
        } else {
            ProtFlags::READ
        };
        // SAFETY: `fd` is live for the call, the offset 0 is page-aligned,
        // and `len > 0`. A null hint lets the kernel place the mapping,
        // so it overlaps no Rust allocation.
        let ptr =
            unsafe { rustix::mm::mmap(core::ptr::null_mut(), len, prot, MapFlags::SHARED, fd, 0) }?;
        Ok(Self { ptr, len, writable })
    }

    const fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping spans exactly `[ptr, ptr + len)`, is
        // readable, and stays mapped until `drop`, which the returned
        // borrow of `self` cannot outlive. Nothing in this process writes
        // through it while the shared borrow lives, since `bytes_mut`
        // needs `&mut self`.
        unsafe { core::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }

    const fn bytes_mut(&mut self) -> Option<&mut [u8]> {
        if !self.writable {
            return None;
        }
        // SAFETY: as in `bytes`, and the mapping was made `PROT_WRITE`;
        // the exclusive borrow of `self` makes this the only reference.
        Some(unsafe { core::slice::from_raw_parts_mut(self.ptr.cast::<u8>(), self.len) })
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are the pair `mmap` returned, unmapped only
        // here, and no slice from `bytes` or `bytes_mut` outlives `self`.
        unsafe {
            let _unmapped = rustix::mm::munmap(self.ptr, self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn the_carrier_reads_back_what_was_written() {
        for len in [0, 1, 4095, 4096, 4097, 200_000] {
            let fd = create(&payload(len)).unwrap();
            assert_eq!(read(fd.as_fd()).unwrap(), payload(len), "length {len}");
        }
    }

    #[test]
    fn a_reader_finds_the_dump_wherever_the_offset_was_left() {
        let fd = create(&payload(10_000)).unwrap();
        assert_eq!(read(fd.as_fd()).unwrap(), payload(10_000));
        assert_eq!(read(fd.as_fd()).unwrap(), payload(10_000), "a second read");
    }

    #[test]
    fn the_carrier_is_not_inherited_across_exec() {
        let fd = create(b"dump").unwrap();
        let flags = rustix::io::fcntl_getfd(&fd).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }

    /// The probe's stdin is a duplicate without close-on-exec; it must
    /// read the same dump through it.
    #[test]
    fn the_probe_reads_the_dump_through_its_stdin_duplicate() {
        let fd = create(&payload(5000)).unwrap();
        let dup = fd.as_fd().try_clone_to_owned().unwrap();
        drop(fd);
        assert_eq!(read(dup.as_fd()).unwrap(), payload(5000));
    }

    #[test]
    fn a_shared_memory_carrier_reads_back_through_a_mapping() {
        for len in [0, 1, 4095, 4096, 4097, 3 * 1024 * 1024 + 17] {
            let fd = create_shm(&payload(len)).unwrap();
            assert_eq!(read_mmap(fd.as_fd()).unwrap(), payload(len), "length {len}");
        }
    }

    /// The object is unlinked before the dump is written, so no name
    /// outlives `create_shm` for another process to open.
    #[test]
    fn a_shared_memory_carrier_leaves_no_name_behind() {
        let (fd, name) = create_shm_named(&payload(4096)).unwrap();
        let reopened = rustix::shm::open(
            name.as_str(),
            rustix::shm::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        );
        assert_eq!(reopened.err(), Some(rustix::io::Errno::NOENT), "{name}");
        assert_eq!(read_mmap(fd.as_fd()).unwrap(), payload(4096));
    }

    #[test]
    fn a_shared_memory_carrier_is_not_inherited_across_exec() {
        let fd = create_shm(b"dump").unwrap();
        let flags = rustix::io::fcntl_getfd(&fd).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }

    /// Both readers take the same framing, so a dump written on one path
    /// reads back on the other.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_memory_file_and_the_shared_memory_object_share_one_framing() {
        let memfd = create_memfd(&payload(9000)).unwrap();
        assert_eq!(read_mmap(memfd.as_fd()).unwrap(), payload(9000));
        let shm = create_shm(&payload(9000)).unwrap();
        assert_eq!(read_pread(shm.as_fd()).unwrap(), payload(9000));
    }

    #[test]
    fn a_length_past_the_object_is_refused() {
        let mut object = framed(b"abc");
        object[..HEADER].copy_from_slice(&4u64.to_le_bytes());
        assert_eq!(
            unframed(&object).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            unframed(&[0u8; 3]).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "shorter than the header"
        );
        let mut huge = framed(b"");
        huge[..HEADER].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(unframed(&huge).is_err());
    }

    #[test]
    fn a_read_only_mapping_hands_out_no_mutable_bytes() {
        let fd = create_shm(b"dump").unwrap();
        let mut mapping = Mapping::new(fd.as_fd(), HEADER + 4, false).unwrap();
        assert!(mapping.bytes_mut().is_none());
        assert_eq!(unframed(mapping.bytes()).unwrap(), b"dump");
    }

    #[test]
    fn an_empty_mapping_is_refused_rather_than_made() {
        let fd = create_shm(b"").unwrap();
        assert!(Mapping::new(fd.as_fd(), 0, false).is_err());
    }

    #[test]
    fn trailing_bytes_past_the_named_length_are_ignored() {
        let mut object = framed(b"dump");
        object.extend_from_slice(&[0u8; 4092]);
        assert_eq!(unframed(&object).unwrap(), b"dump");
    }
}

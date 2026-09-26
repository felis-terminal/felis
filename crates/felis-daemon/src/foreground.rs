//! Resolve a PTY foreground process-group id to a program name.
//!
//! Lookups read the OS process table by pid, never shell output (principle 4).

/// The short program name of the process-group leader `pgid`, or `None`
/// if it cannot be resolved. Linux reads `/proc/<pgid>/comm`; macOS calls
/// `proc_pidpath` and takes the basename; other platforms return `None`.
// `comm_impl` is `const` only on platforms without a lookup, so the
// lint fires on those alone.
#[allow(clippy::missing_const_for_fn)]
#[must_use]
pub fn comm_for_pgid(pgid: i32) -> Option<String> {
    if pgid <= 0 {
        return None;
    }
    comm_impl(pgid)
}

#[cfg(target_os = "linux")]
fn comm_impl(pgid: i32) -> Option<String> {
    // `comm` is truncated to TASK_COMM_LEN (16) and newline-terminated.
    let raw = std::fs::read_to_string(format!("/proc/{pgid}/comm")).ok()?;
    let name = raw.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

// macOS has no /proc. The libproc crate is rejected: its 0.14 line
// pulls bindgen/libclang into the build for this single call.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn comm_impl(pgid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let cap = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    // SAFETY: `buf` is a live, uniquely-owned allocation of `cap` bytes.
    // `proc_pidpath` writes at most `cap` bytes into it and returns the
    // count written (> 0) or <= 0 on error; it neither reads past the
    // buffer nor touches aliased memory. The returned length is validated
    // before the buffer is interpreted as a path.
    let written = unsafe { libc::proc_pidpath(pgid, buf.as_mut_ptr().cast::<libc::c_void>(), cap) };
    let len = usize::try_from(written).ok().filter(|&n| n > 0)?;
    buf.truncate(len);
    let path = String::from_utf8(buf).ok()?;
    path.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const fn comm_impl(_pgid: i32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_positive_pgid_resolves_to_none() {
        assert_eq!(comm_for_pgid(0), None);
        assert_eq!(comm_for_pgid(-1), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn resolves_this_test_process_group_to_a_name() {
        let pgrp = rustix::process::getpgrp();
        let name = comm_for_pgid(pgrp.as_raw_nonzero().get());
        assert!(
            name.is_some_and(|n| !n.is_empty()),
            "the live test process group must resolve to a name",
        );
    }
}

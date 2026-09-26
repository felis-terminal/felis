//! Unix PTY backend: `posix_openpt` primitives + `fork`/`exec` via
//! `std::process::Command`.

use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, OwnedFd},
    os::unix::process::CommandExt,
    process::Stdio,
};

use crate::{Command, PtyError, Size};

impl From<Size> for rustix::termios::Winsize {
    fn from(size: Size) -> Self {
        Self {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        }
    }
}

/// The PTY master fd. `SIGHUP` reaches the child's foreground process
/// group only when the *last* master fd closes, and this is never the
/// last one while the child runs: [`clone_reader`](Master::clone_reader)
/// hands the reader thread a dup it holds until EOF. Teardown signals
/// the child explicitly ([`crate::ChildHandle::hangup`]) instead.
pub(crate) struct Master {
    fd: OwnedFd,
}

impl Master {
    /// The dup shares the master's open file description, so the
    /// `O_NONBLOCK` set here lands on every clone; [`MasterWriter`]
    /// exists to absorb it.
    pub(crate) fn clone_reader(&self) -> io::Result<MasterReader> {
        let fd = self.fd.try_clone()?;
        let flags = rustix::fs::fcntl_getfl(&fd).map_err(io::Error::from)?;
        rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(io::Error::from)?;
        Ok(MasterReader(File::from(fd)))
    }

    pub(crate) fn clone_writer(&self) -> io::Result<MasterWriter> {
        Ok(MasterWriter(File::from(self.fd.try_clone()?)))
    }

    pub(crate) fn resize(&self, size: Size) -> io::Result<()> {
        rustix::termios::tcsetwinsize(&self.fd, size.into()).map_err(io::Error::from)
    }

    #[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
    pub(crate) fn foreground_pgrp(&self) -> Option<i32> {
        // SAFETY: Not `rustix::termios::tcgetpgrp`: once the session leader
        // exits, macOS reports a non-positive pgid, which rustix funnels
        // through `Pid::from_raw_unchecked` and panics in debug builds.
        // `tcgetpgrp` reads the foreground pgid of the terminal behind `fd`
        // and writes no memory; `self.fd` is a live owned descriptor. It
        // reports failure as -1, which the filter maps to `None`.
        let raw = unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) };
        (raw > 0).then_some(raw)
    }
}

/// How long [`MasterReader::read`] re-tries a non-blocking read before
/// parking in `poll(2)`: enough to bridge the ~2–4 µs gaps between the
/// kernel's ~1 KiB PTY chunks mid-burst, short enough that the
/// busy-wait at a burst's end stays invisible.
#[cfg(not(target_os = "linux"))]
const READ_SPIN_WINDOW: std::time::Duration = std::time::Duration::from_micros(50);
/// No spin on Linux: its PTY queue holds ~68 KiB, so the writer is not
/// held to the reader chunk by chunk as in the macOS lockstep, and there
/// the spin moved no DOOM-fire frame rate while burning reader CPU.
#[cfg(target_os = "linux")]
const READ_SPIN_WINDOW: std::time::Duration = std::time::Duration::ZERO;

/// Master-side `Read` mapping Linux master `EIO` on slave close to `Ok(0)`.
///
/// Why not parse on another thread: lockstep, not parse CPU, owns the
/// drain (`.agents/skills/perf-trace/references/cost-maps.md`).
pub(crate) struct MasterReader(File);

impl Read for MasterReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut spin_deadline = None;
        loop {
            match self.0.read(buf) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let now = std::time::Instant::now();
                    let deadline = *spin_deadline.get_or_insert_with(|| now + READ_SPIN_WINDOW);
                    if now >= deadline {
                        wait_for(&self.0, rustix::event::PollFlags::IN)?;
                        spin_deadline = None;
                    } else {
                        // Keep the retries hot: each read(2) is what
                        // wakes the refilling writer, so pacing this
                        // loop stalls the lockstep (512 spin hints
                        // doubled blocked-write time; 0–128 were flat).
                        for _ in 0..64 {
                            std::hint::spin_loop();
                        }
                    }
                }
                Err(err) if err.raw_os_error() == Some(libc::EIO) => return Ok(0),
                other => return other,
            }
        }
    }
}

/// Absorbs the `O_NONBLOCK` the reader dup put on the shared open file
/// description ([`Master::clone_reader`]): a full input queue (a large
/// paste against a stopped child) surfaces as `WouldBlock`, and this
/// parks in `poll(2)` instead of erroring the writer's `write_all`.
pub(crate) struct MasterWriter(File);

impl io::Write for MasterWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.0.write(buf) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    wait_for(&self.0, rustix::event::PollFlags::OUT)?;
                }
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// `POLLHUP`/`POLLERR` arrive regardless of `events`; the follow-up
/// read/write surfaces the actual condition.
fn wait_for(file: &File, events: rustix::event::PollFlags) -> io::Result<()> {
    let mut fds = [rustix::event::PollFd::new(file, events)];
    rustix::event::poll(&mut fds, None).map_err(io::Error::from)?;
    Ok(())
}

pub(crate) struct Child(std::process::Child);

impl Child {
    /// Send `SIGHUP` to the child's process group (the session leader).
    ///
    /// Notifies both the shell and any active foreground job when the
    /// terminal closes. `ESRCH` maps to `Ok(())` if the child already exited.
    pub(crate) fn hangup(&self) -> io::Result<()> {
        let Some(pid) = self.process_id().and_then(rustix::process::Pid::from_raw) else {
            return Ok(());
        };
        match rustix::process::kill_process_group(pid, rustix::process::Signal::HUP) {
            Err(rustix::io::Errno::SRCH) => Ok(()),
            other => other.map_err(io::Error::from),
        }
    }

    /// std reports `InvalidInput` once the child has been reaped;
    /// callers treat that as already dead.
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        match self.0.kill() {
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(()),
            other => other,
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }

    fn process_id(&self) -> Option<i32> {
        i32::try_from(self.0.id()).ok()
    }
}

/// Allocate the PTY pair `(master, slave)` with winsize and CLOEXEC applied.
///
/// Uses `posix_openpt` primitives: `openpty(3)` relies on `ptsname`'s static
/// non-thread-safe buffer, whereas rustix uses ioctls into caller buffers.
fn open_pair(size: Size) -> Result<(OwnedFd, OwnedFd), PtyError> {
    use rustix::{
        fs::{Mode, OFlags},
        pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt},
    };
    // Serialize allocation process-wide: macOS kernel ptmx clone path races
    // under concurrent `posix_openpt` and slave-open sequences, failing with
    // `ENXIO`. Apple libc's `openpty(3)` serializes internally for `ptsname`.
    static PTY_ALLOC: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _alloc_guard = PTY_ALLOC
        .lock()
        .map_err(|_| PtyError::OpenPty("pty allocation lock poisoned".to_string()))?;

    let err =
        |what: &'static str| move |e: rustix::io::Errno| PtyError::OpenPty(format!("{what}: {e}"));
    // No CLOEXEC at openpt: macOS `posix_openpt` rejects flags beyond
    // RDWR|NOCTTY, so the flag lands via fcntl below instead.
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).map_err(err("openpt"))?;
    grantpt(&master).map_err(err("grantpt"))?;
    unlockpt(&master).map_err(err("unlockpt"))?;
    let name = ptsname(&master, Vec::new()).map_err(err("ptsname"))?;
    let slave = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(err("open slave"))?;
    // Close-on-exec so a concurrent spawn's fork/exec window cannot leak
    // the master into an unrelated child
    // (docs/reference/security-audits.md "`O_CLOEXEC` + `O_NOFOLLOW`
    // audit (standing)"). The slave still reaches this child because
    // `Stdio` dup2s it onto fds 0-2, which clears the flag on the copies.
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).map_err(err("set cloexec"))?;
    rustix::termios::tcsetwinsize(&slave, size.into()).map_err(err("set winsize"))?;
    Ok((master, slave))
}

pub(crate) fn open_and_spawn(command: &Command, size: Size) -> Result<(Master, Child), PtyError> {
    let (master, slave) = open_pair(size)?;

    let mut cmd = std::process::Command::new(&command.program);
    cmd.args(&command.args);
    cmd.env_clear();
    cmd.envs(&command.env);
    if let Some(cwd) = &command.cwd {
        cmd.current_dir(cwd);
    }
    let clone_slave = || {
        slave
            .try_clone()
            .map(Stdio::from)
            .map_err(|e| PtyError::OpenPty(format!("dup slave: {e}")))
    };
    cmd.stdin(clone_slave()?);
    cmd.stdout(clone_slave()?);
    // Move, not clone: the parent must keep no slave fd, or the master
    // never sees EOF when the child exits.
    cmd.stderr(Stdio::from(slave));

    // SAFETY: the closure runs in the forked child before exec, where
    // only async-signal-safe calls are permitted (fork in a threaded
    // process can leave any lock, including the allocator's, held by
    // an absent thread). `setsid(2)` and `ioctl(2)` are
    // direct syscalls: no allocation, no locking, no libc state.
    // `TIOCSCTTY` targets fd 0 because std has already dup2'ed the
    // slave onto stdin by the time pre_exec closures run.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // `From` (not `as`) because the constant's type differs
            // per platform: `c_uint` on Apple libc, `c_ulong` on
            // linux-gnu: a lossless widening either way.
            if libc::ioctl(0, libc::c_ulong::from(libc::TIOCSCTTY), 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().map_err(|e| PtyError::Spawn(e.to_string()))?;
    drop(cmd);

    Ok((Master { fd: master }, Child(child)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::io::{FdFlags, fcntl_getfd};

    /// Both ends carry `FD_CLOEXEC` (REQ-912;
    /// `docs/reference/security-audits.md`
    /// "`O_CLOEXEC` + `O_NOFOLLOW` audit (standing)"). The master gets
    /// the flag from `fcntl`, the slave from `O_CLOEXEC` at open, so
    /// both are read back.
    #[test]
    fn open_pair_sets_cloexec_on_both_ends() {
        let (master, slave) = open_pair(Size {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

        assert!(fcntl_getfd(&master).unwrap().contains(FdFlags::CLOEXEC));
        assert!(fcntl_getfd(&slave).unwrap().contains(FdFlags::CLOEXEC));
    }
}

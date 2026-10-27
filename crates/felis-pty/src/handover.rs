//! Carrying a Windows session to an in-place upgrade's successor
//! (`docs/explanation/architecture/overview.md` "In-place upgrade"):
//! the handles that keep a pseudoconsole and its shell alive, moved
//! into another process by `DuplicateHandle`, and the processes the
//! hand-over runs between.
#![allow(unsafe_code)]

use std::{
    ffi::OsStr,
    io,
    os::windows::ffi::OsStrExt as _,
    os::windows::io::{AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle},
    path::Path,
    ptr,
    time::Duration,
};

use conpty_bin_vendored as conpty;
use windows_sys::Win32::{
    Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, GetHandleInformation, HANDLE, S_OK,
        WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, GetCurrentProcess, GetProcessId, INFINITE,
        PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
    },
};

use crate::{
    ByteSink, ChildHandle, PtyError, PtySession, Resizer, poison, session_over,
    windows::{Api, Child, Host, Master, api, append_quoted, hresult_error, process_host},
};

/// Whether this process's sessions can be handed over: only the
/// bundled host rebuilds a pseudoconsole from its handles.
pub fn available() -> Result<(), String> {
    let host = process_host().map_err(|err| err.to_string())?;
    if host != Host::Bundled {
        return Err(
            "this daemon runs its shells on the inbox console host, which cannot hand them over"
                .to_owned(),
        );
    }
    api(host).map(|_| ()).map_err(|err| err.to_string())
}

/// One session's handles, duplicated within this process so they stay
/// valid whatever the session does meanwhile.
pub struct Handover {
    signal: OwnedHandle,
    reference: OwnedHandle,
    host_process: OwnedHandle,
    output: OwnedHandle,
    input: OwnedHandle,
    child: OwnedHandle,
}

/// [`Handover`]'s handles as values valid in the process they were
/// duplicated into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HandoverValues {
    pub signal: u64,
    pub reference: u64,
    pub host_process: u64,
    pub output: u64,
    pub input: u64,
    pub child: u64,
}

impl HandoverValues {
    #[must_use]
    pub const fn all(&self) -> [u64; 6] {
        [
            self.signal,
            self.reference,
            self.host_process,
            self.output,
            self.input,
            self.child,
        ]
    }
}

/// The members of a pseudoconsole, in the order upstream's
/// `src/winconpty/winconpty.h` declares them: `hSignal`,
/// `hPtyReference`, `hConPtyProcess`. Nothing exports them; the
/// `conpty-bin-vendored` pin fixes the layout, and a test reads back
/// what `ConptyPackPseudoConsole` stored.
const fn members(hpc: conpty::Hpcon) -> [HANDLE; 3] {
    // SAFETY: `hpc` is a live pseudoconsole of the bundled host, which
    // allocates it as that struct; the three leading members are
    // pointer-sized and the allocation is pointer-aligned.
    unsafe { hpc.cast::<[HANDLE; 3]>().read() }
}

fn duplicate_here(handle: HANDLE) -> io::Result<OwnedHandle> {
    // SAFETY: `handle` is live for the duration of the call: it belongs
    // to a pseudoconsole or pipe this process still holds.
    unsafe { BorrowedHandle::borrow_raw(handle) }.try_clone_to_owned()
}

impl Handover {
    /// Takes the session's handles. Refused for a session on the inbox
    /// host, whose pseudoconsole no successor could rebuild.
    pub fn of(resizer: &Resizer, child: &ChildHandle) -> io::Result<Self> {
        let master = &resizer.master;
        if master.api.pack.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the session runs on the inbox console host",
            ));
        }
        let [signal, reference, host_process] = members(master.hpc);
        let child = child.child.lock().map_err(poison)?;
        Ok(Self {
            signal: duplicate_here(signal)?,
            reference: duplicate_here(reference)?,
            host_process: duplicate_here(host_process)?,
            output: master.output.try_clone()?,
            input: master.input.try_clone()?,
            child: child.process.try_clone()?,
        })
    }

    /// Duplicates every handle into `target`. The copies there close
    /// with `target`, so a successor that never commits leaves nothing
    /// open behind it.
    pub fn values_in(&self, target: &Process) -> io::Result<HandoverValues> {
        let into = |handle: &OwnedHandle| duplicate_into(handle.as_raw_handle(), target, None);
        Ok(HandoverValues {
            signal: into(&self.signal)?,
            reference: into(&self.reference)?,
            host_process: into(&self.host_process)?,
            output: into(&self.output)?,
            input: into(&self.input)?,
            child: into(&self.child)?,
        })
    }
}

/// `access` `None` keeps the source handle's access.
fn duplicate_into(handle: HANDLE, target: &Process, access: Option<u32>) -> io::Result<u64> {
    let mut out: HANDLE = ptr::null_mut();
    // SAFETY: `handle` is live in this process, `target.handle` is a
    // live process handle with `PROCESS_DUP_HANDLE` (this process
    // created it), and `out` is a live out-pointer. The copy is not
    // inheritable.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            target.handle.as_raw_handle(),
            &raw mut out,
            access.unwrap_or(0),
            FALSE,
            if access.is_some() {
                0
            } else {
                DUPLICATE_SAME_ACCESS
            },
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(out as u64)
}

/// Takes ownership of a handle another process duplicated into this
/// one, after checking that the value names an open handle.
fn take(value: u64) -> io::Result<OwnedHandle> {
    let handle = value as HANDLE;
    if handle.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a null handle value",
        ));
    }
    let mut flags = 0u32;
    // SAFETY: a pure query; an invalid value fails instead of touching
    // anything.
    if unsafe { GetHandleInformation(handle, &raw mut flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the value is an open handle in this process that the
    // predecessor duplicated here for this process to own, and the
    // dump names each value once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// A carried session rebuilt in the successor but not running: no
/// thread reads its output or writes its input, and dropping it neither
/// closes the pseudoconsole nor touches the child, so a successor that
/// never commits leaves the session exactly as the predecessor parked
/// it.
pub struct Adopted {
    api: &'static Api,
    hpc: conpty::Hpcon,
    output: OwnedHandle,
    input: OwnedHandle,
    child: OwnedHandle,
}

// SAFETY: as for `Master`: `hpc` is an opaque handle the pseudoconsole
// functions accept from any thread.
unsafe impl Send for Adopted {}

/// Rebuilds a carried session from the handle values its dump names.
pub fn prepare_adopt(values: &HandoverValues) -> Result<Adopted, PtyError> {
    let adopt = |what: &str, err: io::Error| PtyError::OpenPty(format!("adopt the {what}: {err}"));
    // Bundled whatever this process's integrity: only a bundled
    // pseudoconsole is ever handed over, and a successor runs at its
    // predecessor's level.
    let api = api(Host::Bundled)?;
    let pack = api.pack.ok_or_else(|| {
        PtyError::OpenPty("the bundled console host cannot adopt a pseudoconsole".to_owned())
    })?;
    let signal = take(values.signal).map_err(|e| adopt("signal pipe", e))?;
    let reference = take(values.reference).map_err(|e| adopt("console reference", e))?;
    let host_process = take(values.host_process).map_err(|e| adopt("console host", e))?;
    let output = take(values.output).map_err(|e| adopt("output pipe", e))?;
    let input = take(values.input).map_err(|e| adopt("input pipe", e))?;
    let child = take(values.child).map_err(|e| adopt("child process", e))?;
    let mut hpc: conpty::Hpcon = ptr::null_mut();
    // SAFETY: the three handles are open and owned here; on success the
    // pseudoconsole owns them, so they are released from `OwnedHandle`
    // only then.
    let hr = unsafe {
        pack(
            host_process.as_raw_handle(),
            reference.as_raw_handle(),
            signal.as_raw_handle(),
            &raw mut hpc,
        )
    };
    if hr != S_OK {
        return Err(PtyError::OpenPty(format!(
            "ConptyPackPseudoConsole: {}",
            hresult_error(hr)
        )));
    }
    for owned in [host_process, reference, signal] {
        let _released = std::os::windows::io::IntoRawHandle::into_raw_handle(owned);
    }
    Ok(Adopted {
        api,
        hpc,
        output,
        input,
        child,
    })
}

impl Adopted {
    /// The child, for ending it if the session cannot be started.
    pub fn child(&self) -> io::Result<Process> {
        Ok(Process {
            handle: self.child.try_clone()?,
        })
    }

    /// Starts the session's threads: from here it is this process's
    /// like any it spawned.
    pub fn start(self, sink: ByteSink) -> Result<PtySession, PtyError> {
        let master = Master {
            api: self.api,
            hpc: self.hpc,
            output: self.output,
            input: self.input,
        };
        session_over(
            master,
            Child {
                process: self.child,
            },
            sink,
        )
    }
}

/// A process the hand-over runs between: the successor as its
/// predecessor sees it, and the predecessor as the successor does.
#[derive(Debug)]
pub struct Process {
    handle: OwnedHandle,
}

impl Process {
    /// Starts `program` with no handle inherited and its own hidden
    /// console, as the auto-spawn starts a daemon.
    #[allow(clippy::cast_possible_truncation)]
    pub fn spawn_detached(program: &Path, args: &[&OsStr]) -> io::Result<Self> {
        let mut cmdline: Vec<u16> = Vec::new();
        append_quoted(program.as_os_str(), &mut cmdline);
        for arg in args {
            cmdline.push(u16::from(b' '));
            append_quoted(arg, &mut cmdline);
        }
        cmdline.push(0);
        let application: Vec<u16> = program.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: plain data the OS reads by pointer; zeroed is its
        // documented initial state.
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        // SAFETY: as above.
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: both wide buffers are NUL-terminated and outlive the
        // call; inherit-handles is FALSE, so the successor receives
        // nothing but what is later duplicated into it.
        let ok = unsafe {
            CreateProcessW(
                application.as_ptr(),
                cmdline.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                FALSE,
                CREATE_NO_WINDOW,
                ptr::null(),
                ptr::null(),
                &raw const startup,
                &raw mut info,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success both handles are fresh and ours; the thread
        // handle is not needed.
        unsafe {
            drop(OwnedHandle::from_raw_handle(info.hThread));
            Ok(Self {
                handle: OwnedHandle::from_raw_handle(info.hProcess),
            })
        }
    }

    /// This process, as a handle it owns.
    pub fn current() -> io::Result<Self> {
        // SAFETY: the pseudo-handle names this process and needs no
        // close; duplicating it yields a real handle.
        let current = unsafe { BorrowedHandle::borrow_raw(GetCurrentProcess()) };
        Ok(Self {
            handle: current.try_clone_to_owned()?,
        })
    }

    /// Takes the handle to this process's predecessor that it
    /// duplicated here.
    pub fn adopt(value: u64) -> io::Result<Self> {
        Ok(Self {
            handle: take(value)?,
        })
    }

    /// A handle to this process, duplicated into `target`, with the
    /// rights to read its pid, to wait for it, and to end it.
    pub fn current_into(target: &Self) -> io::Result<u64> {
        // SAFETY: the pseudo-handle names this process and needs no close.
        let current = unsafe { GetCurrentProcess() };
        duplicate_into(
            current,
            target,
            Some(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE),
        )
    }

    /// This handle, duplicated into `target`, with the rights to read
    /// the pid and to wait.
    pub fn share_into(&self, target: &Self) -> io::Result<u64> {
        duplicate_into(
            self.handle.as_raw_handle(),
            target,
            Some(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE),
        )
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        // SAFETY: a pure query on a live handle.
        unsafe { GetProcessId(self.handle.as_raw_handle()) }
    }

    /// Ends the process. It is gone once [`wait`](Self::wait) returns.
    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: a live process handle with `PROCESS_TERMINATE`.
        if unsafe { TerminateProcess(self.handle.as_raw_handle(), 1) } == 0 {
            if self.wait(Some(Duration::ZERO))? {
                return Ok(());
            }
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Blocks until the process has exited (`true`) or `timeout` passes
    /// (`false`); `None` waits for as long as it takes.
    #[allow(clippy::cast_possible_truncation)]
    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
        let ms = timeout.map_or(INFINITE, |t| {
            t.as_millis().min(u128::from(INFINITE - 1)) as u32
        });
        // SAFETY: a live process handle with `SYNCHRONIZE`.
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), ms) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            other => Err(io::Error::other(format!(
                "WaitForSingleObject returned {other:#x}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Threading::CreateEventW;

    fn event() -> OwnedHandle {
        // SAFETY: an unnamed manual-reset event with default security.
        let handle = unsafe { CreateEventW(ptr::null(), 1, 1, ptr::null()) };
        assert!(!handle.is_null(), "{}", io::Error::last_os_error());
        // SAFETY: fresh and ours.
        unsafe { OwnedHandle::from_raw_handle(handle) }
    }

    /// The layout `members` reads is the one the pinned `conpty.dll`
    /// writes: a pseudoconsole packed from three known handles reads
    /// back as those handles, in its declared order.
    #[test]
    fn a_packed_pseudoconsole_reads_back_its_members_in_order() {
        let api = api(Host::Bundled).expect("the bundled host loads");
        let pack = api.pack.expect("the bundled host packs");
        let [process, reference, signal] =
            [event(), event(), event()].map(IntoRawHandle::into_raw_handle);
        let mut hpc: conpty::Hpcon = ptr::null_mut();
        // SAFETY: three open handles whose ownership passes to the
        // packed pseudoconsole.
        let hr = unsafe { pack(process, reference, signal, &raw mut hpc) };
        assert_eq!(hr, S_OK);
        assert_eq!(members(hpc), [signal, reference, process]);
        // SAFETY: closes the three handles and frees the allocation.
        unsafe { (api.close)(hpc) };
    }
}

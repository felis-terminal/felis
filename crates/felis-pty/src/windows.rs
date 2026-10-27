//! Windows PTY backend: `ConPTY` (`CreatePseudoConsole`) via `windows-sys`.
//!
//! Attaches `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` directly via `CreateProcessW`.
//! Session teardown polls `try_wait` because `ConPTY` closes output pipes
//! only on `ClosePseudoConsole`.
#![allow(unsafe_code)]

use std::{
    ffi::{CStr, OsStr, c_void},
    fs::{self, File},
    io::{self, Read, Write},
    mem,
    os::windows::ffi::OsStrExt,
    os::windows::fs::OpenOptionsExt,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    os::windows::process::ExitStatusExt,
    path::{Path, PathBuf},
    ptr,
    sync::OnceLock,
    time::{Duration, Instant},
};

use conpty_bin_vendored as conpty;
use windows_sys::Win32::{
    Foundation::{
        ERROR_OPERATION_ABORTED, ERROR_SHARING_VIOLATION, HANDLE, INVALID_HANDLE_VALUE, S_OK,
        WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel,
    },
    Storage::FileSystem::{FILE_SHARE_READ, MoveFileExW},
    System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
    System::Pipes::CreatePipe,
    System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess,
        InitializeProcThreadAttributeList, OpenProcessToken, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject,
    },
};
use windows_sys::core::HRESULT;

use crate::{Command, PtyError, Size};

impl From<Size> for conpty::Coord {
    #[allow(clippy::cast_possible_wrap)]
    fn from(size: Size) -> Self {
        Self {
            x: size.cols as i16,
            y: size.rows as i16,
        }
    }
}

/// Unwrap `HRESULT_FROM_WIN32` (`0x8007_xxxx`) to bare Win32 codes so
/// `io::Error::from_raw_os_error` formats known error descriptions.
#[allow(clippy::cast_possible_wrap)]
pub(crate) fn hresult_error(hr: HRESULT) -> io::Error {
    // HRESULT_FROM_WIN32(code) == 0x8007_0000 | (code & 0xFFFF).
    const FACILITY_WIN32: HRESULT = 0x8007_0000_u32 as HRESULT;
    let code = if hr & (0xFFFF_0000_u32 as HRESULT) == FACILITY_WIN32 {
        hr & 0xFFFF
    } else {
        hr
    };
    io::Error::from_raw_os_error(code)
}

/// Which `ConPTY` implementation hosts a pseudoconsole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Host {
    /// `kernel32`'s, hosted by the system's conhost.
    Inbox,
    /// The `conpty.dll` and `OpenConsole.exe` felis embeds.
    Bundled,
}

const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x2000;

impl Host {
    /// Exactly Medium: the extraction directory is writable at Medium,
    /// and `OpenConsole.exe` imports DLLs that are not `KnownDLLs`, so a
    /// process at any other level must not run the host from there.
    const fn for_integrity(rid: u32) -> Self {
        if rid == SECURITY_MANDATORY_MEDIUM_RID {
            Self::Bundled
        } else {
            Self::Inbox
        }
    }
}

/// The mandatory-label RID of this process's token.
fn integrity_rid() -> io::Result<u32> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess` is a pseudo-handle needing no close,
    // and `token` is a live out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success the token handle is fresh and owned here once.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut len: u32 = 0;
    // SAFETY: the documented size probe: a null buffer of length 0 and
    // a live out-length.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenIntegrityLevel,
            ptr::null_mut(),
            0,
            &raw mut len,
        );
    }
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` is exactly the `len` bytes the probe asked for.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenIntegrityLevel,
            buf.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `buf` begins with a `TOKEN_MANDATORY_LABEL`
    // whose SID points into `buf`, which outlives every read below.
    // Unaligned because a `Vec<u8>` carries no alignment guarantee.
    let sid = unsafe {
        buf.as_ptr()
            .cast::<TOKEN_MANDATORY_LABEL>()
            .read_unaligned()
    }
    .Label
    .Sid;
    // SAFETY: `sid` is a valid SID; both accessors return pointers into it.
    unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return Err(io::Error::other("the integrity label has no RID"));
        }
        Ok(*GetSidSubAuthority(sid, u32::from(count) - 1))
    }
}

/// The host this process runs every shell on.
pub(crate) fn process_host() -> Result<Host, PtyError> {
    static HOST: OnceLock<Result<Host, String>> = OnceLock::new();
    HOST.get_or_init(|| {
        let host = integrity_rid()
            .map(Host::for_integrity)
            .map_err(|err| format!("read this process's integrity level: {err}"));
        match &host {
            Ok(Host::Inbox) => tracing::info!("using the inbox ConPTY: not at Medium integrity"),
            Ok(Host::Bundled) => {}
            Err(err) => tracing::error!(%err, "no ConPTY host"),
        }
        host
    })
    .clone()
    .map_err(PtyError::OpenPty)
}

/// The functions of one `ConPTY` implementation. A pseudoconsole's
/// resize and close must go to the implementation that created it.
pub(crate) struct Api {
    pub(crate) create: conpty::ConptyCreatePseudoConsoleFn,
    pub(crate) resize: conpty::ConptyResizePseudoConsoleFn,
    pub(crate) close: conpty::ConptyClosePseudoConsoleFn,
    /// Only the bundled host can rebuild a pseudoconsole from its
    /// handles, which is what an in-place upgrade's successor does.
    pub(crate) pack: Option<conpty::ConptyPackPseudoConsoleFn>,
}

// Declared with the crate's types so both hosts fill one `Api`:
// `windows-sys` types `HPCON` as an integer.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreatePseudoConsole(
        size: conpty::Coord,
        input: conpty::Handle,
        output: conpty::Handle,
        flags: u32,
        pc: *mut conpty::Hpcon,
    ) -> conpty::Hresult;
    fn ResizePseudoConsole(pc: conpty::Hpcon, size: conpty::Coord) -> conpty::Hresult;
    fn ClosePseudoConsole(pc: conpty::Hpcon);
}

static INBOX: Api = Api {
    create: CreatePseudoConsole,
    resize: ResizePseudoConsole,
    close: ClosePseudoConsole,
    pack: None,
};

const DLL: &str = "conpty.dll";
const HOST: &str = "OpenConsole.exe";
const LICENSE: &str = "LICENSE-conpty.txt";

struct Bundle {
    api: Api,
    dir: PathBuf,
    _held: Extracted,
}

/// Opened without write or delete sharing and held for the process
/// lifetime: while open, no process can replace, delete or rename the
/// files or a directory above them, so the verified bytes are the ones
/// that run, and `conpty.dll` never finds its host missing and falls
/// back to conhost.
struct Extracted {
    _dll: File,
    _host: File,
}

/// The embedded host, extracted and loaded on first use. A failure is
/// kept and refuses every spawn: there is no fallback to the inbox host.
fn bundle() -> Result<&'static Bundle, PtyError> {
    static BUNDLE: OnceLock<Result<Bundle, String>> = OnceLock::new();
    BUNDLE
        .get_or_init(|| {
            let bundle = bundle_dir().and_then(load_bundle);
            match &bundle {
                Ok(bundle) => {
                    tracing::info!(dir = %bundle.dir.display(), "using the bundled ConPTY");
                }
                Err(err) => tracing::error!(%err, "the bundled ConPTY is unusable"),
            }
            bundle
        })
        .as_ref()
        .map_err(|err| PtyError::OpenPty(err.clone()))
}

fn bundle_dir() -> Result<PathBuf, String> {
    directories::ProjectDirs::from("", "", "felis")
        .map(|dirs| dirs.data_local_dir().join("conpty").join(conpty::VERSION))
        .ok_or_else(|| "no local application data directory for the ConPTY host".to_owned())
}

fn load_bundle(dir: PathBuf) -> Result<Bundle, String> {
    let held = extract(&dir)?;
    let dll = dir.join(DLL);
    let path: Vec<u16> = dll.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `path` is a NUL-terminated absolute path to the bytes just
    // verified, which the held handle keeps unchanged; the flag resolves
    // the DLL's own imports from System32 only.
    let module =
        unsafe { LoadLibraryExW(path.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err(format!(
            "load {}: {}",
            dll.display(),
            io::Error::last_os_error()
        ));
    }
    let resolve = |name: &CStr| {
        // SAFETY: `module` stays loaded for the process lifetime (never
        // freed) and `name` is NUL-terminated.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
            .ok_or_else(|| format!("{} lacks {}", dll.display(), name.to_string_lossy()))
    };
    // SAFETY: each export has the type the crate declares for its name.
    let api = unsafe {
        Api {
            create: mem::transmute::<
                unsafe extern "system" fn() -> isize,
                conpty::ConptyCreatePseudoConsoleFn,
            >(resolve(conpty::export::ConptyCreatePseudoConsole)?),
            resize: mem::transmute::<
                unsafe extern "system" fn() -> isize,
                conpty::ConptyResizePseudoConsoleFn,
            >(resolve(conpty::export::ConptyResizePseudoConsole)?),
            close: mem::transmute::<
                unsafe extern "system" fn() -> isize,
                conpty::ConptyClosePseudoConsoleFn,
            >(resolve(conpty::export::ConptyClosePseudoConsole)?),
            pack: Some(mem::transmute::<
                unsafe extern "system" fn() -> isize,
                conpty::ConptyPackPseudoConsoleFn,
            >(resolve(
                conpty::export::ConptyPackPseudoConsole,
            )?)),
        }
    };
    Ok(Bundle {
        api,
        dir,
        _held: held,
    })
}

/// Places the embedded files in `dir` unless present, then verifies
/// whatever is there against the embedded bytes. Concurrent daemons may
/// extract into one directory at once.
fn extract(dir: &Path) -> Result<Extracted, String> {
    fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    let dll = place(dir, DLL, conpty::CONPTY_DLL)?;
    let host = place(dir, HOST, conpty::OPENCONSOLE_EXE)?;
    place(dir, LICENSE, conpty::MICROSOFT_LICENSE.as_bytes())?;
    Ok(Extracted {
        _dll: dll,
        _host: host,
    })
}

fn place(dir: &Path, name: &str, bytes: &[u8]) -> Result<File, String> {
    let path = dir.join(name);
    let file = match open_held(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            write_new(dir, name, bytes)?;
            open_held(&path)
        }
        opened => opened,
    }
    .map_err(|err| format!("open {}: {err}", path.display()))?;
    let mut on_disk = Vec::with_capacity(bytes.len());
    (&file)
        .read_to_end(&mut on_disk)
        .map_err(|err| format!("read {}: {err}", path.display()))?;
    if on_disk != bytes {
        // Never overwritten: another daemon may be running it.
        return Err(format!(
            "{} is not the ConPTY felis embeds; delete {} while no felis daemon runs",
            path.display(),
            dir.display()
        ));
    }
    Ok(file)
}

/// Opens `path` so that no process can replace or delete it while held.
/// The process that just moved the file into place can still hold it
/// open in a way this share mode refuses, so a sharing violation is
/// retried briefly rather than reported.
fn open_held(path: &Path) -> io::Result<File> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
        {
            Err(err)
                if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION.cast_signed())
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            opened => return opened,
        }
    }
}

/// Writes `bytes` to a uniquely named `.tmp` file and moves it to
/// `name` without replacing, so a file another process placed first
/// stays; `place` verifies whichever won.
fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let nonce = std::hash::BuildHasher::hash_one(&std::hash::RandomState::new(), ());
    let tmp = dir.join(format!("{name}.{}.{nonce:016x}.tmp", std::process::id()));
    let path = dir.join(name);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|err| format!("create {}: {err}", tmp.display()))?;
    match write_and_move(file, bytes, &tmp, &path) {
        Ok(()) => Ok(()),
        Err(err) => {
            drop(fs::remove_file(&tmp));
            if err.kind() == io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(format!("write {}: {err}", path.display()))
            }
        }
    }
}

fn write_and_move(mut file: File, bytes: &[u8], tmp: &Path, path: &Path) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    // SAFETY: both arguments are NUL-terminated wide paths that outlive
    // the call; no flags means an existing target is never replaced.
    if unsafe { MoveFileExW(wide(tmp).as_ptr(), wide(path).as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn api(host: Host) -> Result<&'static Api, PtyError> {
    match host {
        Host::Inbox => Ok(&INBOX),
        Host::Bundled => bundle().map(|bundle| &bundle.api),
    }
}

/// The pseudoconsole plus our ends of its I/O pipes.
///
/// Drop order: `ClosePseudoConsole` runs first (in `Drop`), which
/// flushes pending output and then breaks the output pipe, the break
/// the reader thread sees as EOF. The pipe handles close afterwards.
pub(crate) struct Master {
    pub(crate) api: &'static Api,
    pub(crate) hpc: conpty::Hpcon,
    /// Read end of the `ConPTY` output pipe (`ConPTY` writes VT here).
    pub(crate) output: OwnedHandle,
    /// Write end of the `ConPTY` input pipe (keystrokes go here).
    pub(crate) input: OwnedHandle,
}

// SAFETY: `hpc` is an opaque handle the pseudoconsole functions accept
// from any thread; `api` is shared function pointers.
unsafe impl Send for Master {}
// SAFETY: as above; `&Master` only resizes, which `ConPTY` serializes.
unsafe impl Sync for Master {}

impl Drop for Master {
    fn drop(&mut self) {
        // SAFETY: `self.hpc` came from a successful
        // create through the same `api` and is closed exactly once, here.
        unsafe { (self.api.close)(self.hpc) };
    }
}

impl Master {
    pub(crate) fn clone_reader(
        &self,
        _quiesce: &std::sync::Arc<crate::quiesce::QuiesceShared>,
    ) -> io::Result<MasterReader> {
        Ok(MasterReader(File::from(self.output.try_clone()?)))
    }

    pub(crate) fn clone_writer(
        &self,
        _quiesce: &std::sync::Arc<crate::quiesce::QuiesceShared>,
    ) -> io::Result<File> {
        Ok(File::from(self.input.try_clone()?))
    }

    pub(crate) fn resize(&self, size: Size) -> io::Result<()> {
        // SAFETY: `self.hpc` is a live pseudoconsole handle (closed
        // only in `Drop`) of the same `api`; `Coord` is plain data.
        let hr = unsafe { (self.api.resize)(self.hpc, size.into()) };
        if hr == S_OK {
            Ok(())
        } else {
            Err(hresult_error(hr))
        }
    }

    /// No foreground-process-group concept on Windows; the session
    /// listing's "what's running" column stays empty there.
    #[allow(clippy::unused_self, clippy::missing_const_for_fn)]
    pub(crate) fn foreground_pgrp(&self) -> Option<i32> {
        None
    }
}

/// Master-side `Read` that reports end-of-stream instead of
/// `ERROR_BROKEN_PIPE`, which is what a closed pseudoconsole produces,
/// and a read the quiesce cancelled as interrupted, so the reader
/// retries it from its park point.
pub(crate) struct MasterReader(File);

impl Read for MasterReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(0),
            Err(err) if err.raw_os_error() == Some(ERROR_OPERATION_ABORTED.cast_signed()) => {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            }
            other => other,
        }
    }
}

pub(crate) struct Child {
    pub(crate) process: OwnedHandle,
}

impl Child {
    /// Windows has no `SIGHUP`. A console child's "the terminal went
    /// away" signal is `ClosePseudoConsole`, which fires when the
    /// session's pseudoconsole drops, so terminate is all that is left.
    pub(crate) fn hangup(&mut self) -> io::Result<()> {
        self.kill()
    }

    /// Idempotent: an already-exited child is success, as on Unix.
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        // SAFETY: `self.process` is a live process handle owned by
        // this struct.
        if unsafe { TerminateProcess(self.process.as_raw_handle(), 1) } == 0 {
            let err = io::Error::last_os_error();
            // Lost the race: the child exited between the check and
            // the call (TerminateProcess then fails ACCESS_DENIED).
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        // SAFETY: `self.process` is a live process handle; a zero
        // timeout makes the wait a pure state poll.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => {
                let mut code: u32 = 0;
                // SAFETY: the handle is live and `code` outlives the
                // call.
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(std::process::ExitStatus::from_raw(code)))
            }
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            other => Err(io::Error::other(format!(
                "WaitForSingleObject returned {other:#x}"
            ))),
        }
    }
}

fn pipe() -> Result<(OwnedHandle, OwnedHandle), PtyError> {
    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();
    // SAFETY: both out-pointers are live locals; a null
    // SECURITY_ATTRIBUTES means non-inheritable handles, which is
    // what we want: the pseudoconsole attribute, not handle
    // inheritance, is how the child reaches the ConPTY.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, ptr::null(), 0) } == 0 {
        return Err(PtyError::OpenPty(format!(
            "CreatePipe: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: on success CreatePipe hands us two fresh handles we now
    // own exactly once each.
    unsafe {
        Ok((
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        ))
    }
}

/// Quote one argument per the `CommandLineToArgvW` / MSVCRT parsing
/// rules, appending to the wide command line.
pub(crate) fn append_quoted(arg: &OsStr, out: &mut Vec<u16>) {
    let wide: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = wide.is_empty()
        || wide
            .iter()
            .any(|&c| c == u16::from(b' ') || c == u16::from(b'\t') || c == u16::from(b'"'));
    if !needs_quotes {
        out.extend_from_slice(&wide);
        return;
    }
    out.push(u16::from(b'"'));
    let mut backslashes = 0usize;
    for &c in &wide {
        if c == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        if c == u16::from(b'"') {
            out.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        }
        backslashes = 0;
        out.push(c);
    }
    // Trailing backslashes double so the closing quote stays a quote.
    out.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    out.push(u16::from(b'"'));
}

/// The `CREATE_UNICODE_ENVIRONMENT` block: sorted `K=V\0` entries plus
/// the final terminator.
fn env_block(command: &Command) -> Vec<u16> {
    let mut block = Vec::new();
    for (key, val) in &command.env {
        block.extend(key.encode_wide());
        block.push(u16::from(b'='));
        block.extend(val.encode_wide());
        block.push(0);
    }
    // An empty environment still needs a (single) empty-string entry
    // so the block ends in the documented double terminator.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

struct AttributeList {
    buf: Vec<u8>,
}

impl AttributeList {
    fn new(hpc: conpty::Hpcon) -> Result<Self, PtyError> {
        let os_err =
            |what: &str| PtyError::Spawn(format!("{what}: {}", io::Error::last_os_error()));
        let mut size: usize = 0;
        // SAFETY: the documented sizing call: a null list with a live
        // out-size pointer fails with ERROR_INSUFFICIENT_BUFFER and
        // reports the needed byte count.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &raw mut size) };
        if size == 0 {
            return Err(os_err("size ProcThreadAttributeList"));
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` is exactly the byte count the sizing call
        // asked for and outlives the list (it moves into `Self`).
        if unsafe {
            InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), 1, 0, &raw mut size)
        } == 0
        {
            return Err(os_err("InitializeProcThreadAttributeList"));
        }
        let list = Self { buf };
        // SAFETY: the list was just initialized; the attribute value
        // is the pseudoconsole handle itself, passed by value as the
        // API requires (lpValue points at a copy the OS reads during
        // this call).
        if unsafe {
            UpdateProcThreadAttribute(
                list.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc.cast_const(),
                size_of::<conpty::Hpcon>(),
                ptr::null_mut(),
                ptr::null(),
            )
        } == 0
        {
            return Err(os_err("UpdateProcThreadAttribute(pseudoconsole)"));
        }
        Ok(list)
    }

    const fn as_ptr(&self) -> *mut c_void {
        self.buf.as_ptr().cast_mut().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: `new` only returns an initialized list, and this is
        // the single deletion point.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

pub(crate) fn open_and_spawn(command: &Command, size: Size) -> Result<(Master, Child), PtyError> {
    open_and_spawn_on(process_host()?, command, size)
}

// The `cb` truncation cast is the API's own contract: the struct size
// fits u32 by definition.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn open_and_spawn_on(
    host: Host,
    command: &Command,
    size: Size,
) -> Result<(Master, Child), PtyError> {
    let api = api(host)?;
    let (conpty_input, our_input) = pipe()?;
    let (our_output, conpty_output) = pipe()?;

    let mut hpc: conpty::Hpcon = ptr::null_mut();
    // SAFETY: both pipe handles are live for the duration of the call
    // (ConPTY dups what it keeps, so our originals drop right after).
    // Grapheme measurement matches the grid's permanently set `?2027`;
    // the inbox host ignores the flag.
    let hr = unsafe {
        (api.create)(
            size.into(),
            conpty_input.as_raw_handle(),
            conpty_output.as_raw_handle(),
            conpty::PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES,
            &raw mut hpc,
        )
    };
    if hr != S_OK {
        return Err(PtyError::OpenPty(format!(
            "CreatePseudoConsole: {}",
            hresult_error(hr)
        )));
    }
    let master = Master {
        api,
        hpc,
        output: our_output,
        input: our_input,
    };
    // ConPTY holds its own references now; keeping these would stop
    // the pipes from ever breaking.
    drop(conpty_input);
    drop(conpty_output);

    let attrs = AttributeList::new(master.hpc)?;

    // Null application name plus quoted argv[0], so `CreateProcessW`
    // performs its documented PATH/.exe search as a shell launch would.
    let mut cmdline: Vec<u16> = Vec::new();
    append_quoted(&command.program, &mut cmdline);
    for arg in &command.args {
        cmdline.push(u16::from(b' '));
        append_quoted(arg, &mut cmdline);
    }
    cmdline.push(0);

    let env = env_block(command);
    let cwd_wide: Option<Vec<u16>> = command.cwd.as_ref().map(|d| {
        let mut w: Vec<u16> = d.as_os_str().encode_wide().collect();
        w.push(0);
        w
    });

    // SAFETY: plain-data struct the OS fills/reads by pointer; zeroed
    // is its documented initial state.
    let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attrs.as_ptr();
    // Force child onto pseudoconsole handles, not daemon's: without
    // STARTF_USESTDHANDLES + INVALID_HANDLE_VALUE child inherits the
    // daemon's nulled handles, output goes to NUL and window is blank.
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    // SAFETY: same plain-data argument as above.
    let mut proc_info: PROCESS_INFORMATION = unsafe { mem::zeroed() };

    // SAFETY: `cmdline` / `env` / `cwd_wide` are live, NUL-terminated
    // wide buffers for the duration of the call; `startup` carries the
    // initialized attribute list (kept alive by `attrs` until after
    // the call); inherit-handles stays FALSE because the
    // pseudoconsole attribute (not inheritance) plumbs the child's
    // console.
    let ok = unsafe {
        CreateProcessW(
            ptr::null(),
            cmdline.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr().cast(),
            cwd_wide.as_ref().map_or(ptr::null(), Vec::as_ptr),
            &raw const startup.StartupInfo,
            &raw mut proc_info,
        )
    };
    drop(attrs);
    if ok == 0 {
        return Err(PtyError::Spawn(format!(
            "CreateProcessW: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: on success both handles are fresh and owned by us; the
    // thread handle is unused, so wrapping it hands it straight to
    // OwnedHandle's Drop.
    let child = unsafe {
        drop(OwnedHandle::from_raw_handle(proc_info.hThread));
        Child {
            process: OwnedHandle::from_raw_handle(proc_info.hProcess),
        }
    };

    Ok((master, child))
}

#[cfg(test)]
mod tests {
    // The HRESULT test literals are `u32` bit patterns reinterpreted as
    // the signed `HRESULT`.
    #![allow(clippy::cast_possible_wrap)]

    use super::{
        DLL, HOST, Host, LICENSE, SECURITY_MANDATORY_MEDIUM_RID, append_quoted, bundle, conpty,
        env_block, extract, hresult_error, open_and_spawn_on,
    };

    fn test_quiesce() -> Arc<crate::quiesce::QuiesceShared> {
        Arc::new(crate::quiesce::QuiesceShared::new().expect("quiesce state"))
    }
    use crate::{Command, Size};
    use std::ffi::{OsStr, OsString};
    use std::io::Read;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    /// From `ComSpec` with the canonical location as a fallback, so the
    /// spawn resolves under a stripped-down environment whose `PATH`
    /// lacks `System32`.
    fn comspec() -> OsString {
        std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into())
    }

    /// The `ConPTY` round trip on the inbox host: a child spawned onto a
    /// pseudoconsole, its output arriving on the master's read end, an
    /// observable exit status, and a resize the live `HPCON` accepts.
    #[test]
    fn inbox_conpty_spawn_reaches_the_master_and_the_child_exits() {
        spawn_reaches_the_master_and_the_child_exits(Host::Inbox);
    }

    /// The same round trip on the bundled host.
    #[test]
    fn bundled_conpty_spawn_reaches_the_master_and_the_child_exits() {
        spawn_reaches_the_master_and_the_child_exits(Host::Bundled);
    }

    fn spawn_reaches_the_master_and_the_child_exits(host: Host) {
        const MARKER: &str = "felis-conpty-spawn";

        let mut command = Command::new(comspec());
        command.args(["/c", &format!("echo {MARKER}")]);
        let (master, mut child) = open_and_spawn_on(
            host,
            &command,
            Size {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("CreatePseudoConsole + CreateProcessW must spawn cmd.exe");

        let mut reader = master
            .clone_reader(&test_quiesce())
            .expect("clone the master read end");
        let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&collected);
        // Detached: the read is released only when `master` drops
        // (`ClosePseudoConsole` breaks the output pipe), so joining it
        // while still holding `master` would deadlock.
        drop(std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf)
                && n > 0
            {
                sink.lock()
                    .expect("collector lock")
                    .extend_from_slice(&buf[..n]);
            }
        }));

        let seen = poll_until(Duration::from_secs(30), || {
            let text =
                String::from_utf8_lossy(&collected.lock().expect("collector lock")).into_owned();
            text.contains(MARKER).then_some(text)
        });
        assert!(
            seen.is_some(),
            "the child's output never reached the master: {:?}",
            String::from_utf8_lossy(&collected.lock().expect("collector lock")),
        );

        let status = poll_until(Duration::from_secs(30), || {
            child.try_wait().expect("try_wait must not error")
        })
        .expect("the child must be reaped after it echoes and exits");
        assert!(
            status.success(),
            "`cmd /c echo` must exit cleanly; got {status:?}"
        );

        // Resized last: conhost reflows on a width change, and a reflow
        // racing the echo could split `MARKER` across the stream. There
        // is nothing to read the size back from (`ConPTY` exposes no
        // query, and the child has exited).
        master
            .resize(Size {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("ResizePseudoConsole must accept a live pseudoconsole");
    }

    const KEY_RECORD_READY: &str = "felis-key-record-ready";
    const VK_F24: u16 = 0x87;

    /// The interactive child for the key-record test: reads console
    /// input records until the `VK_F24` release and prints one line per
    /// key record. Ignored in an ordinary run: it is a fixture, and its
    /// verdict is that test's.
    #[test]
    #[ignore = "driven inside a pseudoconsole by the win32-input-mode key-record test"]
    fn key_record_fixture_reports_console_input() {
        use std::io::Write;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Console::{INPUT_RECORD, KEY_EVENT, ReadConsoleInputW};

        // The console's own devices rather than the std handles, which
        // `open_and_spawn` hands over as `INVALID_HANDLE_VALUE`.
        let input = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONIN$")
            .expect("open CONIN$");
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .open("CONOUT$")
            .expect("open CONOUT$");
        output
            .write_all(format!("{KEY_RECORD_READY}\r\n").as_bytes())
            .expect("announce readiness");

        loop {
            let mut record = INPUT_RECORD::default();
            let mut read = 0u32;
            // SAFETY: `input` is a live console input handle, and the
            // one-record buffer and the count outlive the call.
            let ok = unsafe {
                ReadConsoleInputW(input.as_raw_handle(), &raw mut record, 1, &raw mut read)
            };
            assert!(
                ok != 0,
                "ReadConsoleInputW: {}",
                std::io::Error::last_os_error()
            );
            if read == 0 || u32::from(record.EventType) != KEY_EVENT {
                continue;
            }
            // SAFETY: `EventType == KEY_EVENT` selects the `KeyEvent`
            // arm of the union, and every arm is plain data.
            let key = unsafe { record.Event.KeyEvent };
            // SAFETY: as above; `UnicodeChar` is the arm the W API fills.
            let ch = unsafe { key.uChar.UnicodeChar };
            let report = format!(
                "key:vk={},sc={},ch={ch},down={},ctrl={},rep={}\r\n",
                key.wVirtualKeyCode,
                key.wVirtualScanCode,
                key.bKeyDown,
                key.dwControlKeyState,
                key.wRepeatCount,
            );
            // One write per report: conhost may interleave VT controls
            // between separate writes.
            output
                .write_all(report.as_bytes())
                .expect("report the key record");
            if key.wVirtualKeyCode == VK_F24 && key.bKeyDown == 0 {
                return;
            }
        }
    }

    /// A win32-input-mode key written to the master reaches an
    /// interactive child on the inbox host as a console key record with
    /// every field intact. `VK_F24`, its scan code and its release have
    /// no VT encoding, so only the win32-input-mode parse can produce
    /// them.
    #[test]
    fn a_win32_input_mode_key_reaches_an_inbox_child_as_a_key_record() {
        a_win32_input_mode_key_reaches_the_child_as_a_key_record(Host::Inbox);
    }

    /// The same key record on the bundled host.
    #[test]
    fn a_win32_input_mode_key_reaches_a_bundled_child_as_a_key_record() {
        a_win32_input_mode_key_reaches_the_child_as_a_key_record(Host::Bundled);
    }

    fn a_win32_input_mode_key_reaches_the_child_as_a_key_record(host: Host) {
        use std::io::Write;

        const PRESS: &str = "key:vk=135,sc=118,ch=0,down=1,ctrl=8,rep=1";
        const RELEASE: &str = "key:vk=135,sc=118,ch=0,down=0,ctrl=8,rep=1";

        let mut command = Command::new(std::env::current_exe().expect("this test binary"));
        command.args([
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
            "windows::tests::key_record_fixture_reports_console_input",
        ]);
        let (master, mut child) = open_and_spawn_on(
            host,
            &command,
            Size {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("spawn the fixture onto a pseudoconsole");

        let mut reader = master
            .clone_reader(&test_quiesce())
            .expect("clone the master read end");
        let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&collected);
        // Detached for the same reason as in the spawn test above.
        drop(std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf)
                && n > 0
            {
                sink.lock()
                    .expect("collector lock")
                    .extend_from_slice(&buf[..n]);
            }
        }));
        let screen =
            || String::from_utf8_lossy(&collected.lock().expect("collector lock")).into_owned();
        let text = || without_escapes(&screen());

        if poll_until(Duration::from_secs(30), || {
            text().contains(KEY_RECORD_READY).then_some(())
        })
        .is_none()
        {
            fail(
                &mut child,
                &screen(),
                "the fixture never announced it was reading input",
            );
        }
        if !screen().contains("\x1b[?9001h") {
            fail(
                &mut child,
                &screen(),
                "the host must request win32-input-mode from the terminal",
            );
        }

        // Ctrl+F24 (LEFT_CTRL_PRESSED) press then release, scan code 0x76.
        let mut writer = master
            .clone_writer(&test_quiesce())
            .expect("clone the master write end");
        writer
            .write_all(b"\x1b[135;118;0;1;8;1_\x1b[135;118;0;0;8;1_")
            .expect("write the key records");

        if poll_until(Duration::from_secs(30), || {
            text().contains(RELEASE).then_some(())
        })
        .is_none()
        {
            fail(
                &mut child,
                &screen(),
                "the release record never reached the fixture",
            );
        }
        let seen = text();
        assert!(
            seen.find(PRESS)
                .is_some_and(|press| press < seen.find(RELEASE).unwrap_or(0)),
            "the press must arrive before the release: {seen:?}"
        );

        let Some(status) = poll_until(Duration::from_secs(30), || {
            child.try_wait().expect("try_wait must not error")
        }) else {
            fail(
                &mut child,
                &screen(),
                "the fixture must exit after the release",
            );
        };
        assert!(
            status.success(),
            "the fixture failed ({status:?}): {:?}",
            screen()
        );
    }

    /// The printable text of conhost's VT stream, so a control sequence
    /// conhost places mid-line cannot split a marker.
    fn without_escapes(stream: &str) -> String {
        let mut out = String::new();
        let mut chars = stream.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                if !c.is_control() {
                    out.push(c);
                }
                continue;
            }
            match chars.next() {
                Some('[') => while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {},
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// A fixture still blocked in `ReadConsoleInputW` would outlive the
    /// test otherwise.
    fn fail(child: &mut super::Child, screen: &str, what: &str) -> ! {
        drop(child.kill());
        panic!("{what}: {screen:?}");
    }

    fn poll_until<T>(budget: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(value) = probe() {
                return Some(value);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Only a Medium token runs the bundled host; elevated, SYSTEM and
    /// Low processes, and Medium Plus, use the inbox one.
    #[test]
    fn only_medium_integrity_selects_the_bundled_host() {
        assert_eq!(
            Host::for_integrity(SECURITY_MANDATORY_MEDIUM_RID),
            Host::Bundled
        );
        for rid in [0x1000, 0x2100, 0x3000, 0x4000] {
            assert_eq!(Host::for_integrity(rid), Host::Inbox, "RID {rid:#x}");
        }
    }

    fn embedded() -> [(&'static str, &'static [u8]); 3] {
        [
            (DLL, conpty::CONPTY_DLL),
            (HOST, conpty::OPENCONSOLE_EXE),
            (LICENSE, conpty::MICROSOFT_LICENSE.as_bytes()),
        ]
    }

    /// The directory holds exactly the embedded files, byte for byte, and
    /// no leftover `.tmp` file.
    fn assert_extracted(dir: &Path) {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .expect("list the version directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect();
        names.sort();
        let mut expected: Vec<_> = embedded().map(|(name, _)| OsString::from(name)).into();
        expected.sort();
        assert_eq!(names, expected);
        for (name, bytes) in embedded() {
            assert!(
                std::fs::read(dir.join(name)).expect("read an extracted file") == bytes,
                "{name} differs from the embedded bytes"
            );
        }
    }

    #[test]
    fn extraction_fills_an_empty_directory_with_the_embedded_files() {
        let root = tempfile::tempdir().expect("temporary directory");
        let dir = root.path().join("version");
        drop(extract(&dir).expect("extract into an empty directory"));
        assert_extracted(&dir);
    }

    #[test]
    fn extraction_over_a_complete_directory_rewrites_nothing() {
        let root = tempfile::tempdir().expect("temporary directory");
        drop(extract(root.path()).expect("first extraction"));
        let mtimes = || {
            embedded().map(|(name, _)| {
                std::fs::metadata(root.path().join(name))
                    .and_then(|meta| meta.modified())
                    .expect("mtime")
            })
        };
        let before = mtimes();
        drop(extract(root.path()).expect("second extraction"));
        assert_eq!(mtimes(), before);
    }

    #[test]
    fn extraction_refuses_a_differing_file_and_leaves_it() {
        let root = tempfile::tempdir().expect("temporary directory");
        let planted = root.path().join(DLL);
        std::fs::write(&planted, b"not conpty").expect("plant a differing conpty.dll");
        let Err(err) = extract(root.path()) else {
            panic!("a differing conpty.dll must be refused");
        };
        assert!(err.contains(DLL), "the error must name the file: {err}");
        assert_eq!(
            std::fs::read(&planted).expect("read the planted file"),
            b"not conpty"
        );
    }

    const RACE_DIR: &str = "FELIS_PTY_TEST_RACE_DIR";
    const RACE_EVENT: &str = "FELIS_PTY_TEST_RACE_EVENT";

    /// One racer of the cold-extraction test: waits for the shared start
    /// event, then extracts. Ignored in an ordinary run: it is a fixture,
    /// and its verdict is that test's.
    #[test]
    #[ignore = "re-executed by the concurrent extraction test"]
    fn extraction_race_fixture() {
        use windows_sys::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_SYNCHRONIZE};

        let dir = std::env::var_os(RACE_DIR).expect("the race directory");
        let event = wide(&std::env::var_os(RACE_EVENT).expect("the race event name"));
        // SAFETY: `event` is a NUL-terminated wide name.
        let event = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, event.as_ptr()) };
        assert!(
            !event.is_null(),
            "OpenEventW: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a fresh handle owned here once.
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        // SAFETY: a live event handle.
        let waited = unsafe { WaitForSingleObject(event.as_raw_handle(), 60_000) };
        assert_eq!(waited, WAIT_OBJECT_0, "the start event never fired");
        drop(extract(Path::new(&dir)).expect("a racer's extraction"));
    }

    /// Eight processes extracting into one fresh directory at once all
    /// succeed and leave exactly the embedded files.
    #[test]
    fn concurrent_extractions_into_one_empty_directory_all_succeed() {
        use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};

        let root = tempfile::tempdir().expect("temporary directory");
        let dir = root.path().join("version");
        let name = format!(
            r"Local\felis-pty-extraction-race-{}-{:016x}",
            std::process::id(),
            std::hash::BuildHasher::hash_one(&std::hash::RandomState::new(), ())
        );
        let wide_name = wide(OsStr::new(&name));
        // SAFETY: a NUL-terminated name and default security; manual
        // reset, so one `SetEvent` releases every waiter.
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, wide_name.as_ptr()) };
        assert!(
            !event.is_null(),
            "CreateEventW: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a fresh handle owned here once.
        let event = unsafe { OwnedHandle::from_raw_handle(event) };

        let racers: Vec<_> = (0..8)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().expect("this test binary"))
                    .args([
                        "--exact",
                        "--ignored",
                        "--nocapture",
                        "--test-threads=1",
                        "windows::tests::extraction_race_fixture",
                    ])
                    .env(RACE_DIR, &dir)
                    .env(RACE_EVENT, &name)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .expect("spawn a racer")
            })
            .collect();
        // Long enough for every racer to reach its wait.
        std::thread::sleep(Duration::from_secs(2));
        // SAFETY: a live event handle.
        assert!(unsafe { SetEvent(event.as_raw_handle()) } != 0);
        for racer in racers {
            let output = racer.wait_with_output().expect("wait for a racer");
            assert!(
                output.status.success(),
                "a racer failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_extracted(&dir);
    }

    /// While a daemon holds the extracted host, nothing can remove it, so
    /// `conpty.dll` never falls back to conhost; and a pseudoconsole
    /// created through the bundle is hosted by the extracted
    /// `OpenConsole.exe`, not by another copy.
    #[test]
    fn the_held_bundled_host_cannot_be_removed_and_hosts_every_spawn() {
        let dir = &bundle().expect("the bundled ConPTY loads").dir;
        let host = dir.join(HOST);
        assert!(std::fs::remove_file(&host).is_err(), "deleted the host");
        assert!(
            std::fs::rename(&host, dir.join("moved.exe")).is_err(),
            "renamed the host"
        );
        assert!(
            std::fs::rename(dir, dir.with_extension("moved")).is_err(),
            "renamed the version directory"
        );

        let mut command = Command::new(comspec());
        command.args(["/c", "echo felis-bundled-host"]);
        let (master, mut child) = open_and_spawn_on(
            Host::Bundled,
            &command,
            Size {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("spawn on the bundled host");
        let expected = std::fs::canonicalize(&host).expect("canonical host path");
        let images = openconsole_children();
        drop(child.kill());
        drop(master);
        assert!(!images.is_empty(), "no OpenConsole.exe hosts the session");
        for image in images {
            assert_eq!(
                std::fs::canonicalize(&image).expect("canonical image path"),
                expected
            );
        }
    }

    /// The full image paths of this process's `OpenConsole.exe` children.
    fn openconsole_children() -> Vec<PathBuf> {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };

        // SAFETY: a process snapshot, owned below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        assert_ne!(snapshot, INVALID_HANDLE_VALUE, "CreateToolhelp32Snapshot");
        // SAFETY: a fresh handle owned here once.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(size_of::<PROCESSENTRY32W>()).expect("entry size"),
            ..PROCESSENTRY32W::default()
        };
        let mut images = Vec::new();
        // SAFETY: a live snapshot and an entry whose `dwSize` is set.
        let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &raw mut entry) } != 0;
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if entry.th32ParentProcessID == std::process::id() && name.eq_ignore_ascii_case(HOST) {
                images.push(image_path(entry.th32ProcessID));
            }
            // SAFETY: as for `Process32FirstW`.
            more = unsafe { Process32NextW(snapshot.as_raw_handle(), &raw mut entry) } != 0;
        }
        images
    }

    fn image_path(pid: u32) -> PathBuf {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };

        // SAFETY: plain call; a null result is checked.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert!(
            !process.is_null(),
            "OpenProcess({pid}): {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a fresh handle owned here once.
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        let mut buf = [0u16; 1024];
        let mut len = u32::try_from(buf.len()).expect("buffer length");
        // SAFETY: `buf` holds `len` UTF-16 units and both outlive the call.
        let ok = unsafe {
            QueryFullProcessImageNameW(
                process.as_raw_handle(),
                PROCESS_NAME_WIN32,
                buf.as_mut_ptr(),
                &raw mut len,
            )
        };
        assert!(
            ok != 0,
            "QueryFullProcessImageNameW({pid}): {}",
            std::io::Error::last_os_error()
        );
        PathBuf::from(OsString::from_wide(&buf[..len as usize]))
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    /// A `HRESULT_FROM_WIN32` result unwraps to its bare Win32 code; a
    /// non-Win32 HRESULT passes through unchanged.
    #[test]
    fn hresult_error_unwraps_facility_win32() {
        // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND) == 0x8007_0002.
        assert_eq!(
            hresult_error(0x8007_0002_u32 as i32).raw_os_error(),
            Some(2)
        );
        assert_eq!(
            hresult_error(0x8000_4005_u32 as i32).raw_os_error(),
            Some(0x8000_4005_u32 as i32)
        );
    }

    fn quoted(arg: &str) -> String {
        let mut out = Vec::new();
        append_quoted(OsStr::new(arg), &mut out);
        String::from_utf16(&out).unwrap()
    }

    /// `CommandLineToArgvW` quoting: bare when unambiguous, quoted
    /// otherwise, backslash runs doubled only before a quote or the
    /// closing delimiter.
    #[test]
    fn append_quoted_follows_argv_rules() {
        // A path separator must not trigger doubling.
        assert_eq!(quoted("foo"), "foo");
        assert_eq!(quoted(r"C:\Windows\cmd.exe"), r"C:\Windows\cmd.exe");
        assert_eq!(quoted("a b"), "\"a b\"");
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("a\"b"), "\"a\\\"b\"");
        assert_eq!(quoted(r#"a\""#), r#""a\\\"""#);
    }

    fn env_of(pairs: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new("x");
        cmd.env_clear();
        for (k, v) in pairs {
            cmd.env(k, v);
        }
        cmd
    }

    /// Sorted `KEY=VAL\0` entries closed by the documented double NUL.
    #[test]
    fn env_block_is_sorted_and_double_terminated() {
        let cmd = env_of(&[("B", "2"), ("A", "1")]);
        let block = String::from_utf16(&env_block(&cmd)).unwrap();
        assert_eq!(block, "A=1\u{0}B=2\u{0}\u{0}");
    }

    /// An empty environment still ends in the double NUL.
    #[test]
    fn env_block_empty_is_double_nul() {
        let cmd = env_of(&[]);
        let block = String::from_utf16(&env_block(&cmd)).unwrap();
        assert_eq!(block, "\u{0}\u{0}");
    }
}

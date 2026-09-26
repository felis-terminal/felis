//! Windows PTY backend: `ConPTY` (`CreatePseudoConsole`) via `windows-sys`.
//!
//! Attaches `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` directly via `CreateProcessW`.
//! Session teardown polls `try_wait` because `ConPTY` closes output pipes
//! only on `ClosePseudoConsole`.
#![allow(unsafe_code)]

use std::{
    ffi::{OsStr, c_void},
    fs::File,
    io::{self, Read},
    mem,
    os::windows::ffi::OsStrExt,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    os::windows::process::ExitStatusExt,
    ptr,
};

use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, S_OK, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole},
    System::Pipes::CreatePipe,
    System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    },
};
use windows_sys::core::HRESULT;

use crate::{Command, PtyError, Size};

/// `CreatePseudoConsole` flag synthesizing `INPUT_RECORD`s from win32-input-mode
/// keys (`CSI Vk;... _`). Without this, `PSReadLine` receives nothing under
/// `DECSET ?9001`. Defined locally because `windows-sys` 0.59 omits the constant.
const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x8;

impl From<Size> for COORD {
    #[allow(clippy::cast_possible_wrap)]
    fn from(size: Size) -> Self {
        Self {
            X: size.cols as i16,
            Y: size.rows as i16,
        }
    }
}

/// Unwrap `HRESULT_FROM_WIN32` (`0x8007_xxxx`) to bare Win32 codes so
/// `io::Error::from_raw_os_error` formats known error descriptions.
#[allow(clippy::cast_possible_wrap)]
fn hresult_error(hr: HRESULT) -> io::Error {
    // HRESULT_FROM_WIN32(code) == 0x8007_0000 | (code & 0xFFFF).
    const FACILITY_WIN32: HRESULT = 0x8007_0000_u32 as HRESULT;
    let code = if hr & (0xFFFF_0000_u32 as HRESULT) == FACILITY_WIN32 {
        hr & 0xFFFF
    } else {
        hr
    };
    io::Error::from_raw_os_error(code)
}

/// The pseudoconsole plus our ends of its I/O pipes.
///
/// Drop order: `ClosePseudoConsole` runs first (in `Drop`), which
/// flushes pending output and then breaks the output pipe, the break
/// the reader thread sees as EOF. The pipe handles close afterwards.
pub(crate) struct Master {
    hpc: HPCON,
    /// Read end of the `ConPTY` output pipe (`ConPTY` writes VT here).
    output: OwnedHandle,
    /// Write end of the `ConPTY` input pipe (keystrokes go here).
    input: OwnedHandle,
}

impl Drop for Master {
    fn drop(&mut self) {
        // SAFETY: `self.hpc` came from a successful
        // `CreatePseudoConsole` and is closed exactly once, here.
        unsafe { ClosePseudoConsole(self.hpc) };
    }
}

impl Master {
    pub(crate) fn clone_reader(&self) -> io::Result<MasterReader> {
        Ok(MasterReader(File::from(self.output.try_clone()?)))
    }

    pub(crate) fn clone_writer(&self) -> io::Result<File> {
        Ok(File::from(self.input.try_clone()?))
    }

    pub(crate) fn resize(&self, size: Size) -> io::Result<()> {
        // SAFETY: `self.hpc` is a live pseudoconsole handle (closed
        // only in `Drop`); `COORD` is plain data.
        let hr = unsafe { ResizePseudoConsole(self.hpc, size.into()) };
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
/// `ERROR_BROKEN_PIPE`, which is what a closed pseudoconsole produces.
pub(crate) struct MasterReader(File);

impl Read for MasterReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(0),
            other => other,
        }
    }
}

pub(crate) struct Child {
    process: OwnedHandle,
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
fn append_quoted(arg: &OsStr, out: &mut Vec<u16>) {
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
    fn new(hpc: HPCON) -> Result<Self, PtyError> {
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
                hpc as *const c_void,
                size_of::<HPCON>(),
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

// The `cb` truncation cast is the API's own contract: the struct size
// fits u32 by definition.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn open_and_spawn(command: &Command, size: Size) -> Result<(Master, Child), PtyError> {
    let (conpty_input, our_input) = pipe()?;
    let (our_output, conpty_output) = pipe()?;

    let mut hpc: HPCON = 0;
    // SAFETY: both pipe handles are live for the duration of the call
    // (ConPTY dups what it keeps, so our originals drop right after).
    let hr = unsafe {
        CreatePseudoConsole(
            size.into(),
            conpty_input.as_raw_handle(),
            conpty_output.as_raw_handle(),
            PSEUDOCONSOLE_WIN32_INPUT_MODE,
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

    use super::{append_quoted, env_block, hresult_error, open_and_spawn};
    use crate::{Command, Size};
    use std::ffi::OsStr;
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// From `ComSpec` with the canonical location as a fallback, so the
    /// spawn resolves under a stripped-down environment whose `PATH`
    /// lacks `System32`.
    fn comspec() -> std::ffi::OsString {
        std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into())
    }

    /// The `ConPTY` round trip against real conhost: a child spawned onto
    /// a pseudoconsole, its output arriving on the master's read end, an
    /// observable exit status, and a resize the live `HPCON` accepts.
    #[test]
    fn conpty_spawn_reaches_the_master_and_the_child_exits() {
        const MARKER: &str = "felis-conpty-spawn";

        let mut command = Command::new(comspec());
        command.args(["/c", &format!("echo {MARKER}")]);
        let (master, mut child) = open_and_spawn(
            &command,
            Size {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("CreatePseudoConsole + CreateProcessW must spawn cmd.exe");

        let mut reader = master.clone_reader().expect("clone the master read end");
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
    /// interactive child as a console key record with every field
    /// intact. `VK_F24`, its scan code and its release have no VT
    /// encoding, so only the win32-input-mode parse can produce them.
    #[test]
    fn a_win32_input_mode_key_reaches_the_child_as_a_key_record() {
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
        let (master, mut child) = open_and_spawn(
            &command,
            Size {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("spawn the fixture onto a pseudoconsole");

        let mut reader = master.clone_reader().expect("clone the master read end");
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
                "conhost must request win32-input-mode from the terminal",
            );
        }

        // Ctrl+F24 (LEFT_CTRL_PRESSED) press then release, scan code 0x76.
        let mut writer = master.clone_writer().expect("clone the master write end");
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

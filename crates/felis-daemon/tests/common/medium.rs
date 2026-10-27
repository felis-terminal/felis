//! Running a test at Medium integrity, where a daemon uses the bundled
//! console host: a test process above Medium reruns the one test in a
//! Medium-integrity copy of itself.
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::io::Read as _;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree, TRUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount,
    GetTokenInformation, PSID, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, SecurityImpersonation,
    SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel, TokenPrimary,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_READ,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
    INFINITE, OpenProcess, OpenProcessToken, PROCESS_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

const MEDIUM_RID: u32 = 0x2000;
/// `SE_GROUP_INTEGRITY`, from a `windows-sys` module nothing else needs.
const SE_GROUP_INTEGRITY: u32 = 0x20;
const RELAUNCHED: &str = "FELIS_TEST_AT_MEDIUM";

/// Runs `body` here when this process is at Medium integrity, else
/// reruns `test` (this test's own name) at Medium and asserts it passed.
pub(crate) fn run(test: &str, body: impl FnOnce()) {
    if integrity_rid() == MEDIUM_RID {
        body();
        return;
    }
    assert!(
        std::env::var_os(RELAUNCHED).is_none(),
        "the rerun is still above Medium integrity"
    );
    let (code, output) = rerun_at_medium(test);
    assert_eq!(code, 0, "the test failed at Medium integrity:\n{output}");
}

fn token() -> OwnedHandle {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo-handle needs no close; `token` is a live
    // out-pointer.
    let ok = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY,
            &raw mut token,
        )
    };
    assert_ne!(
        ok,
        0,
        "OpenProcessToken: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: fresh and ours.
    unsafe { OwnedHandle::from_raw_handle(token) }
}

fn integrity_rid() -> u32 {
    let token = token();
    let mut len = 0u32;
    // SAFETY: the size probe.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenIntegrityLevel,
            ptr::null_mut(),
            0,
            &raw mut len,
        );
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` is the size the probe reported.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenIntegrityLevel,
            buf.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    };
    assert_ne!(
        ok,
        0,
        "GetTokenInformation: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `buf` holds a `TOKEN_MANDATORY_LABEL` whose SID points
    // into `buf`.
    unsafe {
        let sid = buf
            .as_ptr()
            .cast::<TOKEN_MANDATORY_LABEL>()
            .read_unaligned()
            .Label
            .Sid;
        let count = *GetSidSubAuthorityCount(sid);
        *GetSidSubAuthority(sid, u32::from(count) - 1)
    }
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

/// This environment plus the rerun marker, as a sorted Unicode block.
fn environment() -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(key, _)| key != RELAUNCHED)
        .chain([(RELAUNCHED.into(), "1".into())])
        .collect();
    vars.sort_by_key(|(key, _)| key.to_ascii_uppercase());
    let mut block = Vec::new();
    for (key, value) in vars {
        block.extend(key.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

#[allow(clippy::cast_possible_truncation)]
fn rerun_at_medium(test: &str) -> (u32, String) {
    let token = token();
    let mut medium: HANDLE = ptr::null_mut();
    // SAFETY: `token` is live with `TOKEN_DUPLICATE`; `medium` is a live
    // out-pointer.
    let ok = unsafe {
        DuplicateTokenEx(
            token.as_raw_handle(),
            0,
            ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &raw mut medium,
        )
    };
    assert_ne!(
        ok,
        0,
        "DuplicateTokenEx: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: fresh and ours.
    let medium = unsafe { OwnedHandle::from_raw_handle(medium) };
    let mut sid: PSID = ptr::null_mut();
    let medium_sid = wide("S-1-16-8192".as_ref());
    // SAFETY: a NUL-terminated SID string and a live out-pointer; the SID
    // is freed with `LocalFree` below.
    let ok = unsafe { ConvertStringSidToSidW(medium_sid.as_ptr(), &raw mut sid) };
    assert_ne!(ok, 0, "ConvertStringSidToSidW");
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: SE_GROUP_INTEGRITY,
        },
    };
    // SAFETY: `label` points at `sid`, live until the `LocalFree`.
    let ok = unsafe {
        SetTokenInformation(
            medium.as_raw_handle(),
            TokenIntegrityLevel,
            (&raw const label).cast(),
            size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid),
        )
    };
    let set = std::io::Error::last_os_error();
    // SAFETY: allocated by `ConvertStringSidToSidW`, freed once.
    unsafe { LocalFree(sid) };
    assert_ne!(ok, 0, "SetTokenInformation: {set}");

    let dir = tempfile::TempDir::new().unwrap();
    let log = dir.path().join("output");
    let inheritable = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: TRUE,
    };
    // SAFETY: a NUL-terminated path and initialized attributes; a file
    // rather than a pipe, so a grandchild still holding the handle never
    // keeps the read below from finishing.
    let output = unsafe {
        CreateFileW(
            wide(log.as_os_str()).as_ptr(),
            FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            &raw const inheritable,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    assert!(
        !output.is_null() && output != INVALID_HANDLE_VALUE,
        "CreateFileW: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: fresh and ours.
    let output = unsafe { OwnedHandle::from_raw_handle(output) };

    let exe = std::env::current_exe().unwrap();
    let mut cmdline = wide(
        format!(
            "\"{}\" {test} --exact --nocapture --test-threads=1",
            exe.display()
        )
        .as_ref(),
    );
    let application = wide(exe.as_os_str());
    let env = environment();
    // SAFETY: plain data, zeroed as documented.
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESTDHANDLES;
    startup.hStdOutput = output.as_raw_handle();
    startup.hStdError = output.as_raw_handle();
    // SAFETY: as above.
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every buffer is NUL-terminated and outlives the call; the
    // output handle is inheritable for the child's standard handles.
    let ok = unsafe {
        CreateProcessAsUserW(
            medium.as_raw_handle(),
            application.as_ptr(),
            cmdline.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            TRUE,
            CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr().cast(),
            ptr::null(),
            &raw const startup,
            &raw mut info,
        )
    };
    // SAFETY: a plain query.
    assert_ne!(ok, 0, "CreateProcessAsUserW: {}", unsafe { GetLastError() });
    drop(output);
    // SAFETY: fresh and ours.
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(info.hProcess),
            OwnedHandle::from_raw_handle(info.hThread),
        )
    };
    drop(thread);
    let mut code = 0u32;
    // SAFETY: a live process handle and a live out-pointer.
    unsafe {
        WaitForSingleObject(process.as_raw_handle(), INFINITE);
        GetExitCodeProcess(process.as_raw_handle(), &raw mut code);
    }
    let mut text = String::new();
    if let Ok(mut file) = std::fs::File::open(&log) {
        let _read = file.read_to_string(&mut text);
    }
    (code, text)
}

/// Whether `pid` names a process that has not exited.
pub(crate) fn alive(pid: u32) -> bool {
    // SAFETY: a plain open; a null result is checked.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: fresh and ours.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    // SAFETY: a live handle with `SYNCHRONIZE`.
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_TIMEOUT }
}

/// The running `felis-daemon` processes `parent` started.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn daemon_children(parent: u32) -> Vec<u32> {
    // SAFETY: a plain snapshot; an invalid result is checked.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert_ne!(
        snapshot,
        INVALID_HANDLE_VALUE,
        "CreateToolhelp32Snapshot: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: fresh and ours.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    // SAFETY: plain data, zeroed as documented.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut children = Vec::new();
    // SAFETY: a live snapshot and an entry whose size is set.
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if entry.th32ParentProcessID == parent && name.eq_ignore_ascii_case("felis-daemon.exe") {
            children.push(entry.th32ProcessID);
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &raw mut entry) } != 0;
    }
    children
}

/// Ends a daemon the test started indirectly, as a successor.
pub(crate) fn terminate(pid: u32) {
    // SAFETY: a plain open; a null result is checked.
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        return;
    }
    // SAFETY: fresh and ours.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    // SAFETY: a live handle with `PROCESS_TERMINATE`.
    unsafe { TerminateProcess(process.as_raw_handle(), 1) };
}

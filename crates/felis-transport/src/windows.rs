//! Windows named-pipe backend for the [`crate::local`] carrier facade.
//!
//! Enforces DACL per-instance and validates client SID before `Hello` (REQ-106)
//! via `GetNamedPipeClientProcessId` and `OpenProcessToken`.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::Mutex;
use tracing::warn;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, FALSE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::local::{AcceptError, BindError, Endpoint};
use crate::peer::PeerError;
use crate::retry::{RetryPolicy, nonzero, retry_with_backoff};

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the pointer is an owned, self-contained security descriptor
// allocated by the OS; it is not aliased and is freed exactly once on
// drop. Moving it across threads (the Listener lives in the daemon's
// async runtime) is sound: the OS object has no thread affinity.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above; the descriptor is only read (passed by pointer to
// pipe creation), never mutated through a shared `&`.
unsafe impl Sync for SecurityDescriptor {}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was returned by
            // ConvertStringSecurityDescriptorToSecurityDescriptorW,
            // which documents LocalFree as the matching deallocator.
            // Dropped once; `Drop` runs at most once per value.
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: `self.0` is a handle we opened (process or
            // token) and have not closed elsewhere; closed once on
            // drop.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_ptr_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0isize;
    // SAFETY: `p` points at an OS-allocated NUL-terminated wide
    // string; we stop at the NUL, never reading past it.
    let slice = unsafe {
        while *p.offset(len) != 0 {
            len += 1;
        }
        std::slice::from_raw_parts(p, len as usize)
    };
    String::from_utf16_lossy(slice)
}

fn sid_to_string(psid: PSID) -> io::Result<String> {
    let mut raw: *mut u16 = ptr::null_mut();
    // SAFETY: `psid` is a valid SID (from a token we just queried).
    // ConvertSidToStringSidW allocates `raw` with LocalAlloc; we free
    // it with LocalFree below.
    let ok = unsafe { ConvertSidToStringSidW(psid, &raw mut raw) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let s = wide_ptr_to_string(raw);
    // SAFETY: `raw` was allocated by ConvertSidToStringSidW, whose
    // documented deallocator is LocalFree; freed exactly once.
    unsafe {
        LocalFree(raw.cast());
    }
    Ok(s)
}

fn token_user_sid_string(token: HANDLE) -> io::Result<String> {
    let mut len: u32 = 0;
    // SAFETY: passing a null buffer with length 0 is the documented
    // size-probe form; `len` is a valid out-pointer.
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &raw mut len);
    }
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` is `len` bytes, exactly what the probe asked for;
    // the token handle is valid and opened with TOKEN_QUERY.
    let ok = unsafe {
        GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &raw mut len)
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `buf` holds a TOKEN_USER followed by the SID
    // bytes it points into; the struct's `User.Sid` is valid for the
    // lifetime of `buf`, which outlives the sid_to_string call. Read
    // unaligned because a `Vec<u8>` buffer carries no `TOKEN_USER`
    // alignment guarantee; `User.Sid` still points into `buf`.
    let psid = unsafe { buf.as_ptr().cast::<TOKEN_USER>().read_unaligned() }
        .User
        .Sid;
    sid_to_string(psid)
}

fn current_user_sid_string() -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no
    // closing; OpenProcessToken fills `token` with a real handle we
    // wrap in OwnedHandle below.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    token_user_sid_string(token.0)
}

fn security_descriptor_for(sid: &str) -> io::Result<SecurityDescriptor> {
    // A protected (`P`) DACL whose lone allow-ACE implicitly denies
    // everyone else.
    let sddl = to_wide(&format!("D:P(A;;GA;;;{sid})"));
    let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is a valid NUL-terminated wide string; `psd` is a
    // valid out-pointer; size out-pointer is null (we don't need it).
    // On success `psd` owns an allocation freed via LocalFree by
    // SecurityDescriptor::drop.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut psd,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SecurityDescriptor(psd))
}

fn client_sid_string(server: &NamedPipeServer) -> io::Result<String> {
    let handle = server.as_raw_handle() as HANDLE;
    let mut pid: u32 = 0;
    // SAFETY: `handle` is the live server end of a connected pipe;
    // `pid` is a valid out-pointer.
    let ok = unsafe { GetNamedPipeClientProcessId(handle, &raw mut pid) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: opening another process for limited query is sound; the
    // returned handle is wrapped in OwnedHandle and closed on drop.
    //
    // Accepted TOCTOU: between GetNamedPipeClientProcessId and this
    // OpenProcess the client can exit and the pid be recycled, so the
    // SID we read may belong to the recycled process. Exploiting it
    // requires winning a pid-reuse race as an attacker the pipe DACL
    // already excludes from opening the pipe at all, so the risk is
    // recorded rather than chased. Impersonation-based checks were
    // rejected because the client has written no bytes yet (see the
    // module doc).
    let proc = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if proc.is_null() {
        return Err(io::Error::last_os_error());
    }
    let proc = OwnedHandle(proc);

    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `proc.0` is a valid process handle with query rights;
    // `token` is filled with a real handle wrapped below.
    let ok = unsafe { OpenProcessToken(proc.0, TOKEN_QUERY, &raw mut token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    token_user_sid_string(token.0)
}

/// `\\.\pipe\felis.<sid>.daemon`. The SID rather than a session id:
/// stable across logins, and a per-user name keeps two users' daemons
/// from colliding on `first_pipe_instance`.
pub fn default_pipe_name() -> io::Result<String> {
    let sid = current_user_sid_string()?;
    Ok(format!(r"\\.\pipe\felis.{sid}.daemon"))
}

/// Retries only `ERROR_PIPE_BUSY` (the documented client pattern); any
/// other error rides the `Ok` arm so it ends the schedule at once.
/// Daemon-not-yet-up is the client-core dial retry's job.
pub(crate) async fn connect(pipe_name: &str) -> io::Result<NamedPipeClient> {
    const RETRY: RetryPolicy = RetryPolicy {
        initial_backoff: std::time::Duration::from_millis(50),
        max_backoff: std::time::Duration::from_millis(50),
        max_attempts: nonzero(11),
    };
    let outcome = retry_with_backoff(
        || async {
            match ClientOptions::new().open(pipe_name) {
                Ok(client) => Ok(Ok(client)),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.cast_signed()) => Err(e),
                Err(e) => Ok(Err(e)),
            }
        },
        RETRY,
    )
    .await;
    match outcome {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(e)) => Err(e),
        Err(err) => Err(err.source),
    }
}

pub(crate) struct Listener {
    pipe_name: String,
    expected_sid: String,
    security: SecurityDescriptor,
    /// Exactly one instance is always listening: `accept` creates the
    /// next before returning the connected one.
    pending: Mutex<NamedPipeServer>,
}

impl Listener {
    pub(crate) fn bind(endpoint: &Endpoint) -> Result<Self, BindError> {
        let pipe_name = endpoint.pipe_name().to_owned();
        let expected_sid = current_user_sid_string().map_err(BindError::Io)?;
        let security = security_descriptor_for(&expected_sid).map_err(BindError::Io)?;
        let first = create_instance(&pipe_name, &security, true).map_err(BindError::Io)?;
        Ok(Self {
            pipe_name,
            expected_sid,
            security,
            pending: Mutex::new(first),
        })
    }

    pub(crate) async fn accept(&self) -> Result<NamedPipeServer, AcceptError> {
        let mut guard = self.pending.lock().await;
        guard.connect().await.map_err(AcceptError::Io)?;
        let next =
            create_instance(&self.pipe_name, &self.security, false).map_err(AcceptError::Io)?;
        let connected = std::mem::replace(&mut *guard, next);
        drop(guard);

        match client_sid_string(&connected) {
            Ok(peer) => admit(peer, &self.expected_sid)
                .map(|()| connected)
                .map_err(AcceptError::Peer),
            Err(e) => Err(AcceptError::Peer(PeerError::Sockopt(e))),
        }
    }

    #[must_use]
    pub(crate) fn endpoint(&self) -> Endpoint {
        Endpoint::pipe(self.pipe_name.clone())
    }
}

/// Free-standing so the reject arm is testable: a test can only connect
/// as the user running it.
fn admit(peer: String, expected: &str) -> Result<(), PeerError> {
    if peer == expected {
        return Ok(());
    }
    warn!(%peer, %expected, "rejecting pipe client with mismatched SID");
    Err(PeerError::SidMismatch {
        peer,
        expected: expected.to_owned(),
    })
}

/// `first` sets `first_pipe_instance`, so the create fails when another
/// process already owns the name.
fn create_instance(
    pipe_name: &str,
    security: &SecurityDescriptor,
    first: bool,
) -> io::Result<NamedPipeServer> {
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: FALSE,
    };
    // SAFETY: `attrs` points at a fully-initialized SECURITY_ATTRIBUTES
    // whose security descriptor (`security.0`) outlives this call
    // (owned by the Listener). tokio reads the attributes synchronously
    // during creation and does not retain the pointer.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .create_with_security_attributes_raw(pipe_name, (&raw mut attrs).cast::<c_void>())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetLengthSid,
        GetSecurityDescriptorControl, INHERITED_ACE, SE_DACL_PRESENT, SE_DACL_PROTECTED,
    };

    use super::*;

    /// `windows-sys` exports this only under `Win32_System_SystemServices`,
    /// a feature not enabled for one `u8`.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    fn unique_pipe_name(tag: &str) -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        format!(r"\\.\pipe\felis-test.{}.{tag}.{seq}", std::process::id())
    }

    /// `Vec<u32>` rather than `Vec<u8>`: a `PSID` must be DWORD-aligned.
    fn current_user_sid_words() -> Vec<u32> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: GetCurrentProcess returns a pseudo-handle needing no
        // close; OpenProcessToken fills `token` with a real handle,
        // wrapped in OwnedHandle below.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
        assert_ne!(ok, 0, "OpenProcessToken: {}", io::Error::last_os_error());
        let token = OwnedHandle(token);

        let mut len: u32 = 0;
        // SAFETY: the documented size probe: a null buffer of length 0
        // with a valid out-size pointer.
        unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &raw mut len) };
        assert_ne!(len, 0, "sizing TokenUser: {}", io::Error::last_os_error());
        let mut buf = vec![0u8; len as usize];
        // SAFETY: `buf` is exactly the byte count the probe asked for and
        // the token was opened with TOKEN_QUERY.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast(),
                len,
                &raw mut len,
            )
        };
        assert_ne!(
            ok,
            0,
            "GetTokenInformation(TokenUser): {}",
            io::Error::last_os_error()
        );
        // SAFETY: on success `buf` holds a TOKEN_USER whose `User.Sid`
        // points into it; read unaligned because a `Vec<u8>` carries no
        // TOKEN_USER alignment guarantee.
        let psid = unsafe { buf.as_ptr().cast::<TOKEN_USER>().read_unaligned() }
            .User
            .Sid;
        // SAFETY: `psid` is a valid SID living inside `buf`.
        let sid_len = unsafe { GetLengthSid(psid) } as usize;
        let mut words = vec![0u32; sid_len.div_ceil(size_of::<u32>())];
        // SAFETY: the regions are disjoint allocations and the
        // destination holds at least `sid_len` bytes.
        unsafe {
            ptr::copy_nonoverlapping(psid.cast::<u8>(), words.as_mut_ptr().cast::<u8>(), sid_len);
        }
        words
    }

    #[derive(Debug)]
    struct AceFacts {
        ace_type: u8,
        inherited: bool,
        names_the_owning_user: bool,
    }

    #[derive(Debug)]
    struct DaclFacts {
        /// A present-but-NULL DACL grants everyone everything.
        present_and_not_null: bool,
        protected: bool,
        ace_count: u32,
        sole_ace: Option<AceFacts>,
    }

    /// Read DACL facts directly from the pipe handle.
    ///
    /// Kernel object creation maps generic rights to object-specific ones.
    /// Opening by name would consume a pending instance, so query the server
    /// handle which carries `READ_CONTROL` via `PIPE_ACCESS_DUPLEX`.
    fn dacl_facts_of(handle: HANDLE) -> DaclFacts {
        let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let mut pdacl: *mut ACL = ptr::null_mut();
        // SAFETY: `handle` is a live named-pipe handle owned by the
        // caller; every out-pointer is valid. On success the security
        // descriptor is an owned allocation freed with LocalFree below,
        // and `pdacl` points into it rather than owning anything.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &raw mut pdacl,
                ptr::null_mut(),
                &raw mut psd,
            )
        };
        assert_eq!(
            status, 0,
            "GetSecurityInfo returned {status} \
             (ERROR_ACCESS_DENIED here means the pipe handle lacks READ_CONTROL)"
        );

        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        // SAFETY: `psd` is the descriptor GetSecurityInfo just returned;
        // both out-pointers are valid locals.
        let ok = unsafe { GetSecurityDescriptorControl(psd, &raw mut control, &raw mut revision) };
        assert_ne!(
            ok,
            0,
            "GetSecurityDescriptorControl: {}",
            io::Error::last_os_error()
        );

        let present_and_not_null = control & SE_DACL_PRESENT != 0 && !pdacl.is_null();
        let mut ace_count = 0;
        let mut sole_ace = None;
        if present_and_not_null {
            let mut size = ACL_SIZE_INFORMATION {
                AceCount: 0,
                AclBytesInUse: 0,
                AclBytesFree: 0,
            };
            // SAFETY: `pdacl` is a valid ACL inside `psd`, and the
            // buffer/length pair describes the matching struct for
            // AclSizeInformation.
            let ok = unsafe {
                GetAclInformation(
                    pdacl,
                    (&raw mut size).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            };
            assert_ne!(ok, 0, "GetAclInformation: {}", io::Error::last_os_error());
            ace_count = size.AceCount;
            if ace_count == 1 {
                sole_ace = Some(ace_facts(pdacl));
            }
        }

        // SAFETY: `psd` came from GetSecurityInfo, whose documented
        // deallocator is LocalFree; freed exactly once, after the last
        // read of anything pointing into it.
        unsafe {
            LocalFree(psd.cast());
        }
        DaclFacts {
            present_and_not_null,
            protected: control & SE_DACL_PROTECTED != 0,
            ace_count,
            sole_ace,
        }
    }

    fn ace_facts(dacl: *mut ACL) -> AceFacts {
        let mut pace: *mut c_void = ptr::null_mut();
        // SAFETY: `dacl` is a valid ACL with at least one ACE; `pace` is
        // a valid out-pointer that receives a borrow into the ACL.
        let ok = unsafe { GetAce(dacl, 0, &raw mut pace) };
        assert_ne!(ok, 0, "GetAce(0): {}", io::Error::last_os_error());

        // SAFETY: every ACE begins with an ACE_HEADER. Read unaligned:
        // ACEs are DWORD-aligned within the ACL, which is weaker than
        // what a typed read would assume of an arbitrary allocation.
        let header = unsafe { pace.cast::<ACE_HEADER>().read_unaligned() };

        // SAFETY: `pace` points at an ACE inside the ACL; taking the
        // field's address is a pointer computation that never forms a
        // reference, so the ACE's weaker alignment cannot bite.
        let ace_sid: PSID =
            unsafe { &raw mut (*pace.cast::<ACCESS_ALLOWED_ACE>()).SidStart }.cast();
        let ours = current_user_sid_words();
        // SAFETY: both arguments are valid SIDs: one inside the ACL,
        // one owned by `ours` for the duration of the call.
        let names_the_owning_user =
            unsafe { EqualSid(ace_sid, ours.as_ptr().cast::<c_void>().cast_mut()) } != 0;

        AceFacts {
            ace_type: header.AceType,
            inherited: u32::from(header.AceFlags) & INHERITED_ACE != 0,
            names_the_owning_user,
        }
    }

    /// REQ-106a, the `0600` half: one non-inherited allow-ACE for the
    /// daemon's own user.
    #[tokio::test]
    async fn the_pipe_dacl_admits_only_the_owning_user_sid() {
        let name = unique_pipe_name("dacl");
        let listener = Listener::bind(&Endpoint::pipe(name)).unwrap();

        let facts = {
            let pending = listener.pending.lock().await;
            dacl_facts_of(pending.as_raw_handle() as HANDLE)
        };

        assert!(
            facts.present_and_not_null,
            "an absent or NULL DACL grants everyone everything: {facts:?}"
        );
        assert!(
            facts.protected,
            "an unprotected DACL can be widened by an inherited ACE: {facts:?}"
        );
        assert_eq!(facts.ace_count, 1, "exactly one ACE may appear: {facts:?}");
        let ace = facts
            .sole_ace
            .as_ref()
            .expect("one ACE means there is one to inspect");
        assert_eq!(
            ace.ace_type, ACCESS_ALLOWED_ACE_TYPE,
            "the lone ACE must be an allow-ACE: {facts:?}"
        );
        assert!(
            !ace.inherited,
            "the lone ACE must be the one this code set, not an inherited one: {facts:?}"
        );
        assert!(
            ace.names_the_owning_user,
            "the lone ACE must name the owning user: {facts:?}"
        );
    }

    /// REQ-106a, the `SO_PEERCRED` half: the SID compared is the
    /// connecting process's real one.
    #[tokio::test]
    async fn a_same_user_client_is_admitted_and_its_sid_is_the_one_checked() {
        let name = unique_pipe_name("admit");
        let listener = Listener::bind(&Endpoint::pipe(name.clone())).unwrap();

        let server_task = tokio::spawn(async move {
            let mut server = listener.accept().await.expect("same-user client admitted");
            let mut buf = [0u8; 4];
            server.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
            server.write_all(b"pong").await.unwrap();
            client_sid_string(&server).unwrap()
        });

        let mut client = connect(&name).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");

        let observed = server_task.await.unwrap();
        assert_eq!(
            observed,
            current_user_sid_string().unwrap(),
            "the accept path must have resolved this process's own SID"
        );
    }

    /// REQ-106a's reject arm, unreachable through `accept` on a
    /// single-account runner. The error names both SIDs because the
    /// daemon's log line is the only place a rejection is visible.
    #[test]
    fn a_mismatched_client_sid_is_rejected_naming_both_sids() {
        let peer = "S-1-5-21-1111111111-2222222222-3333333333-1002";
        let expected = "S-1-5-21-1111111111-2222222222-3333333333-1001";

        let err = admit(peer.to_owned(), expected).expect_err("a foreign SID must be rejected");

        match err {
            PeerError::SidMismatch {
                peer: got,
                expected: want,
            } => {
                assert_eq!(got, peer);
                assert_eq!(want, expected);
            }
            other => panic!("expected PeerError::SidMismatch, got {other:?}"),
        }
    }

    /// Pinned separately so a rule rejecting everything fails here.
    #[test]
    fn a_matching_client_sid_is_admitted() {
        let sid = current_user_sid_string().unwrap();
        assert!(admit(sid.clone(), &sid).is_ok());
    }

    #[tokio::test]
    async fn a_second_listener_cannot_take_a_bound_pipe_name() {
        let name = unique_pipe_name("collide");
        let _first = Listener::bind(&Endpoint::pipe(name.clone())).unwrap();

        let second = Listener::bind(&Endpoint::pipe(name));

        assert!(
            second.is_err(),
            "binding an already-served pipe name must fail"
        );
    }

    #[test]
    fn the_default_pipe_name_carries_the_user_sid() {
        let name = default_pipe_name().unwrap();
        let sid = current_user_sid_string().unwrap();
        assert!(
            name.starts_with(r"\\.\pipe\felis.") && name.ends_with(".daemon"),
            "unexpected default pipe name: {name}"
        );
        assert!(name.contains(&sid), "{name} must carry the user SID {sid}");
    }

    /// A descriptor with no DACL would admit everyone.
    #[test]
    fn a_malformed_sid_fails_the_descriptor_build() {
        assert!(security_descriptor_for("not-a-sid").is_err());
    }
}

//! The private pipe a Windows in-place upgrade runs its hand-over on
//! (`docs/explanation/architecture/overview.md` "In-place upgrade"):
//! one instance, under a name nobody can guess, admitting only the
//! process the predecessor started.

use std::{ffi::c_void, hash::BuildHasher as _, io, os::windows::io::AsRawHandle as _};

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::{
    Foundation::{FALSE, HANDLE},
    Security::SECURITY_ATTRIBUTES,
    System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
};

use crate::windows::{current_identity, security_descriptor_for, verify_server};

/// The predecessor's end, listening before the successor exists.
pub struct ChannelServer {
    name: String,
    pipe: NamedPipeServer,
}

/// Creates the channel under a fresh random name, as the only instance
/// it will ever have, readable and writable by this user at this
/// integrity level alone.
pub fn open_channel() -> io::Result<ChannelServer> {
    let identity = current_identity()?;
    let nonce = || std::hash::RandomState::new().hash_one(());
    let name = format!(
        r"\\.\pipe\felis-upgrade.{}.{:016x}{:016x}",
        identity.sid,
        nonce(),
        nonce()
    );
    let security = security_descriptor_for(&identity)?;
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: security.0,
        bInheritHandle: FALSE,
    };
    // SAFETY: `attrs` is fully initialized and its descriptor outlives
    // the call, which reads both synchronously and keeps neither.
    let pipe = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .max_instances(1)
            .create_with_security_attributes_raw(&name, (&raw mut attrs).cast::<c_void>())
    }?;
    Ok(ChannelServer { name, pipe })
}

impl ChannelServer {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Waits for the one client and admits it only if it is `pid`, the
    /// process the caller started and still holds a handle to, so the
    /// pid cannot name anyone else.
    pub async fn accept(self, pid: u32) -> io::Result<NamedPipeServer> {
        self.pipe.connect().await?;
        let mut client: u32 = 0;
        // SAFETY: the live server end of a connected pipe and a live
        // out-pointer.
        let ok = unsafe {
            GetNamedPipeClientProcessId(self.pipe.as_raw_handle() as HANDLE, &raw mut client)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if client != pid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the upgrade channel was opened by pid {client}, not the successor {pid}"),
            ));
        }
        Ok(self.pipe)
    }
}

/// The successor's end. The server must run as this user at this
/// integrity level, the check every client dial makes (REQ-106).
pub fn connect_channel(name: &str) -> io::Result<(NamedPipeClient, u32)> {
    let client = ClientOptions::new().open(name)?;
    verify_server(&client)?;
    let mut server: u32 = 0;
    // SAFETY: the live client end of a connected pipe and a live
    // out-pointer.
    let ok =
        unsafe { GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &raw mut server) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((client, server))
}

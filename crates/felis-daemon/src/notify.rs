//! Readiness notification for a daemon the systemd user manager
//! started (`docs/explanation/architecture/overview.md`
//! "Where an auto-spawned daemon lands").

use std::{
    ffi::OsStr,
    io,
    os::unix::{ffi::OsStrExt, net::UnixDatagram},
};

/// Sends `READY=1` to the manager's endpoint; an absent or empty
/// address is the ordinary fork, which has no manager to answer.
pub fn notify_ready(address: Option<&OsStr>) -> io::Result<()> {
    let Some(address) = address.filter(|address| !address.is_empty()) else {
        return Ok(());
    };
    let socket = UnixDatagram::unbound()?;
    let bytes = address.as_bytes();
    if let Some(abstract_name) = bytes.strip_prefix(b"@") {
        // `sd_notify(3)` spells an abstract name with a leading `@` in
        // the variable and a leading NUL on the wire; `connect` on a
        // path would address a filesystem socket named `@…` instead.
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(abstract_name)?;
        socket.connect_addr(&addr)?;
    } else {
        socket.connect(OsStr::from_bytes(bytes))?;
    }
    socket.send(b"READY=1\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::ffi::OsString;

    #[test]
    fn ready_reaches_a_listening_manager_endpoint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("notify.sock");
        let manager = UnixDatagram::bind(&path).unwrap();

        notify_ready(Some(path.as_os_str())).expect("send READY=1");

        let mut buf = [0u8; 64];
        let read = manager.recv(&mut buf).unwrap();
        assert_eq!(&buf[..read], b"READY=1\n");
    }

    #[test]
    fn an_unset_address_sends_nothing() {
        notify_ready(None).expect("no manager, no notification");
        notify_ready(Some(OsString::new().as_os_str())).expect("an empty address is absence");
    }

    #[test]
    fn a_dead_endpoint_is_an_error_the_caller_can_fail_on() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("absent.sock");
        notify_ready(Some(path.as_os_str())).expect_err("nothing is listening on that path");
    }
}

//! Resolves target daemon socket: `--socket`, `FELIS_SOCKET`, then platform default.
//!
//! Daemons must not read `FELIS_SOCKET`, or an auto-spawned child inherits the
//! parent socket and tries to bind an address already in use.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Written by `felis_daemon::apply_env_policy`, which reserves it so no
/// caller can supply it.
pub const SOCKET_ENV: &str = "FELIS_SOCKET";

/// Which of the three sources named the socket. Path equality is no
/// substitute: an explicit or stamped path can name the platform
/// default, and only the default was chosen by felis rather than by the
/// caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketSource {
    /// `--socket`.
    Explicit,
    /// A non-empty `FELIS_SOCKET`, which every shell inside a felis
    /// session carries.
    Stamped,
    /// The platform default.
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSocket {
    pub path: PathBuf,
    pub source: SocketSource,
}

pub fn resolve_local_socket(override_path: Option<&Path>) -> std::io::Result<PathBuf> {
    resolve_local_socket_source(override_path).map(|resolved| resolved.path)
}

pub fn resolve_local_socket_source(override_path: Option<&Path>) -> std::io::Result<LocalSocket> {
    resolve_local_socket_from(override_path, std::env::var_os(SOCKET_ENV))
}

pub fn resolve_local_socket_from(
    override_path: Option<&Path>,
    stamped: Option<OsString>,
) -> std::io::Result<LocalSocket> {
    if let Some(path) = override_path {
        return Ok(LocalSocket {
            path: anchor(path.to_path_buf()),
            source: SocketSource::Explicit,
        });
    }
    // An empty stamp is absence: `FELIS_SOCKET=` in a login file would
    // otherwise fail every dial on the empty path.
    if let Some(stamp) = stamped.filter(|value| !value.is_empty()) {
        return Ok(LocalSocket {
            path: anchor(PathBuf::from(stamp)),
            source: SocketSource::Stamped,
        });
    }
    felis_transport::socket::default_socket_path().map(|path| LocalSocket {
        path,
        source: SocketSource::Default,
    })
}

/// A relative path is anchored once, here: the dial, the spawn and the
/// unit name a systemd hand-off derives must all name the same socket,
/// and a process felis hands the path to has its own working directory.
#[cfg(unix)]
fn anchor(path: PathBuf) -> PathBuf {
    felis_transport::socket::anchor_to_current_dir(path)
}

#[cfg(windows)]
const fn anchor(path: PathBuf) -> PathBuf {
    felis_transport::socket::anchor_to_current_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_socket_outranks_the_stamp() {
        let flag = PathBuf::from("/tmp/felis-flag.sock");
        let resolved = resolve_local_socket_from(
            Some(flag.as_path()),
            Some(OsString::from("/tmp/felis-stamp.sock")),
        )
        .unwrap();
        assert_eq!(resolved.path, flag);
        assert_eq!(resolved.source, SocketSource::Explicit);
    }

    #[test]
    fn the_stamp_outranks_the_platform_default() {
        let resolved =
            resolve_local_socket_from(None, Some(OsString::from("/tmp/felis-stamp.sock"))).unwrap();
        assert_eq!(resolved.path, PathBuf::from("/tmp/felis-stamp.sock"));
        assert_eq!(resolved.source, SocketSource::Stamped);
    }

    #[test]
    fn no_flag_and_no_stamp_falls_back_to_the_platform_default() {
        let resolved = resolve_local_socket_from(None, None)
            .expect("the platform default resolves in the dev shell");
        assert_eq!(resolved.source, SocketSource::Default);
        let resolved = resolved.path;
        #[cfg(unix)]
        assert!(
            resolved
                .file_name()
                .is_some_and(|name| name == "daemon.sock"),
            "expected the default to end with daemon.sock; got {resolved:?}",
        );
        #[cfg(windows)]
        {
            let shown = resolved.to_string_lossy();
            assert!(
                shown.starts_with(r"\\.\pipe\felis.") && shown.ends_with(".daemon"),
                "expected the default to be the felis daemon pipe; got {resolved:?}",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_flag_is_anchored_to_the_current_directory() {
        let resolved =
            resolve_local_socket_from(Some(Path::new("felis-relative.sock")), None).unwrap();
        assert_eq!(
            resolved.path,
            std::env::current_dir().unwrap().join("felis-relative.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_stamp_is_anchored_too() {
        let resolved =
            resolve_local_socket_from(None, Some(OsString::from("felis-stamp-relative.sock")))
                .unwrap();
        assert!(resolved.path.is_absolute(), "got {resolved:?}");
    }

    #[test]
    fn an_empty_stamp_is_ignored() {
        let empty = resolve_local_socket_from(None, Some(OsString::new()))
            .expect("the platform default resolves in the dev shell");
        let unset = resolve_local_socket_from(None, None).unwrap();
        assert_eq!(empty, unset);
        assert_eq!(empty.source, SocketSource::Default);
    }
}

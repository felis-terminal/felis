//! Daemon socket-path resolution: the uid alone, so every process of
//! one user lands on one socket (`docs/reference/cli.md` "Carrier and
//! connection lifetime"). A pathname socket only, never the Linux abstract
//! namespace, and the directory and socket modes are established at
//! bind time (`crate::local`).

#[cfg(unix)]
use std::env;
use std::path::PathBuf;

pub const SOCKET_FILENAME: &str = "daemon.sock";

/// The default endpoint and the directory holding it, derived from the
/// uid: `/tmp/felis.<uid>/daemon.sock`. `/tmp` has the lifetime the
/// daemon needs (the boot) on every Unix felis targets, which a login
/// session's runtime directory does not
/// (`docs/explanation/architecture/ipc.md`).
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPath {
    pub dir: PathBuf,
    pub socket: PathBuf,
}

#[cfg(unix)]
impl SocketPath {
    #[must_use]
    pub fn resolve(uid: u32) -> Self {
        let dir = PathBuf::from(format!("/tmp/felis.{uid}"));
        let socket = dir.join(SOCKET_FILENAME);
        Self { dir, socket }
    }

    #[must_use]
    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

/// Anchors a caller-supplied socket path, so a process felis hands it to
/// cannot resolve it against a different working directory.
///
/// Lexical, never canonicalized: the name the user typed is the one the
/// daemon stamps as `FELIS_SOCKET`.
#[cfg(unix)]
#[must_use]
pub fn anchor_to_current_dir(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path,
    }
}

/// The local address is a pipe name rather than a filesystem path, so
/// there is nothing to anchor.
#[cfg(windows)]
#[must_use]
pub const fn anchor_to_current_dir(path: PathBuf) -> PathBuf {
    path
}

/// The daemon's default local address as the opaque `PathBuf` the
/// binaries carry; it becomes an [`Endpoint`](crate::Endpoint) only at
/// the connect / serve boundary (`docs/reference/ipc.md`).
// The `Result` is Windows's, where the pipe name queries the user's SID.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
pub fn default_socket_path() -> std::io::Result<PathBuf> {
    Ok(SocketPath::resolve(rustix::process::geteuid().as_raw()).socket)
}

#[cfg(windows)]
pub fn default_socket_path() -> std::io::Result<PathBuf> {
    crate::local::default_pipe_name().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::Path;

    #[cfg(unix)]
    #[test]
    fn the_endpoint_is_derived_from_the_uid_alone() {
        let resolved = SocketPath::resolve(1234);
        assert_eq!(resolved.dir(), Path::new("/tmp/felis.1234"));
        assert_eq!(resolved.socket(), Path::new("/tmp/felis.1234/daemon.sock"));
    }

    /// Whatever this test process inherited from its environment, the
    /// default endpoint is the uid's `/tmp` directory.
    #[cfg(unix)]
    #[test]
    fn the_default_endpoint_ignores_the_environment_this_process_carries() {
        let own = rustix::process::geteuid().as_raw();
        assert_eq!(
            default_socket_path().unwrap(),
            PathBuf::from(format!("/tmp/felis.{own}/daemon.sock"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_path_is_anchored_to_the_current_directory() {
        let anchored = anchor_to_current_dir(PathBuf::from("felis-relative.sock"));
        assert_eq!(
            anchored,
            env::current_dir().unwrap().join("felis-relative.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_absolute_path_is_left_as_typed() {
        let typed = PathBuf::from("/tmp/felis-typed/../daemon.sock");
        assert_eq!(anchor_to_current_dir(typed.clone()), typed);
    }

    #[test]
    fn socket_filename_is_stable() {
        // The CLI surface promises a single socket per UID.
        assert_eq!(SOCKET_FILENAME, "daemon.sock");
    }
}

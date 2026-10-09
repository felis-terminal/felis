//! Platform-neutral local IPC carrier facade: the single place the
//! OS-specific stream type is named, imported by the connector, the
//! client app state and the daemon's accept loop
//! (`docs/reference/ipc.md` "Local carrier",
//! `docs/explanation/architecture/overview.md` "Workspace: the crate-boundary decision record").

use std::io;

use thiserror::Error;

use crate::peer::PeerError;

#[cfg(unix)]
use crate::unix as backend;
#[cfg(windows)]
use crate::windows as backend;

#[derive(Debug, Error)]
pub enum BindError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// The socket's parent, which the daemon judges from the descriptor
    /// it locked and never tightens (REQ-107).
    #[cfg(unix)]
    #[error(
        "{path} is not usable as a socket directory ({reason}); its owner can remove or chown it, \
         or pass --socket <dir>/daemon.sock naming a 0700 directory you own",
        path = path.display()
    )]
    Parent {
        path: std::path::PathBuf,
        reason: String,
    },
    /// What sits at the socket name itself, when the startup probe's
    /// classification licenses no unlink (REQ-009c, REQ-009d).
    #[cfg(unix)]
    #[error("{path} cannot be replaced by this daemon's socket ({reason})", path = path.display())]
    Endpoint {
        path: std::path::PathBuf,
        reason: String,
    },
}

#[derive(Debug, Error)]
pub enum AcceptError {
    #[error("accept: {0}")]
    Io(#[from] io::Error),
    #[error("peer rejected: {0}")]
    Peer(#[from] PeerError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    #[cfg(unix)]
    path: std::path::PathBuf,
    #[cfg(windows)]
    pipe_name: String,
}

#[cfg(unix)]
impl Endpoint {
    pub fn unix(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(windows)]
impl Endpoint {
    pub fn pipe(name: impl Into<String>) -> Self {
        Self {
            pipe_name: name.into(),
        }
    }

    #[must_use]
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(unix)]
        {
            write!(f, "{}", self.path.display())
        }
        #[cfg(windows)]
        {
            write!(f, "{}", self.pipe_name)
        }
    }
}

// On Windows the binaries carry the pipe name inside a `PathBuf` (see
// `crate::socket::default_socket_path`).
impl From<std::path::PathBuf> for Endpoint {
    fn from(path: std::path::PathBuf) -> Self {
        #[cfg(unix)]
        {
            Self::unix(path)
        }
        #[cfg(windows)]
        {
            Self::pipe(path.to_string_lossy().into_owned())
        }
    }
}
impl From<&std::path::Path> for Endpoint {
    fn from(path: &std::path::Path) -> Self {
        Self::from(path.to_path_buf())
    }
}
impl From<&std::path::PathBuf> for Endpoint {
    fn from(path: &std::path::PathBuf) -> Self {
        Self::from(path.clone())
    }
}

#[cfg(windows)]
pub use crate::windows::default_pipe_name;

// The Windows named-pipe types have no owned-half split, hence
// `tokio::io::split` there.
#[cfg(unix)]
pub type ReadHalf = tokio::net::unix::OwnedReadHalf;
#[cfg(unix)]
pub type WriteHalf = tokio::net::unix::OwnedWriteHalf;

#[cfg(windows)]
pub type ReadHalf = tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>;
#[cfg(windows)]
pub type WriteHalf = tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>;

#[cfg(unix)]
pub type ServerStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type ServerStream = tokio::net::windows::named_pipe::NamedPipeServer;

#[cfg(unix)]
pub type ServerReadHalf = tokio::net::unix::OwnedReadHalf;
#[cfg(unix)]
pub type ServerWriteHalf = tokio::net::unix::OwnedWriteHalf;

#[cfg(windows)]
pub type ServerReadHalf = tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeServer>;
#[cfg(windows)]
pub type ServerWriteHalf = tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeServer>;

#[must_use]
pub fn server_split(stream: ServerStream) -> (ServerReadHalf, ServerWriteHalf) {
    #[cfg(unix)]
    {
        stream.into_split()
    }
    #[cfg(windows)]
    {
        tokio::io::split(stream)
    }
}

pub async fn connect(endpoint: impl Into<Endpoint>) -> io::Result<(ReadHalf, WriteHalf)> {
    #[cfg(unix)]
    {
        connect_expecting(endpoint, rustix::process::geteuid().as_raw()).await
    }
    #[cfg(windows)]
    {
        let endpoint = endpoint.into();
        let client = backend::connect(endpoint.pipe_name()).await?;
        Ok(tokio::io::split(client))
    }
}

/// The dialer half of REQ-106: the listener's uid is checked before the
/// first byte of the preface, so a pathname connect is safe whatever a
/// cleaner or another uid did to the path in between. `expected_uid` is
/// the caller's own everywhere but the tests that need a mismatch.
#[cfg(unix)]
pub async fn connect_expecting(
    endpoint: impl Into<Endpoint>,
    expected_uid: u32,
) -> io::Result<(ReadHalf, WriteHalf)> {
    let endpoint = endpoint.into();
    let stream = backend::connect(endpoint.path()).await?;
    crate::peer::verify_peer_uid(&stream, expected_uid).map_err(|err| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{endpoint}: {err}"),
        )
    })?;
    Ok(stream.into_split())
}

pub struct Listener(backend::Listener);

impl Listener {
    /// The admitted peer identity is the process's own (effective UID on
    /// Unix, user SID on Windows).
    pub fn bind(endpoint: &Endpoint) -> Result<Self, BindError> {
        backend::Listener::bind(endpoint).map(Self)
    }

    /// Wraps the listening socket an in-place upgrade's `execve` carried
    /// over, without the probe-unlink-bind `bind` runs.
    #[cfg(unix)]
    pub fn adopt(fd: std::os::fd::OwnedFd, endpoint: &Endpoint) -> io::Result<Self> {
        backend::Listener::adopt(fd, endpoint).map(Self)
    }

    /// The listening socket, which an in-place upgrade carries across `execve`.
    #[cfg(unix)]
    #[must_use]
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }

    /// The peer-identity check runs before this returns; a mismatch
    /// closes the connection and surfaces [`AcceptError::Peer`].
    pub async fn accept(&self) -> Result<ServerStream, AcceptError> {
        self.0.accept().await
    }

    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        self.0.endpoint()
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A socket parent must be a `0700` directory this uid owns
    /// (REQ-107), and `TempDir` follows the process umask.
    #[cfg(unix)]
    fn private_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        tmp
    }

    /// Pins the facade's aliases and splits, not the backend socket.
    #[tokio::test]
    async fn bytes_round_trip_through_the_facade_halves() {
        let tmp = private_dir();
        let path = tmp.path().join("facade.sock");
        let listener = Listener::bind(&Endpoint::unix(path.clone())).unwrap();

        let server_task = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (mut rx, mut tx) = server_split(stream);
            let mut buf = [0u8; 4];
            rx.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
            tx.write_all(b"pong").await.unwrap();
        });

        let (mut rx, mut tx) = connect(path.as_path()).await.unwrap();
        tx.write_all(b"ping").await.unwrap();
        let mut reply = [0u8; 4];
        rx.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"pong");

        server_task.await.unwrap();
    }

    /// The adopted socket is the same listening socket, so a dial that
    /// queued while nobody accepted is served by the adopter.
    #[tokio::test]
    async fn an_adopted_listener_serves_a_dial_queued_before_adoption() {
        let tmp = private_dir();
        let path = tmp.path().join("adopt.sock");
        let endpoint = Endpoint::unix(path.clone());
        let original = Listener::bind(&endpoint).unwrap();
        let carried = original.as_fd().try_clone_to_owned().unwrap();
        drop(original);

        let (mut rx, mut tx) = connect(path.as_path()).await.unwrap();
        tx.write_all(b"ping").await.unwrap();

        let adopted = Listener::adopt(carried, &endpoint).unwrap();
        let stream = adopted.accept().await.unwrap();
        let (mut srx, mut stx) = server_split(stream);
        let mut buf = [0u8; 4];
        srx.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        stx.write_all(b"pong").await.unwrap();
        rx.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn adoption_refuses_a_socket_bound_to_another_endpoint() {
        let tmp = private_dir();
        let listener = Listener::bind(&Endpoint::unix(tmp.path().join("a.sock"))).unwrap();
        let carried = listener.as_fd().try_clone_to_owned().unwrap();
        let other = Endpoint::unix(tmp.path().join("b.sock"));
        assert!(Listener::adopt(carried, &other).is_err());
    }

    #[tokio::test]
    async fn a_bound_listener_reports_the_endpoint_it_was_given() {
        let tmp = private_dir();
        let path = tmp.path().join("endpoint.sock");
        let listener = Listener::bind(&Endpoint::unix(path.clone())).unwrap();

        assert_eq!(listener.endpoint(), Endpoint::unix(path.clone()));
        assert_eq!(listener.endpoint().path(), path);
        assert_eq!(listener.endpoint().to_string(), path.display().to_string());
    }

    #[tokio::test]
    async fn facade_bind_applies_the_peer_isolation_permissions() {
        let tmp = private_dir();
        let dir = tmp.path().join("runtime");
        let path = dir.join("daemon.sock");
        let listener = Listener::bind(&Endpoint::unix(path)).unwrap();

        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::metadata(listener.endpoint().path())
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
    }

    /// REQ-106's dialer half: a listener of another uid ends the dial
    /// with the mismatch, and nothing of the preface was written. The
    /// expectation is injected because a test has one account.
    #[tokio::test]
    async fn a_dial_of_a_listener_of_another_uid_ends_before_the_preface() {
        let tmp = private_dir();
        let path = tmp.path().join("foreign.sock");
        let listener = Listener::bind(&Endpoint::unix(path.clone())).unwrap();
        let own = rustix::process::geteuid().as_raw();
        let bogus = if own == 0 { 65534 } else { 0 };

        let server_task = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (mut rx, _tx) = server_split(stream);
            let mut buf = [0u8; 1];
            rx.read(&mut buf).await.unwrap()
        });

        let err = connect_expecting(path.as_path(), bogus)
            .await
            .expect_err("a foreign listener is not one to speak to");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("does not match"), "{err}");
        assert_eq!(
            server_task.await.unwrap(),
            0,
            "the listener must see no application byte"
        );
    }

    /// Only a bind judges a parent (REQ-107). A dial of a socket
    /// directly under the shared `/tmp` gets the connect's own answer,
    /// and creates nothing: a client that judged would refuse every
    /// endpoint a user is entitled to name.
    #[tokio::test]
    async fn a_dial_judges_no_parent_and_creates_nothing() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::path::PathBuf::from(format!(
            "/tmp/felis-dial-{}-{unique}.sock",
            std::process::id()
        ));
        assert!(!path.exists(), "the name must be one nothing bound");

        let err = connect(path.as_path())
            .await
            .expect_err("nothing listens at a name nothing bound");

        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert!(!path.exists(), "a dial creates no endpoint");
    }

    /// The mismatch half needs a listener bound to a foreign UID, which
    /// only the backend tests can express.
    #[tokio::test]
    async fn accept_admits_a_same_uid_peer() {
        let tmp = private_dir();
        let path = tmp.path().join("peer.sock");
        let listener = Listener::bind(&Endpoint::unix(path.clone())).unwrap();

        let server_task = tokio::spawn(async move { listener.accept().await.is_ok() });
        let _client = connect(path.as_path()).await.unwrap();
        assert!(server_task.await.unwrap(), "own UID must be admitted");
    }
}

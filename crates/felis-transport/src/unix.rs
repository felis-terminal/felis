//! Unix-socket backend for the [`crate::local`] carrier facade: a
//! pathname socket in a `0700` directory the daemon locks for its
//! startup window, `0600` socket, peer UID checked on both sides
//! (`docs/explanation/security-model.md` "Daemon IPC").

use std::{
    fs, io,
    os::fd::AsFd as _,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _},
    path::{Path, PathBuf},
};

use rustix::fs::{FlockOperation, Mode, OFlags};
use tokio::net::{UnixListener, UnixStream};
use tracing::warn;

use crate::local::{AcceptError, BindError, Endpoint};
use crate::peer::{verify_peer_uid, verify_peer_uid_of};

/// `umask` is process-global, so concurrent `bind` calls (and the
/// `bind_restores_the_process_umask` test which probes it) must not
/// interleave their `umask` save/restore windows.
static UMASK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug)]
pub(crate) struct Listener {
    inner: UnixListener,
    path: PathBuf,
    expected_uid: u32,
}

impl Listener {
    pub(crate) fn bind(endpoint: &Endpoint) -> Result<Self, BindError> {
        Self::bind_with_uid(endpoint, geteuid())
    }

    fn bind_with_uid(endpoint: &Endpoint, expected_uid: u32) -> Result<Self, BindError> {
        Self::bind_hooked(endpoint, expected_uid, || {})
    }

    /// `expected_uid` is the peer identity this listener admits; the
    /// directory is judged against the euid, since felis has to own it.
    /// `between` runs between the judge and the bind: the seam the
    /// post-bind identity check is tested through.
    fn bind_hooked(
        endpoint: &Endpoint,
        expected_uid: u32,
        between: impl FnOnce(),
    ) -> Result<Self, BindError> {
        let path = endpoint.path().to_path_buf();
        let parent = socket_parent(&path);
        let dir = SocketDir::lock(&parent, geteuid())?;
        clear_endpoint(&path, expected_uid)?;
        between();
        let inner = bind_under_umask(&path)?;
        dir.still_names(&parent)?;
        drop(dir);
        Ok(Self {
            inner,
            path,
            expected_uid,
        })
    }

    pub(crate) async fn accept(&self) -> Result<UnixStream, AcceptError> {
        let (stream, _addr) = self.inner.accept().await?;
        match verify_peer_uid(&stream, self.expected_uid) {
            Ok(()) => Ok(stream),
            Err(err) => {
                warn!(?err, "rejecting connection with mismatched peer UID");
                Err(AcceptError::Peer(err))
            }
        }
    }

    #[cfg(test)]
    #[must_use]
    fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub(crate) fn endpoint(&self) -> Endpoint {
        Endpoint::unix(self.path.clone())
    }
}

pub(crate) async fn connect(path: impl AsRef<Path>) -> io::Result<UnixStream> {
    UnixStream::connect(path).await
}

fn socket_parent(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The socket directory, created if absent and held under an exclusive
/// `flock` for the probe-unlink-bind window only (REQ-009d). The lock
/// is the directory itself because a lock file beside the socket is
/// what every tmp cleaner ages, and an aged inode is how two starters
/// end up holding different locks.
struct SocketDir {
    _locked: fs::File,
    dev: u64,
    ino: u64,
}

impl SocketDir {
    fn lock(dir: &Path, expected_uid: u32) -> Result<Self, BindError> {
        create_dir_0700(dir)?;
        let opened = rustix::fs::open(
            dir,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|err| parent_failure(dir, &io::Error::from(err)))?;
        let locked = fs::File::from(opened);
        rustix::fs::flock(&locked, FlockOperation::LockExclusive).map_err(io::Error::from)?;
        // From the locked descriptor, never the path: what is judged is
        // then what the bind writes into.
        let facts = locked.metadata()?;
        judge_socket_dir(
            dir,
            expected_uid,
            DirFacts {
                uid: facts.uid(),
                mode: facts.mode() & 0o777,
            },
        )?;
        Ok(Self {
            _locked: locked,
            dev: facts.dev(),
            ino: facts.ino(),
        })
    }

    /// The parent could have been renamed away and replaced between the
    /// judge and the bind; a daemon serving inside the replacement would
    /// hand its sessions whatever `<socket>.agent` that uid points at.
    fn still_names(&self, dir: &Path) -> Result<(), BindError> {
        let now = fs::symlink_metadata(dir).map_err(|err| parent_failure(dir, &err))?;
        if now.dev() == self.dev && now.ino() == self.ino {
            return Ok(());
        }
        Err(BindError::Parent {
            path: dir.to_path_buf(),
            reason: "it was replaced while the daemon was binding its socket".to_owned(),
        })
    }
}

/// `mkdir` is filtered by the process umask, so an inherited `0777`
/// would create a `0000` directory the judge then refuses. Only the
/// parent itself is created: an explicit `--socket` whose grandparent
/// is missing names a place the user has not made, and materializing a
/// tree for it would bury the typo.
fn create_dir_0700(dir: &Path) -> Result<(), BindError> {
    match with_umask(Mode::from_raw_mode(0o077), || fs::create_dir(dir)) {
        // Something already at the path is the judge's answer to give.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(parent_failure(dir, &err)),
        Ok(()) => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirFacts {
    uid: u32,
    mode: u32,
}

/// tmux's `check_dir`, and what closes the squat on a predictable name
/// in a shared `/tmp`: setgid and sticky bits are ignored, the access
/// bits are not. Never tightened: a parent felis did not create is the
/// user's, and a `chmod` on it would hide whatever set it loose.
fn judge_socket_dir(dir: &Path, expected_uid: u32, facts: DirFacts) -> Result<(), BindError> {
    let refuse = |reason: &str| {
        Err(BindError::Parent {
            path: dir.to_path_buf(),
            reason: reason.to_owned(),
        })
    };
    if facts.uid != expected_uid {
        return refuse(&format!("uid {} owns it, not you", facts.uid));
    }
    if facts.mode != 0o700 {
        return refuse(&format!("its mode is {:04o}, not 0700", facts.mode));
    }
    Ok(())
}

/// Every failure to create or open the parent is the parent's, so it
/// is reported as one: `BindError::Io` names no path and no recovery,
/// and the errno alone names none of the squats the judge exists for.
/// `O_DIRECTORY | O_NOFOLLOW` answers `ENOTDIR` for a symlink on Linux
/// and `ELOOP` elsewhere, so what is there is read off the path.
fn parent_failure(dir: &Path, err: &io::Error) -> BindError {
    let reason = match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => "it is a symlink".to_owned(),
        Ok(meta) if !meta.is_dir() => "it is not a directory".to_owned(),
        Ok(meta) => format!(
            "uid {uid} owns it and its mode is {mode:04o} ({err})",
            uid = meta.uid(),
            mode = meta.mode() & 0o777,
        ),
        Err(_) => format!("{err}"),
    };
    BindError::Parent {
        path: dir.to_path_buf(),
        reason,
    }
}

/// REQ-009c, applied to the startup probe too: only absence licenses the
/// unlink. A directory or a symlink at the endpoint is never removed:
/// felis removes no directory, and a symlink there is the user's, who
/// may have pointed it at a socket another daemon serves.
fn clear_endpoint(path: &Path, expected_uid: u32) -> Result<(), BindError> {
    let file_type = match fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(BindError::Io(err)),
    };
    let refuse = |reason: &str| {
        Err(BindError::Endpoint {
            path: path.to_path_buf(),
            reason: reason.to_owned(),
        })
    };
    if file_type.is_symlink() {
        return refuse("it is a symlink");
    }
    if file_type.is_dir() {
        return refuse("it is a directory");
    }
    if file_type.is_socket() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(stream) => {
                return match verify_peer_uid_of(stream.as_fd(), expected_uid) {
                    Ok(()) => Err(BindError::Io(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another felis-daemon is serving {}", path.display()),
                    ))),
                    Err(err) => refuse(&err.to_string()),
                };
            }
            Err(err) if crate::preface::connect_error_is_absent(&err) => {}
            Err(err) => return Err(BindError::Io(err)),
        }
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(BindError::Io(err)),
    }
}

/// `0177` creates the socket at `0600` directly: a `chmod` afterwards
/// would leave a window with looser bits, and follows the pathname
/// rather than the inode it just bound.
fn bind_under_umask(path: &Path) -> Result<UnixListener, BindError> {
    with_umask(Mode::from_raw_mode(0o177), || UnixListener::bind(path)).map_err(BindError::Io)
}

fn with_umask<T>(mask: Mode, op: impl FnOnce() -> T) -> T {
    #[allow(clippy::unwrap_used)]
    let guard = UMASK_LOCK.lock().unwrap();
    let prior = rustix::process::umask(mask);
    let out = op();
    rustix::process::umask(prior);
    drop(guard);
    out
}

fn geteuid() -> u32 {
    rustix::process::geteuid().as_raw()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::peer::PeerError;

    fn swap_umask(mask: Mode) -> Mode {
        #[allow(clippy::unwrap_used)]
        let _guard = UMASK_LOCK.lock().unwrap();
        rustix::process::umask(mask)
    }

    /// A socket parent must be a `0700` directory this uid owns
    /// (REQ-107), and `TempDir` follows the process umask.
    #[cfg(unix)]
    fn private_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        tmp
    }

    fn parent_reason(err: BindError) -> String {
        match err {
            BindError::Parent { reason, path } => format!("{}: {reason}", path.display()),
            other => panic!("expected a parent refusal, got {other:?}"),
        }
    }

    fn endpoint_reason(err: BindError) -> String {
        match err {
            BindError::Endpoint { reason, path } => format!("{}: {reason}", path.display()),
            other => panic!("expected an endpoint refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_socket_has_mode_0600_and_directory_has_mode_0700() {
        let tmp = private_dir();
        let dir = tmp.path().join("felis");
        let path = dir.join("daemon.sock");
        let server = Listener::bind(&Endpoint::unix(path)).unwrap();

        let dir_mode = fs::metadata(&dir).unwrap().mode() & 0o777;
        let sock_mode = fs::metadata(server.path()).unwrap().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "parent dir must be 0700");
        assert_eq!(sock_mode, 0o600, "socket must be 0600, with no chmod");
    }

    /// A daemon inheriting a permissive umask must still create the
    /// directory the judge accepts, and a restrictive one must not make
    /// it `0000`.
    #[tokio::test]
    async fn the_created_directory_is_0700_under_any_inherited_umask() {
        for inherited in [0o077, 0o777, 0o022] {
            let tmp = private_dir();
            let dir = tmp.path().join("felis");
            let restore = swap_umask(Mode::from_raw_mode(inherited));
            let bound = Listener::bind(&Endpoint::unix(dir.join("daemon.sock")));
            swap_umask(restore);
            bound.unwrap();
            assert_eq!(
                fs::metadata(&dir).unwrap().mode() & 0o777,
                0o700,
                "umask {inherited:04o}"
            );
        }
    }

    #[tokio::test]
    async fn round_trip_through_a_real_unix_socket() {
        let tmp = private_dir();
        let path = tmp.path().join("rt.sock");
        let server = Listener::bind(&Endpoint::unix(path)).unwrap();

        let server_path = server.path().to_path_buf();
        let server_task = tokio::spawn(async move {
            let mut stream = server.accept().await.unwrap();
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });

        let mut client = connect(&server_path).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"pong");

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn stale_socket_file_is_replaced_on_bind() {
        let tmp = private_dir();
        let path = tmp.path().join("stale.sock");
        fs::write(&path, b"stale").unwrap();
        let server = Listener::bind(&Endpoint::unix(path)).unwrap();
        assert!(
            fs::metadata(server.path()).unwrap().file_type().is_socket(),
            "the stale regular file must be replaced by a socket",
        );
    }

    /// REQ-009d: a daemon that exits leaves its socket for the next
    /// starter's probe, so an old daemon dying can never unlink a newer
    /// one's socket.
    #[tokio::test]
    async fn drop_leaves_the_socket_file_and_the_next_bind_replaces_it() {
        let tmp = private_dir();
        let path = tmp.path().join("left.sock");
        let server = Listener::bind(&Endpoint::unix(path.clone())).unwrap();
        drop(server);
        assert!(
            fs::symlink_metadata(&path).unwrap().file_type().is_socket(),
            "exit must leave the socket in place"
        );

        let second = Listener::bind(&Endpoint::unix(path.clone())).unwrap();
        let mut client = connect(&path).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut stream = second.accept().await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
    }

    #[tokio::test]
    async fn no_lock_file_is_created_beside_the_socket() {
        let tmp = private_dir();
        let dir = tmp.path().join("felis");
        let _server = Listener::bind(&Endpoint::unix(dir.join("daemon.sock"))).unwrap();

        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("daemon.sock")]);
    }

    #[tokio::test]
    async fn stale_socket_from_a_dead_daemon_is_rebound() {
        let tmp = private_dir();
        let path = tmp.path().join("dead.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(
            path.exists(),
            "a dropped std listener leaves the socket file"
        );

        let server = Listener::bind(&Endpoint::unix(path.clone())).unwrap();
        let server_path = server.path().to_path_buf();
        let server_task = tokio::spawn(async move {
            let mut stream = server.accept().await.unwrap();
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
        });
        let mut client = connect(&server_path).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn live_socket_bind_is_refused_not_stolen() {
        let tmp = private_dir();
        let path = tmp.path().join("live.sock");
        let _first = Listener::bind(&Endpoint::unix(path.clone())).unwrap();

        match Listener::bind(&Endpoint::unix(path)) {
            Ok(_) => panic!("second bind on a live socket must fail, not steal the address"),
            Err(BindError::Io(e)) => {
                assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
                assert!(
                    e.to_string().contains("another felis-daemon is serving"),
                    "message must name the conflict, got: {e}"
                );
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    /// A listener of another uid at the endpoint is neither live nor
    /// absent: binding over it would serve a path that uid can still
    /// reach, so the start fails and the path survives. The uid comes in
    /// injected because a test has one account.
    #[tokio::test]
    async fn a_listener_of_another_uid_fails_the_start_and_keeps_the_path() {
        let tmp = private_dir();
        let path = tmp.path().join("foreign.sock");
        let _theirs = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let bogus = if geteuid() == 0 { 65534 } else { 0 };

        let reason = endpoint_reason(
            clear_endpoint(&path, bogus).expect_err("a foreign listener is not a path to take"),
        );
        assert!(reason.contains("does not match"), "{reason}");
        assert!(
            fs::symlink_metadata(&path).unwrap().file_type().is_socket(),
            "the foreign listener's socket must survive"
        );
        assert!(
            matches!(clear_endpoint(&path, geteuid()), Err(BindError::Io(err)) if err.kind() == io::ErrorKind::AddrInUse),
            "the same listener under our own uid is simply live"
        );
    }

    /// Only `ENOENT` and `ECONNREFUSED` prove nobody is listening; a
    /// connect that failed for any other reason leaves the inode in
    /// place, because it may still be a live daemon's.
    #[tokio::test]
    async fn a_socket_whose_connect_failed_for_another_reason_is_not_unlinked() {
        if geteuid() == 0 {
            return;
        }
        let tmp = private_dir();
        let path = tmp.path().join("eacces.sock");
        let _theirs = std::os::unix::net::UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // EACCES on connect is not universal across Unixes; skip where
        // this host does not raise it.
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            return;
        }

        let err = Listener::bind(&Endpoint::unix(path.clone()))
            .expect_err("an undialable socket is not a free path");
        assert!(
            matches!(&err, BindError::Io(io_err) if io_err.kind() == io::ErrorKind::PermissionDenied),
            "{err}"
        );
        assert!(
            fs::symlink_metadata(&path).unwrap().file_type().is_socket(),
            "the socket must survive a connect that proved nothing"
        );
    }

    #[tokio::test]
    async fn a_symlink_at_the_endpoint_fails_the_bind_untouched() {
        let tmp = private_dir();
        let served = tmp.path().join("served.sock");
        let _live = Listener::bind(&Endpoint::unix(served.clone())).unwrap();
        let link = tmp.path().join("link.sock");
        std::os::unix::fs::symlink(&served, &link).unwrap();

        let reason = endpoint_reason(
            Listener::bind(&Endpoint::unix(link.clone()))
                .expect_err("a symlink at the endpoint is the user's"),
        );
        assert!(reason.contains("it is a symlink"), "{reason}");
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert!(
            fs::symlink_metadata(&served)
                .unwrap()
                .file_type()
                .is_socket(),
            "the link's target must survive"
        );
    }

    #[tokio::test]
    async fn a_directory_at_the_endpoint_fails_the_bind_untouched() {
        let tmp = private_dir();
        let path = tmp.path().join("dir.sock");
        fs::create_dir(&path).unwrap();

        let reason = endpoint_reason(
            Listener::bind(&Endpoint::unix(path.clone())).expect_err("felis removes no directory"),
        );
        assert!(reason.contains("it is a directory"), "{reason}");
        assert!(path.is_dir(), "the directory must survive");
    }

    #[tokio::test]
    async fn a_parent_that_is_not_a_0700_directory_this_uid_owns_is_refused() {
        let tmp = private_dir();

        let loose = tmp.path().join("loose");
        fs::create_dir(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o750)).unwrap();
        let reason = parent_reason(
            Listener::bind(&Endpoint::unix(loose.join("daemon.sock")))
                .expect_err("a 0750 parent is refused"),
        );
        assert!(reason.contains("its mode is 0750"), "{reason}");
        assert_eq!(
            fs::metadata(&loose).unwrap().mode() & 0o777,
            0o750,
            "the parent must not be tightened"
        );

        let target = tmp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let linked = tmp.path().join("linked");
        std::os::unix::fs::symlink(&target, &linked).unwrap();
        let reason = parent_reason(
            Listener::bind(&Endpoint::unix(linked.join("daemon.sock")))
                .expect_err("a symlinked parent is refused"),
        );
        assert!(reason.contains("it is a symlink"), "{reason}");

        let file = tmp.path().join("file");
        fs::write(&file, b"x").unwrap();
        let reason = parent_reason(
            Listener::bind(&Endpoint::unix(file.join("daemon.sock")))
                .expect_err("a regular file is no parent"),
        );
        assert!(reason.contains("it is not a directory"), "{reason}");
    }

    /// An explicit `--socket` names a place the user makes; felis
    /// creates the parent and nothing above it, so a typo in the path
    /// is reported rather than materialized.
    #[tokio::test]
    async fn an_endpoint_whose_grandparent_is_missing_creates_nothing() {
        let tmp = private_dir();
        let missing = tmp.path().join("absent");
        let parent = missing.join("felis");

        let err = Listener::bind(&Endpoint::unix(parent.join("daemon.sock")))
            .expect_err("a missing grandparent is not created");

        let reason = parent_reason(err);
        assert!(reason.contains("felis"), "{reason}");
        assert!(!missing.exists(), "nothing above the parent may be created");
    }

    /// The squat the diagnostics exist for: a `0700` directory at the
    /// derived name that another uid owns cannot be opened, and an
    /// errno with no path is no help. No test can own a directory as
    /// another uid, so the failure is injected onto a real one.
    #[test]
    fn a_parent_that_cannot_be_opened_names_its_owner_and_mode() {
        let tmp = private_dir();
        let reason = parent_reason(parent_failure(
            tmp.path(),
            &io::Error::from(rustix::io::Errno::ACCESS),
        ));

        let own = geteuid();
        assert!(reason.contains(&format!("uid {own} owns it")), "{reason}");
        assert!(reason.contains("its mode is 0700"), "{reason}");
        assert!(reason.contains("Permission denied"), "{reason}");
    }

    /// The same failure without the injection: a parent under a
    /// directory this process cannot search. Root searches anything,
    /// and some Unixes answer `EACCES` where others do not, so the
    /// preflight decides whether there is anything to assert.
    #[tokio::test]
    async fn a_parent_that_cannot_be_reached_is_refused_naming_the_path() {
        let tmp = private_dir();
        let closed = tmp.path().join("closed");
        fs::create_dir(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        let parent = closed.join("felis");
        if fs::create_dir(&parent).is_ok() {
            fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }

        let err = Listener::bind(&Endpoint::unix(parent.join("daemon.sock")))
            .expect_err("an unreachable parent fails the start");

        let reason = parent_reason(err);
        assert!(reason.contains("Permission denied"), "{reason}");
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// The uid arm of the judge, which no test can produce on a real
    /// directory without a second account.
    #[test]
    fn a_directory_another_uid_owns_is_refused_by_the_judge() {
        let dir = Path::new("/tmp/felis.1234");
        let err = judge_socket_dir(
            dir,
            1234,
            DirFacts {
                uid: 4321,
                mode: 0o700,
            },
        )
        .expect_err("another owner is refused");
        let reason = parent_reason(err);
        assert!(reason.contains("uid 4321 owns it"), "{reason}");
        assert!(reason.contains("/tmp/felis.1234"), "{reason}");
        judge_socket_dir(
            dir,
            1234,
            DirFacts {
                uid: 1234,
                mode: 0o700,
            },
        )
        .expect("the uid's own 0700 directory is the one usable answer");
    }

    /// The `--socket` rule stated as the bind sees it: a socket
    /// directly under a shared directory has a parent that is not the
    /// user's own `0700` directory.
    #[test]
    fn a_socket_in_a_shared_directory_is_refused_by_the_judge() {
        let err = judge_socket_dir(
            Path::new("/tmp"),
            geteuid(),
            DirFacts {
                uid: 0,
                mode: 0o1777 & 0o777,
            },
        )
        .expect_err("/tmp is nobody's private socket directory");
        let reason = parent_reason(err);
        assert!(reason.contains("/tmp"), "{reason}");
    }

    /// Two `--socket` daemons may share one dedicated directory: the
    /// startup lock serializes their windows and nothing else.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_endpoints_in_one_parent_serve_concurrently() {
        let tmp = private_dir();
        let dir = tmp.path().join("shared");
        let one = Listener::bind(&Endpoint::unix(dir.join("one.sock"))).unwrap();
        let two = Listener::bind(&Endpoint::unix(dir.join("two.sock"))).unwrap();

        for (server, path) in [(one, dir.join("one.sock")), (two, dir.join("two.sock"))] {
            let mut client = connect(&path).await.unwrap();
            client.write_all(b"ping").await.unwrap();
            let mut stream = server.accept().await.unwrap();
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
        }
    }

    /// The lock is the startup window's alone: a live daemon holds
    /// nothing, or every later starter would wait on it instead of
    /// finding it live.
    #[tokio::test]
    async fn the_directory_lock_is_released_when_bind_returns() {
        let tmp = private_dir();
        let dir = tmp.path().join("felis");
        let observed = std::sync::Mutex::new(None);
        let _server =
            Listener::bind_hooked(&Endpoint::unix(dir.join("daemon.sock")), geteuid(), || {
                let fd = rustix::fs::open(&dir, OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
                    .unwrap();
                let held = rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive);
                *observed.lock().unwrap() = Some(held.is_err());
            })
            .unwrap();

        assert_eq!(
            observed.lock().unwrap().take(),
            Some(true),
            "a starter inside its window holds the directory lock"
        );
        let fd =
            rustix::fs::open(&dir, OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).unwrap();
        rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive)
            .expect("a serving daemon holds no startup lock");
    }

    /// A directory swapped between the judge and the bind would let
    /// another uid own `<socket>.agent`, which the peer check on the
    /// daemon socket cannot cover.
    #[tokio::test]
    async fn a_parent_replaced_before_the_bind_fails_the_start() {
        let tmp = private_dir();
        let dir = tmp.path().join("felis");
        let decoy = tmp.path().join("decoy");

        let err =
            Listener::bind_hooked(&Endpoint::unix(dir.join("daemon.sock")), geteuid(), || {
                fs::create_dir(&decoy).unwrap();
                fs::set_permissions(&decoy, fs::Permissions::from_mode(0o700)).unwrap();
                fs::rename(&decoy, &dir).unwrap();
            })
            .expect_err("a replaced parent is not the judged one");
        let reason = parent_reason(err);
        assert!(reason.contains("was replaced"), "{reason}");
    }

    /// `/proc/net/unix` records the pathname given to `bind` and never
    /// updates it, which is what makes systemd-tmpfiles exempt a live
    /// socket; a listener bound elsewhere and renamed would be aged.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_socket_is_bound_at_the_name_it_keeps() {
        let tmp = private_dir();
        let path = tmp.path().join("named.sock");
        let _server = Listener::bind(&Endpoint::unix(path.clone())).unwrap();

        let registered = fs::read_to_string("/proc/net/unix").unwrap();
        assert!(
            registered.contains(&path.display().to_string()),
            "{} is not in /proc/net/unix",
            path.display()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_binds_elect_exactly_one_listener() {
        let tmp = private_dir();
        let path = tmp.path().join("race.sock");

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let endpoint = Endpoint::unix(path.clone());
            tasks.push(tokio::task::spawn_blocking(move || {
                Listener::bind(&endpoint)
            }));
        }
        let mut winners = Vec::new();
        let mut losers = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(listener) => winners.push(listener),
                Err(BindError::Io(e)) => {
                    assert_eq!(
                        e.kind(),
                        io::ErrorKind::AddrInUse,
                        "unexpected loser error: {e}"
                    );
                    losers += 1;
                }
                Err(other) => panic!("unexpected loser error: {other}"),
            }
        }
        assert_eq!(winners.len(), 1, "exactly one bind must win");
        assert_eq!(losers, 7);

        // The losers' liveness probes sit in the backlog as immediately
        // closed connections; skip them until the real client's ping.
        let server = winners.pop().unwrap();
        let server_task = tokio::spawn(async move {
            loop {
                let mut stream = server.accept().await.unwrap();
                let mut buf = [0u8; 4];
                match stream.read_exact(&mut buf).await {
                    Ok(_) => {
                        assert_eq!(&buf, b"ping");
                        return;
                    }
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {}
                    Err(e) => panic!("unexpected read error: {e}"),
                }
            }
        });
        let mut client = connect(&path).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        server_task.await.unwrap();
    }

    /// A leaked 0o177 umask would over-restrict every file the daemon
    /// creates later.
    #[tokio::test]
    async fn bind_restores_the_process_umask() {
        let tmp = private_dir();
        let path = tmp.path().join("umask.sock");
        let original = swap_umask(Mode::from_raw_mode(0o022));
        let _l = Listener::bind(&Endpoint::unix(path)).unwrap();
        let left_behind = swap_umask(original);
        assert_eq!(
            left_behind,
            Mode::from_raw_mode(0o022),
            "bind must restore the umask it found",
        );
    }

    #[tokio::test]
    async fn connection_from_a_different_uid_is_refused_before_application_bytes() {
        // A test cannot connect as another UID, so the listener expects
        // one that cannot be ours.
        let tmp = private_dir();
        let path = tmp.path().join("uid.sock");
        let bogus = if geteuid() == 0 { 65534 } else { 0 };
        let server = Listener::bind_with_uid(&Endpoint::unix(path), bogus).unwrap();

        let server_path = server.path().to_path_buf();
        let server_task = tokio::spawn(async move {
            match server.accept().await {
                Err(AcceptError::Peer(PeerError::UidMismatch { peer, expected })) => {
                    assert_eq!(expected, bogus);
                    peer
                }
                other => panic!("expected UidMismatch, got {other:?}"),
            }
        });

        let _client = connect(&server_path).await.unwrap();
        let observed_peer = server_task.await.unwrap();
        assert_eq!(observed_peer, geteuid());
    }
}

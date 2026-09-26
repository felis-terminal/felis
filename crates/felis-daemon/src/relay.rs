//! Cross-host SSH stdio relay (`felis-daemon relay`), byte-transparent:
//! it parses no frame, and the only thing it writes of its own is the
//! optional `FRLY` carrier block ahead of the splice
//! (`docs/reference/ipc.md` "Relay carrier block", with the caps and
//! degrade contract argued in `docs/explanation/architecture/ipc.md`).

use std::path::{Path, PathBuf};

use std::io;

use felis_protocol::preface::CarrierBlock;
use felis_transport::local;
use felis_transport::preface::connect_error_is_absent;
use felis_transport::retry::{RetryPolicy, retry_with_backoff};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{error, info, warn};

/// Starting a daemon is the one step the relay tests must not take: it
/// execs `current_exe`, which under `cargo test` is the test binary.
type SpawnDaemon = dyn Fn(&Path) -> io::Result<()> + Send + Sync;

struct RelayDeps<'a> {
    spawn: &'a SpawnDaemon,
    /// The listener identity the dial admits (REQ-106). Its own euid,
    /// except in the tests that need a peer it cannot be.
    #[cfg(unix)]
    expected_uid: u32,
}

pub async fn run_stdio_relay(socket: PathBuf, no_spawn: bool) -> io::Result<()> {
    let deps = RelayDeps {
        spawn: &spawn_persistent_daemon,
        #[cfg(unix)]
        expected_uid: rustix::process::geteuid().as_raw(),
    };
    run_relay(socket, no_spawn, &deps).await
}

async fn run_relay(socket: PathBuf, no_spawn: bool, deps: &RelayDeps<'_>) -> io::Result<()> {
    info!(socket = %socket.display(), "starting felis-daemon stdio relay");
    let (sock_read, sock_write) = open_daemon_link(&socket, no_spawn, deps).await?;
    // `copy_bidirectional` propagates each half-close: an SSH pipe EOF
    // makes the daemon see the subscriber leave, and a daemon-side close
    // EOFs SSH stdout so `ssh` exits.
    let mut sock_write = sock_write;
    prepend_environment(&mut sock_write, &capture_environment()).await;
    let remote = tokio::io::join(sock_read, sock_write);
    let local_pipe = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let (to_daemon, to_client) = pump(local_pipe, remote).await?;
    info!(to_daemon, to_client, "stdio relay closed");
    Ok(())
}

/// Write relay environment block before client preface bytes.
///
/// Send failures degrade gracefully: without the carrier block, the daemon
/// falls back to its own environment rather than terminating the session.
async fn prepend_environment<W: AsyncWrite + Unpin>(write_half: &mut W, block: &CarrierBlock) {
    if let Err(err) = felis_transport::preface::write_carrier_block(write_half, block).await {
        warn!(
            "could not send the relay environment block; the remote daemon will fall back to its \
             own environment: {err}"
        );
    }
}

/// This relay's own environment as a carrier block. Raw platform
/// bytes, not `String`: a forwarded `SSH_AUTH_SOCK` path is not
/// guaranteed UTF-8, and a lossy conversion would cost the child the
/// very agent this carries.
fn capture_environment() -> CarrierBlock {
    CarrierBlock {
        env: std::env::vars_os()
            .map(|(key, value)| (felis_pty::env_bytes(&key), felis_pty::env_bytes(&value)))
            .collect(),
    }
}

/// Connect, or spawn a detached daemon and retry under
/// [`RetryPolicy::DAEMON_BOOT`], the same window the client's local
/// autospawn uses. Only a connect that proves nothing is listening
/// reaches the spawn (`docs/reference/cli.md` "Auto-spawning").
async fn open_daemon_link(
    socket: &Path,
    no_spawn: bool,
    deps: &RelayDeps<'_>,
) -> io::Result<(local::ReadHalf, local::WriteHalf)> {
    let cold = match dial(socket, deps).await {
        Ok(halves) => return Ok(halves),
        Err(err) if !connect_error_is_absent(&err) => return Err(undialable(socket, &err)),
        Err(err) => err,
    };
    if no_spawn {
        no_daemon_here(socket, &cold);
        return Err(cold);
    }
    info!(socket = %socket.display(), "persistent daemon unreachable; auto-spawning");
    (deps.spawn)(socket)?;
    retry_with_backoff(|| dial(socket, deps), RetryPolicy::DAEMON_BOOT)
        .await
        .map_err(|err| err.source)
}

#[cfg(unix)]
async fn dial(
    socket: &Path,
    deps: &RelayDeps<'_>,
) -> io::Result<(local::ReadHalf, local::WriteHalf)> {
    local::connect_expecting(socket, deps.expected_uid).await
}

/// A named pipe's client identity is the server's to check; there is no
/// second uid to name here.
#[cfg(windows)]
async fn dial(
    socket: &Path,
    _deps: &RelayDeps<'_>,
) -> io::Result<(local::ReadHalf, local::WriteHalf)> {
    local::connect(socket).await
}

/// stderr rides the SSH channel back to the caller's terminal, so these
/// lines are the user-facing diagnosis.
fn no_daemon_here(socket: &Path, err: &io::Error) {
    error!(
        socket = %socket.display(),
        "no daemon on this host (--no-spawn; start one with a remote `felis sessions spawn` or by \
         opening a window onto this host): {err}"
    );
}

/// A connect error other than "nothing is listening" is no evidence the
/// endpoint is free, and spawning on it would put a second daemon beside
/// one this relay merely failed to reach.
fn undialable(socket: &Path, err: &io::Error) -> io::Error {
    error!(
        socket = %socket.display(),
        "could not dial the daemon endpoint, and this is not evidence that none is there; not \
         starting one: {err}"
    );
    io::Error::new(
        err.kind(),
        format!("could not dial {}: {err}", socket.display()),
    )
}

/// The daemon this relay starts: the sibling `felis-daemon` when one
/// exists, else `current_exe` itself. Under a launcher-driven layout
/// `current_exe` is the loader, and re-execing it would run the loader
/// with `serve` as its argument.
fn daemon_program(current_exe: &Path) -> PathBuf {
    current_exe
        .parent()
        .map(|dir| dir.join(format!("felis-daemon{}", std::env::consts::EXE_SUFFIX)))
        .filter(|sibling| sibling.is_file())
        .unwrap_or_else(|| current_exe.to_path_buf())
}

/// Spawn `felis-daemon serve --socket <socket>` as a detached child.
///
/// Resolves the daemon beside `current_exe` to match relay build version
/// and isolates the process group on Unix so SSH disconnect SIGHUP does
/// not kill the daemon.
fn spawn_persistent_daemon(socket: &Path) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let program = daemon_program(&std::env::current_exe()?);
    let mut cmd = Command::new(program);
    cmd.args(["serve", "--socket"])
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Inherit stderr so the daemon's boot tracing reaches the
        // SSH-forwarded stderr. After the link drops the fd is a broken
        // pipe, but Rust ignores SIGPIPE so the stray write merely errors.
        .stderr(Stdio::inherit())
        // A relay running under a `Type=notify` unit must not lend its
        // endpoint to the daemon it forks: `serve` acts on the variable,
        // and the daemon would report the relay's unit ready.
        .env_remove(crate::NOTIFY_SOCKET_ENV);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    Ok(())
}

async fn pump<L, R>(mut local_pipe: L, mut remote: R) -> io::Result<(u64, u64)>
where
    L: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + AsyncWrite + Unpin,
{
    tokio::io::copy_bidirectional(&mut local_pipe, &mut remote).await
}

#[cfg(test)]
mod tests {
    use felis_protocol::preface::{ClientPreface, MAX_CARRIER_ENTRIES};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Decodes the platform bytes: a byte literal would miss Windows's
    /// UTF-16LE `Path`.
    fn carries_path(env: &[(Vec<u8>, Vec<u8>)]) -> bool {
        env.iter().any(|(key, _)| {
            felis_pty::env_from_bytes(key).is_some_and(|key| key.eq_ignore_ascii_case("PATH"))
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_relays_bytes_both_directions() {
        let (mut client_side, relay_local) = tokio::io::duplex(64 * 1024);
        let (relay_remote, mut daemon_side) = tokio::io::duplex(64 * 1024);
        let pumping = tokio::spawn(pump(relay_local, relay_remote));

        client_side.write_all(b"to-daemon").await.unwrap();
        let mut buf = [0u8; 9];
        daemon_side.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"to-daemon");

        daemon_side.write_all(b"to-client").await.unwrap();
        let mut buf = [0u8; 9];
        client_side.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"to-client");

        drop(client_side);
        drop(daemon_side);
        let (to_daemon, to_client) = pumping.await.unwrap().unwrap();
        assert_eq!(to_daemon, 9);
        assert_eq!(to_client, 9);
    }

    /// The block goes out first; everything after it is the plain byte
    /// pump.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_relay_prepends_its_environment_then_stays_transparent() {
        let (mut daemon_side, relay_remote) = tokio::io::duplex(256 * 1024);
        let (mut client_side, relay_local) = tokio::io::duplex(64 * 1024);
        let (relay_read, mut relay_write) = tokio::io::split(relay_remote);

        prepend_environment(&mut relay_write, &capture_environment()).await;
        let block = felis_transport::preface::read_client_bootstrap(&mut daemon_side);

        let pumping = tokio::spawn(async move {
            let remote = tokio::io::join(relay_read, relay_write);
            pump(relay_local, remote).await
        });
        client_side
            .write_all(&ClientPreface::CURRENT.encode())
            .await
            .unwrap();
        let opened = block.await.unwrap();

        let carrier = opened.carrier.expect("the relay prepends a block");
        assert!(
            carries_path(&carrier.env),
            "the block carries this process's environment"
        );
        assert_eq!(
            opened.preface,
            ClientPreface::CURRENT,
            "the client's own preface crosses untouched"
        );

        client_side.write_all(b"after").await.unwrap();
        let mut seen = [0u8; 5];
        daemon_side.read_exact(&mut seen).await.unwrap();
        assert_eq!(&seen, b"after");
        drop(client_side);
        drop(daemon_side);
        drop(pumping.await.unwrap());
    }

    /// Over either frozen cap the relay degrades to "no block": the
    /// daemon reads a bare `FLIS` stream and falls back to its own
    /// environment (`docs/reference/ipc.md` "Relay carrier block").
    #[tokio::test]
    async fn an_over_cap_environment_is_not_sent_at_all() {
        let over_cap = CarrierBlock {
            env: (0..=MAX_CARRIER_ENTRIES)
                .map(|n| (format!("K{n}").into_bytes(), b"v".to_vec()))
                .collect(),
        };
        assert!(over_cap.encode().is_err(), "the block is over the cap");
        let (mut daemon_side, mut relay_write) = tokio::io::duplex(64 * 1024);

        prepend_environment(&mut relay_write, &over_cap).await;
        relay_write
            .write_all(&ClientPreface::CURRENT.encode())
            .await
            .unwrap();

        let opened = felis_transport::preface::read_client_bootstrap(&mut daemon_side)
            .await
            .unwrap();
        assert!(
            opened.carrier.is_none(),
            "no carrier bytes reached the daemon"
        );
        assert_eq!(opened.preface, ClientPreface::CURRENT);
    }

    /// A layout whose `current_exe` is not the daemon (a launcher tree
    /// where the loader is what execs) must still reach the daemon.
    #[test]
    fn the_relay_prefers_the_daemon_beside_current_exe() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = dir
            .path()
            .join(format!("felis-daemon{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&sibling, b"").unwrap();
        let loader = dir.path().join("loader");

        assert_eq!(daemon_program(&loader), sibling);
    }

    /// Every layout that ships today has the relay binary itself as the
    /// daemon, so an absent sibling must leave the re-exec untouched.
    #[test]
    fn the_relay_re_execs_itself_when_no_sibling_daemon_exists() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("felis-daemon-under-another-name");
        std::fs::write(&exe, b"").unwrap();

        assert_eq!(daemon_program(&exe), exe);
    }

    #[test]
    fn the_capture_is_this_processs_environment() {
        let block = capture_environment();
        assert!(carries_path(&block.env));
        assert!(
            block.encode().is_ok(),
            "an ordinary environment fits the frozen caps"
        );
    }
}

// Unix-only: binds `serve_unix` on a real socket path.
#[cfg(all(test, unix))]
mod unix_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;
    use std::time::Duration;

    use felis_protocol::{
        ConnectionMode, codec,
        messages::{ConnToClientMsg, ConnToDaemonMsg},
        preface::{ClientPreface, DaemonPreface, PROTOCOL_MAJOR, PROTOCOL_MINOR},
    };
    use felis_transport::connect;
    use felis_transport::{
        FrameReader, FrameWriter,
        preface::{read_daemon_preface, write_client_preface},
    };
    use tempfile::TempDir;

    /// A socket parent must be a `0700` directory this uid owns
    /// (REQ-107), and `TempDir` follows the process umask.
    fn private_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        tmp
    }
    use tokio::sync::Mutex;

    use super::*;
    use crate::serve::DaemonCaps;
    use crate::{SessionPool, serve_unix};

    /// Records every autospawn instead of exec'ing this test binary.
    struct SpawnLog {
        paths: Arc<std::sync::Mutex<Vec<PathBuf>>>,
        spawn: Box<SpawnDaemon>,
    }

    impl SpawnLog {
        fn new() -> Self {
            let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = paths.clone();
            let spawn: Box<SpawnDaemon> = Box::new(move |path: &Path| {
                recorded.lock().expect("spawn log").push(path.to_path_buf());
                Ok(())
            });
            Self { paths, spawn }
        }

        fn deps(&self) -> RelayDeps<'_> {
            RelayDeps {
                spawn: self.spawn.as_ref(),
                #[cfg(unix)]
                expected_uid: rustix::process::geteuid().as_raw(),
            }
        }

        #[cfg(unix)]
        fn deps_expecting(&self, expected_uid: u32) -> RelayDeps<'_> {
            RelayDeps {
                spawn: self.spawn.as_ref(),
                expected_uid,
            }
        }

        fn paths(&self) -> Vec<PathBuf> {
            self.paths.lock().expect("spawn log").clone()
        }
    }

    /// `None` when the caller can read the directory anyway (root, or
    /// `CAP_DAC_OVERRIDE`), for which `EACCES` cannot be provoked.
    fn undialable_dir(parent: &Path, name: &str) -> Option<PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = parent.join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&dir).is_ok() {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            return None;
        }
        Some(dir)
    }

    async fn wait_connectable(path: &Path) {
        for _ in 0..100 {
            if connect(path).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("daemon never became connectable");
    }

    /// A preface plus `Hello` written straight onto the returned halves
    /// draws the daemon's `Welcome`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_reaches_a_live_persistent_daemon() {
        let tmp = private_dir();
        let path = tmp.path().join("daemon.sock");
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let server_path = path.clone();
        tokio::spawn(async move {
            drop(serve_unix(&server_path, DaemonCaps::default(), pool).await);
        });
        // Pre-bind so the autospawn path, which would exec the test binary,
        // is never taken.
        wait_connectable(&path).await;

        let spawns = SpawnLog::new();
        let (mut read_half, mut write_half) = open_daemon_link(&path, false, &spawns.deps())
            .await
            .unwrap();
        write_client_preface(&mut write_half, ClientPreface::CURRENT)
            .await
            .unwrap();
        assert_eq!(
            read_daemon_preface(&mut read_half).await.unwrap(),
            DaemonPreface::Accept {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            },
        );

        let mut reader = FrameReader::new(read_half);
        let mut writer = FrameWriter::at_build_minor(write_half);
        writer
            .send(&ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Window,
                pull_paced: false,
            })
            .await
            .unwrap();

        let welcome = reader.next_frame().await.unwrap().expect("welcome");
        match codec::decode::<ConnToClientMsg>(&welcome.body).unwrap() {
            ConnToClientMsg::Welcome { .. } => {}
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    /// Carrier block, then the client's preface, then frames, over a real
    /// socket: the bootstrap read happens on the raw halves before any
    /// framing exists.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_daemon_accepts_a_carrier_prefixed_connection() {
        let tmp = private_dir();
        let path = tmp.path().join("daemon.sock");
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let server_path = path.clone();
        tokio::spawn(async move {
            drop(serve_unix(&server_path, DaemonCaps::default(), pool).await);
        });
        wait_connectable(&path).await;

        let (mut read_half, mut write_half) = connect(&path).await.unwrap();
        prepend_environment(&mut write_half, &capture_environment()).await;
        write_client_preface(&mut write_half, ClientPreface::CURRENT)
            .await
            .unwrap();
        assert_eq!(
            read_daemon_preface(&mut read_half).await.unwrap(),
            DaemonPreface::Accept {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            },
            "the block is consumed exactly, so the preface reply follows it"
        );

        let mut reader = FrameReader::new(read_half);
        let mut writer = FrameWriter::at_build_minor(write_half);
        writer
            .send(&ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Window,
                pull_paced: false,
            })
            .await
            .unwrap();
        let welcome = reader.next_frame().await.unwrap().expect("welcome");
        match codec::decode::<ConnToClientMsg>(&welcome.body).unwrap() {
            ConnToClientMsg::Welcome { .. } => {}
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    /// A remote `sessions list` must not quietly start a daemon and
    /// report an empty roster.
    #[tokio::test]
    async fn no_spawn_relay_fails_on_a_cold_socket_without_spawning() {
        let dir = private_dir();
        let path = dir.path().join("cold.sock");
        let spawns = SpawnLog::new();
        let err = run_relay(path.clone(), true, &spawns.deps())
            .await
            .expect_err("cold socket with --no-spawn must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert!(spawns.paths().is_empty(), "spawned: {:?}", spawns.paths());
    }

    /// A cold endpoint is the one thing that licenses a spawn.
    #[tokio::test]
    async fn a_cold_socket_spawns_where_it_resolved() {
        let dir = private_dir();
        let path = dir.path().join("cold.sock");
        let spawns = SpawnLog::new();

        let err = open_daemon_link(&path, false, &spawns.deps())
            .await
            .expect_err("the fake spawn stands up no daemon, so the retry runs out");

        assert_eq!(spawns.paths(), vec![path]);
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    /// `EACCES` is not evidence that nothing is listening: a daemon may
    /// hold the endpoint behind a directory this process cannot search,
    /// and spawning would split the session roster.
    #[tokio::test]
    async fn a_connect_error_that_is_not_absence_never_spawns() {
        let dir = private_dir();
        let Some(closed) = undialable_dir(dir.path(), "closed") else {
            return;
        };
        let path = closed.join("daemon.sock");

        for no_spawn in [false, true] {
            let spawns = SpawnLog::new();
            let err = open_daemon_link(&path, no_spawn, &spawns.deps())
                .await
                .expect_err("an endpoint that cannot be dialed is not a free one");

            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
            assert!(
                err.to_string().contains(&path.display().to_string()),
                "{err}"
            );
            assert!(spawns.paths().is_empty(), "spawned: {:?}", spawns.paths());
        }
    }

    /// A relay that reached a listener of another uid has not found its
    /// daemon: it reports the mismatch, spawns nothing, and never sends
    /// the carrier block, which carries its whole environment.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_listener_of_another_uid_is_reported_and_never_spawned_over() {
        let dir = private_dir();
        let path = dir.path().join("daemon.sock");
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let server_path = path.clone();
        tokio::spawn(async move {
            drop(serve_unix(&server_path, DaemonCaps::default(), pool).await);
        });
        wait_connectable(&path).await;
        let own = rustix::process::geteuid().as_raw();
        let bogus = if own == 0 { 65534 } else { 0 };

        for no_spawn in [false, true] {
            let spawns = SpawnLog::new();
            let err = open_daemon_link(&path, no_spawn, &spawns.deps_expecting(bogus))
                .await
                .expect_err("a listener of another uid is not this uid's daemon");

            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
            assert!(err.to_string().contains("does not match"), "{err}");
            assert!(spawns.paths().is_empty(), "spawned: {:?}", spawns.paths());
        }
    }

    /// The relay resolves what every other process of this uid
    /// resolves, whatever the SSH server passed on, so a relay under
    /// Tailscale SSH reaches the daemon a desktop login started.
    #[cfg(unix)]
    #[test]
    fn the_relay_resolves_the_uid_derived_endpoint() {
        use felis_transport::socket::SocketPath;

        assert_eq!(
            SocketPath::resolve(1234).socket(),
            Path::new("/tmp/felis.1234/daemon.sock")
        );
    }
}

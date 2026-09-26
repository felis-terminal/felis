//! Daemon autospawn: connect to a daemon socket, spawning a
//! `felis-daemon serve` child and retrying when none is up.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::info;
#[cfg(target_os = "linux")]
use tracing::{debug, warn};

use crate::connector::{
    Carrier, CarrierConnection, ConnectError, Offer, RemoteSpawn, connect_carrier,
    connect_carrier_with_retry,
};
use felis_transport::retry::RetryPolicy;

#[cfg(target_os = "linux")]
mod systemd;

#[derive(Debug, Error)]
pub enum SpawnConnectError {
    #[error("spawn felis-daemon (is it on PATH?): {0}")]
    Spawn(#[source] std::io::Error),
    #[error(transparent)]
    Connect(#[from] ConnectError),
}

/// Autospawn covers a cold socket and a daemon that vanished
/// mid-handshake; a typed refusal, `at_capacity` included, fails fast
/// with the daemon's own answer.
pub async fn connect_or_spawn_daemon(
    socket: &Path,
    offer: Offer,
) -> Result<CarrierConnection, SpawnConnectError> {
    match connect_carrier(Carrier::Local(socket.into()), offer, RemoteSpawn::Allow).await {
        Ok(c) => return Ok(c),
        // Not `is_transient`: a full daemon is worth retrying and is
        // exactly the daemon that must not be spawned over, because the
        // second daemon fails its bind and buries the refusal under a
        // boot timeout.
        Err(err) if !err.may_be_a_cold_socket() => return Err(err.into()),
        Err(_) => {}
    }
    info!(socket = %socket.display(), "daemon socket unreachable; auto-spawning felis-daemon");
    #[cfg(target_os = "linux")]
    match hand_off_to_user_manager(socket, offer).await {
        HandOff::Connected(connection) => return Ok(*connection),
        HandOff::Undialable(err) => return Err(err.into()),
        HandOff::Fork => {}
    }
    spawn_daemon_child(socket).map_err(SpawnConnectError::Spawn)?;
    Ok(connect_carrier_with_retry(
        Carrier::Local(socket.into()),
        offer,
        RetryPolicy::DAEMON_BOOT,
    )
    .await?)
}

/// The child is not killed on this binary's exit; it outlives the window.
fn spawn_daemon_child(socket: &Path) -> std::io::Result<()> {
    daemon_command(&daemon_program(), socket).spawn()?;
    Ok(())
}

/// Sibling first, PATH as fallback: `nix run .#felis` does not put
/// `$out/bin` on PATH, so a bare lookup would pair a release client with
/// whatever daemon the ambient PATH offers (e.g. a debug build).
fn daemon_program() -> OsString {
    sibling_daemon().map_or_else(|| OsString::from(daemon_name()), Into::into)
}

fn daemon_name() -> String {
    format!("felis-daemon{}", std::env::consts::EXE_SUFFIX)
}

fn sibling_daemon() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(daemon_name())))
        .filter(|sibling| sibling.is_file())
}

fn daemon_command(program: &OsStr, socket: &Path) -> std::process::Command {
    use std::process::{Command, Stdio};
    let mut command = Command::new(program);
    // An inherited stderr would leave the reparented daemon writing to a
    // pipe the caller closed: every log event hits `EPIPE`, and the
    // subscriber's error path `eprintln!`s onto the same broken stderr,
    // panicking inside the panic hook. The daemon's log-file tee
    // (`felis-transport::logging::open_log_file`) keeps every line.
    command
        .args(["serve", "--socket"])
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A launcher inside another `Type=notify` unit must not lend its
        // endpoint to the daemon it forks: `serve` acts on the variable,
        // and the manager would read this daemon as that unit's readiness.
        .env_remove("NOTIFY_SOCKET");
    command
}

/// Asks the systemd user manager to run the daemon as a transient
/// service, so it lands in `app.slice` with `OOMPolicy=continue` instead
/// of the launching window's unit
/// (`docs/explanation/architecture/overview.md` "Where an
/// auto-spawned daemon lands").
#[cfg(target_os = "linux")]
enum HandOff {
    Connected(Box<CarrierConnection>),
    /// The caller forks.
    Fork,
    /// The caller does neither: the endpoint is not free.
    Undialable(ConnectError),
}

#[cfg(target_os = "linux")]
async fn hand_off_to_user_manager(socket: &Path, offer: Offer) -> HandOff {
    // The manager's PATH is not the launcher's, so the daemon the fork
    // would have found by name has to be named absolutely here.
    let Some(program) = sibling_daemon().map(OsString::from).or_else(daemon_on_path) else {
        return HandOff::Fork;
    };
    let Some(hand_off) = systemd::HandOff::production(program) else {
        return HandOff::Fork;
    };
    match ask_user_manager(&hand_off, socket, offer).await {
        Ok(connection) => HandOff::Connected(Box::new(connection)),
        Err(systemd::Fallback::Undialable(err)) => HandOff::Undialable(err),
        // One line per attempt, and only for an attempt: a launcher that
        // never asked is the ordinary case on every host without a user
        // manager, and would otherwise warn on each cold socket.
        Err(systemd::Fallback::NotAsked(reason)) => {
            debug!(reason, "daemon hand-off not attempted; forking");
            HandOff::Fork
        }
        Err(systemd::Fallback::Failed(reason)) => {
            warn!(
                reason,
                "daemon hand-off to the systemd user manager failed; forking"
            );
            HandOff::Fork
        }
    }
}

#[cfg(target_os = "linux")]
async fn ask_user_manager(
    hand_off: &systemd::HandOff,
    socket: &Path,
    offer: Offer,
) -> Result<CarrierConnection, systemd::Fallback> {
    use systemd::{Fallback, Start};

    if !hand_off.under_manager() {
        return Err(Fallback::NotAsked(
            "the launcher is not under the systemd user manager".to_owned(),
        ));
    }
    let unit = systemd::unit_name(socket);
    let contested = match hand_off.start(&unit, socket).await {
        // A start of this launcher's own unit that reports ready and
        // leaves the socket cold has nothing to wait for: the manager
        // ran something that is not serving this path.
        Start::Started => {
            return connect_managed(socket, offer, hand_off.boot)
                .await
                .map_err(|err| {
                    managed_failure(err, || {
                        format!("the unit {unit} started but left the socket cold")
                    })
                });
        }
        Start::Contested(reason) => reason,
        Start::Unavailable(reason) => return Err(Fallback::NotAsked(reason)),
    };
    // One state query, whatever the start reported: `active` or
    // `activating` means a concurrent launcher's start job holds the
    // name, and waiting it out is what keeps the loser from forking a
    // daemon into its own cgroup, the placement this hand-off avoids.
    let concurrent = hand_off
        .active_state(&unit)
        .await
        .is_some_and(|state| systemd::start_job_holds_the_name(&state));
    if concurrent {
        match connect_managed(socket, offer, hand_off.managed_boot).await {
            Ok(connection) => return Ok(connection),
            Err(err) => {
                if let fatal @ Fallback::Undialable(_) = managed_failure(err, String::new) {
                    return Err(fatal);
                }
            }
        }
    }
    Err(Fallback::Failed(contested))
}

/// A managed start that left the endpoint undialable is not a reason to
/// fork: forking would put a second daemon beside whatever holds it.
#[cfg(target_os = "linux")]
fn managed_failure(err: ConnectError, cold: impl FnOnce() -> String) -> systemd::Fallback {
    if err.may_be_a_cold_socket() {
        systemd::Fallback::Failed(cold())
    } else {
        systemd::Fallback::Undialable(err)
    }
}

#[cfg(target_os = "linux")]
async fn connect_managed(
    socket: &Path,
    offer: Offer,
    policy: RetryPolicy,
) -> Result<CarrierConnection, ConnectError> {
    connect_carrier_with_retry(Carrier::Local(socket.into()), offer, policy).await
}

#[cfg(target_os = "linux")]
fn daemon_on_path() -> Option<OsString> {
    let name = daemon_name();
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
        .map(Into::into)
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use felis_protocol::preface::{DaemonPreface, PROTOCOL_MAJOR};
    use felis_transport::{Endpoint, local::Listener, preface::write_daemon_preface};
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

    #[test]
    fn the_forked_daemon_is_not_handed_the_launchers_readiness_endpoint() {
        let command = daemon_command(
            OsStr::new("felis-daemon"),
            Path::new("/run/user/1000/felis/daemon.sock"),
        );
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "NOTIFY_SOCKET" && value.is_none()),
            "the fork must clear NOTIFY_SOCKET, not pass the launcher's",
        );
    }

    /// The branch tests below drive `ask_user_manager` directly: it is
    /// the half that decides between the manager and the fork, and the
    /// fork itself is the caller's single unconditional step.
    #[cfg(target_os = "linux")]
    mod hand_off {
        use super::*;
        use crate::spawn::systemd::{self, Fallback, HandOff};
        use std::ffi::OsString;
        use std::num::NonZeroU32;
        use std::path::{Path, PathBuf};
        use std::time::Duration;

        const UNDER_MANAGER: &str =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/niri.service\n";
        const SESSION_SCOPE: &str = "0::/user.slice/user-1000.slice/session-3.scope\n";

        /// Long enough to outlast the late bind below, short enough that
        /// a broken wait fails the test rather than hanging it.
        const fn patient() -> RetryPolicy {
            RetryPolicy {
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(50),
                max_attempts: match NonZeroU32::new(60) {
                    Some(attempts) => attempts,
                    None => unreachable!(),
                },
            }
        }

        const fn brisk() -> RetryPolicy {
            RetryPolicy {
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(5),
                max_attempts: match NonZeroU32::new(4) {
                    Some(attempts) => attempts,
                    None => unreachable!(),
                },
            }
        }

        /// `systemd-run` is called twice per attempt (`--version`, then
        /// the start), so the stub answers the version probe first.
        fn systemd_run(dir: &Path, record: &Path, exit: i32) -> OsString {
            let record = record.display();
            systemd::stub(
                dir,
                "systemd-run",
                &format!(
                    "echo \"run $*\" >> {record}\n\
                     case \"$1\" in --version) echo 'systemd 261 (261.1)'; exit 0;; esac\n\
                     echo 'the manager refused' >&2\n\
                     exit {exit}"
                ),
            )
            .into()
        }

        fn systemctl(dir: &Path, record: &Path, state: &str) -> OsString {
            let record = record.display();
            systemd::stub(
                dir,
                "systemctl",
                &format!("echo \"show $*\" >> {record}\necho {state}"),
            )
            .into()
        }

        fn hand_off(systemd_run: OsString, systemctl: OsString, cgroup: &str) -> HandOff {
            HandOff {
                systemd_run,
                systemctl,
                cgroup: cgroup.to_owned(),
                uid: 1000,
                program: OsString::from("/nowhere/felis-daemon"),
                helper_budget: Duration::from_secs(5),
                start_budget: Duration::from_secs(5),
                boot: brisk(),
                managed_boot: brisk(),
            }
        }

        fn queries(record: &Path) -> Vec<String> {
            std::fs::read_to_string(record)
                .unwrap_or_default()
                .lines()
                .map(ToOwned::to_owned)
                .collect()
        }

        /// A daemon on the socket, standing in for the one a concurrent
        /// launcher's unit started.
        async fn serve_forever(socket: PathBuf) {
            use felis_daemon::{SessionPool, serve::DaemonCaps, serve_unix};
            use std::sync::Arc;
            use tokio::sync::Mutex;

            drop(
                serve_unix(
                    &socket,
                    DaemonCaps::default(),
                    Arc::new(Mutex::new(SessionPool::new())),
                )
                .await,
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_launcher_that_lost_the_unit_name_waits_for_the_winner_instead_of_forking() {
            let tmp = private_dir();
            let record = tmp.path().join("calls");
            let mut hand_off = hand_off(
                systemd_run(tmp.path(), &record, 1),
                systemctl(tmp.path(), &record, "activating"),
                UNDER_MANAGER,
            );
            hand_off.managed_boot = patient();
            let socket = tmp.path().join("contested.sock");
            // The winner is still starting when the loser asks, so a
            // single immediate connect would find the socket cold: only
            // a wait that outlasts the start job connects here.
            let late = socket.clone();
            let winner = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                serve_forever(late).await;
            });

            let connection = ask_user_manager(&hand_off, &socket, Offer::ops())
                .await
                .expect("the winner's daemon answers, so nothing is forked");
            drop(connection);

            let calls = queries(&record);
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.starts_with("show "))
                    .count(),
                1,
                "exactly one state query decides it: {calls:?}"
            );
            winner.abort();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_unit_that_is_not_starting_sends_the_launcher_straight_to_the_fork() {
            let tmp = private_dir();
            let record = tmp.path().join("calls");
            let hand_off = hand_off(
                systemd_run(tmp.path(), &record, 1),
                systemctl(tmp.path(), &record, "failed"),
                UNDER_MANAGER,
            );

            let err = ask_user_manager(&hand_off, &tmp.path().join("cold.sock"), Offer::ops())
                .await
                .err()
                .expect("a failed unit leaves no daemon to connect to");
            let Fallback::Failed(reason) = err else {
                panic!("the manager was asked, so the fallback is a warning");
            };
            assert!(reason.contains("the manager refused"), "got {reason}");
            assert_eq!(
                queries(&record)
                    .iter()
                    .filter(|call| call.starts_with("show "))
                    .count(),
                1,
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_start_job_that_never_produces_a_daemon_ends_in_the_fork() {
            let tmp = private_dir();
            let record = tmp.path().join("calls");
            let hand_off = hand_off(
                systemd_run(tmp.path(), &record, 1),
                systemctl(tmp.path(), &record, "active"),
                UNDER_MANAGER,
            );

            let err = ask_user_manager(&hand_off, &tmp.path().join("cold.sock"), Offer::ops())
                .await
                .err()
                .expect("the budget expires on a socket that stays cold");
            assert!(matches!(err, Fallback::Failed(_)), "got {err:?}");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_start_that_reports_success_but_serves_nothing_ends_in_the_fork() {
            let tmp = private_dir();
            let record = tmp.path().join("calls");
            let hand_off = hand_off(
                systemd_run(tmp.path(), &record, 0),
                systemctl(tmp.path(), &record, "inactive"),
                UNDER_MANAGER,
            );

            let err = ask_user_manager(&hand_off, &tmp.path().join("cold.sock"), Offer::ops())
                .await
                .err()
                .expect("exit 0 is not a daemon on the socket");
            let Fallback::Failed(reason) = err else {
                panic!("the manager was asked, so the fallback is a warning");
            };
            assert!(reason.contains("left the socket cold"), "got {reason}");
            assert!(
                !queries(&record)
                    .iter()
                    .any(|call| call.starts_with("show ")),
                "this launcher's own unit started; there is no winner to wait for"
            );
        }

        const NOISE_OUT: &str = "felis-test-noise-on-stdout";
        const NOISE_ERR: &str = "felis-test-noise-on-stderr";

        /// The hand-off half of the stdio contract, run as its own
        /// process so the caller's real file descriptors can be read
        /// back by the test below. Ignored in an ordinary run: it is a
        /// fixture, and its verdict is that test's.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "driven as a subprocess by the stdio isolation test"]
        async fn noisy_helpers_are_captured_by_the_launcher() {
            let tmp = private_dir();
            let noisy = format!("echo {NOISE_OUT}\necho {NOISE_ERR} >&2\nexit 1");
            let hand_off = hand_off(
                systemd::stub(tmp.path(), "systemd-run", &noisy).into(),
                systemd::stub(tmp.path(), "systemctl", &noisy).into(),
                UNDER_MANAGER,
            );

            let err = ask_user_manager(&hand_off, &tmp.path().join("cold.sock"), Offer::ops())
                .await
                .err()
                .expect("a refusing manager leaves no daemon");
            let Fallback::Failed(reason) = err else {
                panic!("the manager was asked, so the fallback is a warning");
            };
            assert!(
                reason.contains(NOISE_ERR),
                "the captured stderr is what the warning reports: {reason}"
            );
        }

        /// `systemd-run` writes "Running as unit…" on a good day and
        /// diagnostics on a bad one; a point verb's `--format json`
        /// promises its result object and nothing else, so neither may
        /// reach the caller's own streams.
        #[test]
        fn a_noisy_manager_reaches_neither_stdout_nor_stderr_of_the_caller() {
            let fixture = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "--ignored",
                    "--test-threads=1",
                    "spawn::tests::hand_off::noisy_helpers_are_captured_by_the_launcher",
                ])
                .output()
                .expect("re-run this test binary for the fixture");
            let stdout = String::from_utf8_lossy(&fixture.stdout);
            let stderr = String::from_utf8_lossy(&fixture.stderr);
            assert!(
                fixture.status.success(),
                "the fixture failed: {stdout}{stderr}"
            );
            for stream in [&stdout, &stderr] {
                assert!(
                    !stream.contains(NOISE_OUT) && !stream.contains(NOISE_ERR),
                    "a helper's output reached the caller: {stream}"
                );
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_launcher_outside_the_manager_asks_nothing_of_it() {
            let tmp = private_dir();
            let record = tmp.path().join("calls");
            let hand_off = hand_off(
                systemd_run(tmp.path(), &record, 0),
                systemctl(tmp.path(), &record, "active"),
                SESSION_SCOPE,
            );

            let err = ask_user_manager(&hand_off, &tmp.path().join("cold.sock"), Offer::ops())
                .await
                .err()
                .expect("a session scope forks as it always has");
            assert!(matches!(err, Fallback::NotAsked(_)), "got {err:?}");
            assert!(queries(&record).is_empty(), "nothing may be run");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_host_without_systemd_run_forks_without_a_warning() {
            let tmp = private_dir();
            let hand_off = hand_off(
                OsString::from(tmp.path().join("absent-systemd-run")),
                OsString::from(tmp.path().join("absent-systemctl")),
                UNDER_MANAGER,
            );

            let err = ask_user_manager(&hand_off, &tmp.path().join("cold.sock"), Offer::ops())
                .await
                .err()
                .expect("no manager was reached");
            assert!(matches!(err, Fallback::NotAsked(_)), "got {err:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_typed_refusal_fails_fast_instead_of_autospawning() {
        let tmp = private_dir();
        let path = tmp.path().join("live.sock");
        let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");
        let server_task = tokio::spawn(async move {
            let stream = server.accept().await.expect("accept");
            let (_read, mut write) = stream.into_split();
            write_daemon_preface(
                &mut write,
                DaemonPreface::Refuse {
                    min_major: PROTOCOL_MAJOR + 1,
                    max_major: PROTOCOL_MAJOR + 2,
                },
            )
            .await
            .expect("write refusal");
            std::future::pending::<()>().await;
        });

        let started = std::time::Instant::now();
        let err = connect_or_spawn_daemon(&path, Offer::ops())
            .await
            .err()
            .expect("a refusing daemon must not be autospawned over");
        assert!(
            matches!(
                err,
                SpawnConnectError::Connect(ConnectError::MajorMismatch { .. })
            ),
            "got: {err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must fail fast, not sit out the boot-retry window"
        );
        server_task.abort();
    }

    /// A connect that failed for a reason other than "nothing is
    /// listening" says nothing about whether a daemon holds the
    /// endpoint, so the autospawn must not run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connect_error_that_is_not_absence_is_never_spawned_over() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = private_dir();
        let closed = tmp.path().join("closed");
        std::fs::create_dir(&closed).unwrap();
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&closed).is_ok() {
            // root, or `CAP_DAC_OVERRIDE`: `EACCES` cannot be provoked.
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let path = closed.join("daemon.sock");

        let err = connect_or_spawn_daemon(&path, Offer::ops())
            .await
            .err()
            .expect("an endpoint that cannot be dialed is not a free one");

        let SpawnConnectError::Connect(ConnectError::Connect(io_err)) = err else {
            panic!("expected a raw connect error, got: {err}");
        };
        assert_eq!(io_err.kind(), std::io::ErrorKind::PermissionDenied);
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// An endpoint the kernel refuses to dial at all is the same
    /// verdict as `EACCES`, and it is reachable without the privileges
    /// the test above has to give up on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_endpoint_the_kernel_will_not_dial_is_never_spawned_over() {
        let tmp = private_dir();
        let path = tmp.path().join("a".repeat(200));

        let err = connect_or_spawn_daemon(&path, Offer::ops())
            .await
            .err()
            .expect("a path no socket can carry is not a free endpoint");

        let SpawnConnectError::Connect(err) = err else {
            panic!("expected a connect error, got: {err}");
        };
        assert!(
            matches!(&err, ConnectError::Connect(io_err)
            if !matches!(
                io_err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )),
            "got: {err}"
        );
        assert!(!err.may_be_a_cold_socket(), "got: {err}");
    }

    /// A listener another uid serves is not this uid's daemon: the dial
    /// ends before the preface, and a peer-identity failure is not the
    /// absence that licenses a spawn (REQ-009c), so the autospawn
    /// returns it instead of starting a second daemon beside a live
    /// one. The expectation is injected: a test has one account.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_listener_of_another_uid_is_returned_and_never_spawned_over() {
        let tmp = private_dir();
        let path = tmp.path().join("daemon.sock");
        let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");
        let server_task = tokio::spawn(async move {
            let stream = server.accept().await.expect("accept");
            std::future::pending::<()>().await;
            drop(stream);
        });
        let own = rustix::process::geteuid().as_raw();
        let bogus = if own == 0 { 65534 } else { 0 };

        let err = felis_transport::local::connect_expecting(path.as_path(), bogus)
            .await
            .expect_err("a foreign listener is not this uid's daemon");

        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        assert!(err.to_string().contains("does not match"), "{err}");
        assert!(
            !ConnectError::Connect(err).may_be_a_cold_socket(),
            "a foreign listener holds the endpoint; spawning would put a second daemon beside it"
        );
        server_task.abort();
    }

    /// The other half of the rule: once the connect succeeded, a peer
    /// that vanished mid-handshake is the stale-socket race autospawn
    /// exists for, and the retry has to stay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_daemon_that_vanished_after_the_accept_still_licenses_a_spawn() {
        let tmp = private_dir();
        let path = tmp.path().join("vanishing.sock");
        let server = Listener::bind(&Endpoint::unix(path.clone())).expect("bind");
        let server_task = tokio::spawn(async move {
            drop(server.accept().await.expect("accept"));
            std::future::pending::<()>().await;
        });

        let err = connect_carrier(
            Carrier::Local(path.into()),
            Offer::ops(),
            RemoteSpawn::Allow,
        )
        .await
        .err()
        .expect("a peer that closed mid-handshake cannot answer");
        assert!(
            !matches!(err, ConnectError::Connect(_)),
            "the raw connect succeeded: {err}"
        );
        assert!(
            err.may_be_a_cold_socket(),
            "a socket a dead daemon left behind is replaceable: {err}"
        );
        server_task.abort();
    }

    /// The kinds, on the error the autospawn sites consult.
    #[test]
    fn only_a_not_found_or_refused_raw_connect_licenses_a_spawn() {
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::ConnectionRefused,
        ] {
            assert!(
                ConnectError::Connect(std::io::Error::from(kind)).may_be_a_cold_socket(),
                "{kind:?}"
            );
        }
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::TimedOut,
        ] {
            assert!(
                !ConnectError::Connect(std::io::Error::from(kind)).may_be_a_cold_socket(),
                "{kind:?}"
            );
        }
        assert!(
            !ConnectError::Connect(std::io::Error::from_raw_os_error(24)).may_be_a_cold_socket(),
            "EMFILE is not a cold socket"
        );
        // A peer that answered and then vanished keeps its verdict: the
        // socket may be a stale file a dead daemon left behind.
        assert!(ConnectError::EofBeforeWelcome.may_be_a_cold_socket());
    }

    /// The hand-off's own dial follows the same rule: a manager that
    /// left the endpoint undialable is not a reason to fork beside it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_managed_start_that_left_the_endpoint_undialable_does_not_fork() {
        let undialable = [
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            // EMFILE: the process ran out of descriptors, which says
            // nothing about what holds the endpoint.
            std::io::Error::from_raw_os_error(24),
            std::io::Error::from(std::io::ErrorKind::TimedOut),
        ];
        for err in undialable {
            let reported = format!("{err}");
            let fallback = managed_failure(ConnectError::Connect(err), || "cold".to_owned());
            assert!(
                matches!(fallback, systemd::Fallback::Undialable(_)),
                "{reported}: {fallback:?}"
            );
        }
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::ConnectionRefused,
        ] {
            let cold = managed_failure(ConnectError::Connect(std::io::Error::from(kind)), || {
                "cold".to_owned()
            });
            assert!(
                matches!(cold, systemd::Fallback::Failed(_)),
                "{kind:?}: {cold:?}"
            );
        }
    }

    /// The refusal that is *retryable* is still an answer from a live
    /// daemon: spawning a second one over it would fail its bind and
    /// leave the caller with "unreachable" instead of "full".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_full_daemon_is_reported_at_capacity_instead_of_spawned_over() {
        use felis_daemon::{
            SessionPool, serve::ConnectionAdmission, serve::DaemonCaps, serve_unix,
        };
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let tmp = private_dir();
        let path = tmp.path().join("full.sock");
        let caps = DaemonCaps {
            admission: ConnectionAdmission::with_refusal_slots(1, 4),
            ..DaemonCaps::default()
        };
        let server_path = path.clone();
        let server_task = tokio::spawn(async move {
            drop(serve_unix(&server_path, caps, Arc::new(Mutex::new(SessionPool::new()))).await);
        });
        for _ in 0..200 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let held = connect_or_spawn_daemon(&path, Offer::ops())
            .await
            .expect("the first dial takes the daemon's only permit");

        let started = std::time::Instant::now();
        let err = connect_or_spawn_daemon(&path, Offer::ops())
            .await
            .err()
            .expect("a full daemon must not be autospawned over");
        let SpawnConnectError::Connect(err) = err else {
            panic!("expected a connect error, got: {err}");
        };
        assert!(
            err.at_capacity().is_some(),
            "the caller must be told the daemon is full: {err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must fail fast, not sit out the boot-retry window"
        );

        drop(held);
        server_task.abort();
    }
}

//! Dial-and-land helpers: open a fresh daemon connection and attach to
//! (or create) a session in one round trip.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use felis_protocol::messages::{RetargetCarrier, RetargetTarget, SpawnArgs};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::connector::{
    AttachIntent, Carrier, CarrierConnection, ConnectError, Connection, Offer, RemoteSpawn,
    connect_carrier,
};
use crate::roster::{RingKey, pick_exit_switch_target};
use crate::spawn::SpawnConnectError;

/// A window landing on a daemon is what spawns one, over either
/// carrier: a launch and a retarget's landing both mean "hold this
/// session for me" (docs/reference/cli.md "Auto-spawning"). On a cold
/// local socket only [`Reconnector::dial_launch`] spawns, so a
/// [`dial_and_land`] landing there fails instead.
pub const LANDING_REMOTE_SPAWN: RemoteSpawn = RemoteSpawn::Allow;

#[derive(Debug, Error)]
pub enum DialError {
    #[error("could not reach felis-daemon at {socket} after auto-spawn")]
    SpawnDaemon {
        socket: String,
        #[source]
        source: SpawnConnectError,
    },
    #[error("connect to remote felis-daemon over ssh stdio")]
    SshStdio(#[source] ConnectError),
    #[error("open daemon connection for {intent}")]
    Connect {
        intent: DialIntent,
        #[source]
        source: ConnectError,
    },
    #[error("query session roster for {intent}")]
    Roster {
        intent: DialIntent,
        #[source]
        source: ConnectError,
    },
    #[error("session roster query for {intent} went unanswered for {}s", timeout.as_secs())]
    RosterTimeout {
        intent: DialIntent,
        timeout: Duration,
    },
    #[error("reconnect attempt went unanswered for {}s", timeout.as_secs())]
    AttemptTimeout { timeout: Duration },
    #[error("attach session {id:#x}")]
    Attach {
        id: u128,
        #[source]
        source: ConnectError,
    },
    #[error("create new session on daemon")]
    Create(#[source] ConnectError),
    /// Not a dial failure: the caller reaches the end of the exit ladder
    /// rather than retrying.
    #[error("no live session left to open")]
    NoLiveSession,
    #[error("attach session `{prefix}` on target daemon")]
    AttachPrefixOnTarget {
        prefix: String,
        #[source]
        source: ConnectError,
    },
    #[error("create session on target daemon")]
    CreateOnTarget(#[source] ConnectError),
}

impl DialError {
    /// The one failure class a fresh roster answers (a re-pick names a
    /// different session); the caller's single retry is spent only here.
    #[must_use]
    pub const fn target_vanished(&self) -> bool {
        matches!(
            self,
            Self::Attach {
                source: ConnectError::AttachFailed { reason, .. },
                ..
            } if matches!(
                reason,
                felis_protocol::messages::AttachFailure::UnknownSession
                    | felis_protocol::messages::AttachFailure::SessionEnding
                    | felis_protocol::messages::AttachFailure::SessionExited
            )
        )
    }

    /// Whether a window's reconnect ladder is worth another sleep: a
    /// verdict the daemon stated reads the same on every attempt.
    #[must_use]
    pub const fn reconnect_retryable(&self) -> bool {
        match self {
            Self::SshStdio(source)
            | Self::Connect { source, .. }
            | Self::Roster { source, .. }
            | Self::Attach { source, .. }
            | Self::AttachPrefixOnTarget { source, .. }
            | Self::Create(source)
            | Self::CreateOnTarget(source) => source.is_transient(),
            Self::RosterTimeout { .. } | Self::AttemptTimeout { .. } => true,
            // The autospawn's own boot-retry window does not cover the
            // one refusal it never spawns over: a full daemon answers
            // differently once a peer leaves, so the ladder keeps it.
            Self::SpawnDaemon { source, .. } => match source {
                SpawnConnectError::Connect(source) => source.is_transient(),
                SpawnConnectError::Spawn(_) => false,
            },
            Self::NoLiveSession => false,
        }
    }
}

/// Why a window's reconnect ladder stopped. [`Self::Refused`] and
/// [`Self::Exhausted`] are terminal: the window closes and names the
/// remedy. [`Self::SessionGone`] is the fact `PushMsg::SessionExited`
/// carries, so the window takes the exit ladder instead.
#[derive(Debug, Error)]
pub enum ReconnectError {
    #[error("the daemon no longer knows this session")]
    SessionGone(#[source] DialError),
    #[error("the daemon refused this window")]
    Refused(#[source] DialError),
    #[error("no answer after {attempts} reconnect attempt(s)")]
    Exhausted {
        attempts: u32,
        #[source]
        source: DialError,
    },
}

/// Re-dial the carrier and session the window lost, and nothing else,
/// live-only (`architecture/session-lifecycle.md` "Transport loss").
///
/// # Errors
/// [`ReconnectError`] once the ladder is spent or refused.
pub async fn redial_session(
    carrier: Carrier,
    offer: Offer,
    id: u128,
) -> Result<DialedConnection, ReconnectError> {
    let reconnector = Reconnector { carrier, offer };
    felis_transport::retry::retry_while(
        || async {
            let landing = Landing::Attach {
                id,
                intent: AttachIntent::Automatic,
            };
            // Through `dial_launch`, not `connect_carrier`: a dead local
            // daemon has to be replaced before an attach can report the
            // session gone, or every rung would fail at the socket and
            // the ladder would end in exhaustion saying nothing about
            // the session.
            let attempt = async {
                let conn = reconnector.dial_launch(LANDING_REMOTE_SPAWN).await?;
                land_on(conn, landing, DialIntent::Switch, offer).await
            };
            match tokio::time::timeout(RECONNECT_ATTEMPT_TIMEOUT, attempt).await {
                Ok(landed) => landed,
                Err(_elapsed) => Err(DialError::AttemptTimeout {
                    timeout: RECONNECT_ATTEMPT_TIMEOUT,
                }),
            }
        },
        felis_transport::retry::RetryPolicy::WINDOW_RECONNECT,
        DialError::reconnect_retryable,
    )
    .await
    .map_err(|err| classify_reconnect(err.attempts, err.source))
}

/// A bound on one rung of the ladder: the policy only sums the sleeps
/// *between* attempts, and the connect step under it spawns a fresh
/// `ssh` child, which can sit on a half-open route for as long as the
/// kernel lets it. Without this the window never reaches a terminal
/// state at all.
pub const RECONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

#[must_use]
const fn classify_reconnect(attempts: u32, source: DialError) -> ReconnectError {
    if source.target_vanished() {
        return ReconnectError::SessionGone(source);
    }
    if source.reconnect_retryable() {
        return ReconnectError::Exhausted { attempts, source };
    }
    ReconnectError::Refused(source)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialIntent {
    Switch,
    Spawn,
    Retarget,
}

impl fmt::Display for DialIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Switch => "session switch",
            Self::Spawn => "session spawn",
            Self::Retarget => "host retarget",
        })
    }
}

/// The daemon's pool model is one connection per attached-session
/// lifetime, so every switch is a fresh handshake: a new `ssh` child
/// on [`Carrier::Ssh`], which is why a remote window wants
/// `ControlMaster` (docs/how-to/attach-over-ssh.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconnector {
    pub carrier: Carrier,
    pub offer: Offer,
}

impl Reconnector {
    /// The spawning half of the auto-spawn matrix
    /// (docs/reference/cli.md "Auto-spawning"): the caller's policy is
    /// what the SSH relay is asked for. Only the local carrier
    /// autospawns in-process, since over SSH the remote `felis-daemon
    /// relay` owns the autospawn and its boot-retry window.
    pub async fn dial_launch(
        &self,
        remote_spawn: RemoteSpawn,
    ) -> Result<CarrierConnection, DialError> {
        match self.carrier.local_socket() {
            Some(socket) => crate::spawn::connect_or_spawn_daemon(&socket, self.offer)
                .await
                .map_err(|source| DialError::SpawnDaemon {
                    socket: socket.display().to_string(),
                    source,
                }),
            None => connect_carrier(self.carrier.clone(), self.offer, remote_spawn)
                .await
                .map_err(DialError::SshStdio),
        }
    }
}

/// An empty vector yields `SpawnArgs::default()`, whose empty `command`
/// makes the daemon's factory pick `$SHELL`.
#[must_use]
pub fn spawn_args_from_cli(command: Vec<String>) -> SpawnArgs {
    let mut it = command.into_iter();
    match it.next() {
        Some(program) => SpawnArgs {
            command: program,
            args: it.collect(),
            ..Default::default()
        },
        None => SpawnArgs::default(),
    }
}

pub(crate) fn local_launch_args(command: Vec<String>) -> Result<SpawnArgs, LaunchCwdError> {
    Ok(SpawnArgs {
        cwd: launch_cwd()?,
        ..spawn_args_from_cli(command)
    })
}

/// No cwd for a remote daemon: it chdirs into whatever cwd it is sent,
/// so a local path shipped across fails the spawn outright.
pub fn launch_args(command: Vec<String>, carrier: &Carrier) -> Result<SpawnArgs, LaunchCwdError> {
    let args = if matches!(carrier, Carrier::Local(_)) {
        local_launch_args(command)?
    } else {
        spawn_args_from_cli(command)
    };
    Ok(crate::env_base::fill_for_carrier(args, carrier))
}

/// The launch cwd the window would send is not UTF-8, which
/// `SpawnArgs.cwd` cannot carry (docs/reference/ipc.md "Session
/// (kind = 4)").
#[derive(Debug, Error)]
#[error("the launch directory `{}` is not valid UTF-8", .0.display())]
pub struct LaunchCwdError(PathBuf);

/// Not left empty: an empty `SpawnArgs.cwd` inherits the daemon's cwd,
/// frozen at auto-spawn, and macOS launches GUI apps at `/`, so every
/// fresh window would land there. `/` itself counts as no cwd for the
/// same reason.
pub(crate) fn launch_cwd() -> Result<String, LaunchCwdError> {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_owned());
    resolve_launch_cwd(std::env::current_dir().ok(), home)
}

pub(crate) fn resolve_launch_cwd(
    current: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<String, LaunchCwdError> {
    let Some(chosen) = current.filter(|p| p != std::path::Path::new("/")).or(home) else {
        return Ok(String::new());
    };
    // Not `to_string_lossy`, and not a fallback to the next candidate
    // either: the daemon chdirs into what it is sent, so a window that
    // quietly moved on would run the command somewhere else.
    if let Some(text) = chosen.to_str() {
        return Ok(text.to_owned());
    }
    Err(LaunchCwdError(chosen))
}

/// A bound on the request, not the connection: a daemon can keep
/// streaming grid frames and still never answer `List`, which would
/// leave a switch chord waiting for the life of the connection.
/// Five seconds matches `felis sessions`' reply budget.
pub const ROSTER_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn fetch_roster<R, W>(
    connection: &mut Connection<R, W>,
) -> Result<Vec<felis_protocol::messages::SessionInfo>, ConnectError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    connection.list_sessions().await
}

pub struct DialedConnection {
    pub conn: CarrierConnection,
    pub attach: felis_protocol::messages::SessionInfo,
    pub sessions: Vec<felis_protocol::messages::SessionInfo>,
    pub pull_enabled: bool,
}

pub enum Landing {
    Attach {
        id: u128,
        intent: AttachIntent,
    },
    /// An attach whose target is already known by id, so the roster
    /// round trip [`Landing::Attach`] spends is skipped: the exit
    /// ladder's trail entries name a session outright, and a dead one
    /// must cost one connect and one refusal rather than that plus a
    /// roster fetch and its own timeout.
    AttachExact {
        id: u128,
        intent: AttachIntent,
    },
    Create(SpawnArgs),
    /// The shell-exit pick runs on this dial's connection, not the
    /// window's own: the shell exiting is what kills that one, so a
    /// fetch on it would be answered by a daemon already tearing the
    /// subscriber down.
    PickExit {
        anchor: RingKey,
    },
    /// A `--session` prefix, resolved by the daemon at attach time.
    /// The roster fetch stays for the window's switch ring, which the
    /// prefix itself does not need.
    ResolveOnTarget(String),
    CreateOnTarget(SpawnArgs),
}

impl Landing {
    #[must_use]
    fn with_env_base_for(self, carrier: &Carrier) -> Self {
        match self {
            Self::Create(args) => Self::Create(crate::env_base::fill_for_carrier(args, carrier)),
            Self::CreateOnTarget(args) => {
                Self::CreateOnTarget(crate::env_base::fill_for_carrier(args, carrier))
            }
            other => other,
        }
    }

    /// Whether only the connect step may be cut short; see
    /// [`dial_and_land_within`].
    const fn bounds_connect_only(&self) -> bool {
        matches!(self, Self::Create(_) | Self::CreateOnTarget(_))
    }

    const fn intent(&self) -> DialIntent {
        match self {
            Self::Attach { .. } | Self::AttachExact { .. } | Self::PickExit { .. } => {
                DialIntent::Switch
            }
            Self::Create(_) => DialIntent::Spawn,
            Self::ResolveOnTarget(_) | Self::CreateOnTarget(_) => DialIntent::Retarget,
        }
    }
}

pub async fn dial_and_land(
    carrier: Carrier,
    offer: Offer,
    landing: Landing,
) -> Result<DialedConnection, DialError> {
    let intent = landing.intent();
    // The executing window's environment, not the requester's: a retarget
    // can be typed from a session on a third host.
    let landing = landing.with_env_base_for(&carrier);
    let conn = connect_carrier(carrier, offer, LANDING_REMOTE_SPAWN)
        .await
        .map_err(|source| DialError::Connect { intent, source })?;
    land_on(conn, landing, intent, offer).await
}

/// [`dial_and_land`] under a caller-supplied bound. A creating landing
/// bounds the connect step alone: the daemon may already have created
/// the session, and dropping the connection then orphans it.
/// # Errors
/// [`DialError::AttemptTimeout`] when the bounded phase outlives `budget`.
pub async fn dial_and_land_within(
    carrier: Carrier,
    offer: Offer,
    landing: Landing,
    budget: Duration,
) -> Result<DialedConnection, DialError> {
    let intent = landing.intent();
    let landing = landing.with_env_base_for(&carrier);
    let connect = async {
        connect_carrier(carrier, offer, LANDING_REMOTE_SPAWN)
            .await
            .map_err(|source| DialError::Connect { intent, source })
    };
    if landing.bounds_connect_only() {
        let conn = bounded(budget, connect).await??;
        return land_on(conn, landing, intent, offer).await;
    }
    bounded(budget, async {
        let conn = connect.await?;
        land_on(conn, landing, intent, offer).await
    })
    .await?
}

async fn bounded<T>(budget: Duration, work: impl Future<Output = T>) -> Result<T, DialError> {
    tokio::time::timeout(budget, work)
        .await
        .map_err(|_elapsed| DialError::AttemptTimeout { timeout: budget })
}

/// The half of [`dial_and_land`] past the connect step, so the
/// reconnect ladder can reach it over a carrier it opened its own way.
async fn land_on(
    mut conn: CarrierConnection,
    landing: Landing,
    intent: DialIntent,
    offer: Offer,
) -> Result<DialedConnection, DialError> {
    if let Landing::AttachExact { id, intent } = landing {
        let attach = conn
            .attach(id, intent)
            .await
            .map_err(|source| DialError::Attach { id, source })?;
        return Ok(DialedConnection {
            conn,
            attach,
            sessions: Vec::new(),
            pull_enabled: offer.pull_paced,
        });
    }
    let queried = match tokio::time::timeout(ROSTER_FETCH_TIMEOUT, fetch_roster(&mut conn)).await {
        Ok(Ok(sessions)) => Ok(sessions),
        Ok(Err(source)) => Err(DialError::Roster { intent, source }),
        Err(_elapsed) => Err(DialError::RosterTimeout {
            intent,
            timeout: ROSTER_FETCH_TIMEOUT,
        }),
    };
    let sessions = match queried {
        Ok(sessions) => sessions,
        // Not an empty roster where the landing is decided by it: that
        // would read as a phantom no-match, closing the window over a
        // query error. Every other landing only mirrors the roster.
        Err(err) if matches!(landing, Landing::PickExit { .. }) => {
            return Err(err);
        }
        Err(err) => {
            tracing::warn!(?err, "Ops::List failed; starting with an empty roster");
            Vec::new()
        }
    };
    let attach = match landing {
        Landing::Attach { id, intent } => conn
            .attach(id, intent)
            .await
            .map_err(|source| DialError::Attach { id, source })?,
        Landing::Create(args) => conn.create_with(args).await.map_err(DialError::Create)?,
        // Handled above, before the roster fetch this arm follows.
        Landing::AttachExact { .. } => unreachable!("the exact attach returns before the fetch"),
        Landing::PickExit { anchor } => {
            let id = pick_exit_switch_target(&sessions, anchor).ok_or(DialError::NoLiveSession)?;
            // Automatic: the pick can exit between this roster and the
            // attach, and a window landed on the corpse is never pushed
            // off it.
            conn.attach(id, AttachIntent::Automatic)
                .await
                .map_err(|source| DialError::Attach { id, source })?
        }
        // Deliberate throughout: a typed prefix may land on a
        // just-exited session's final screen.
        Landing::ResolveOnTarget(prefix) => {
            let reported = prefix.clone();
            conn.attach_by_prefix(prefix, AttachIntent::Deliberate)
                .await
                .map_err(|source| DialError::AttachPrefixOnTarget {
                    prefix: reported,
                    source,
                })?
        }
        Landing::CreateOnTarget(args) => conn
            .create_with(args)
            .await
            .map_err(DialError::CreateOnTarget)?,
    };
    let pull_enabled = offer.pull_paced;
    Ok(DialedConnection {
        conn,
        attach,
        sessions,
        pull_enabled,
    })
}

/// `default_socket` is the receiving client's, not the sending daemon's,
/// and `None` is a client with none: only a [`RetargetCarrier::DefaultLocal`]
/// target needs it. Only the SSH carrier declines pull pacing, since a
/// per-vsync round trip over a high-latency link starves the stream.
#[must_use]
pub fn reconnector_for_target(
    target: &RetargetTarget,
    default_socket: Option<PathBuf>,
) -> Option<Reconnector> {
    Some(match &target.carrier {
        RetargetCarrier::DefaultLocal => Reconnector {
            carrier: Carrier::Local(default_socket?.into()),
            offer: Offer::window(true),
        },
        RetargetCarrier::LocalEndpoint(path) => Reconnector {
            carrier: Carrier::Local(PathBuf::from(path).into()),
            offer: Offer::window(true),
        },
        RetargetCarrier::Ssh {
            destination,
            ssh_args,
        } => Reconnector {
            carrier: Carrier::Ssh {
                destination: destination.clone(),
                ssh_args: ssh_args.clone(),
            },
            offer: Offer::window(false),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket parent must be a `0700` directory this uid owns
    /// (REQ-107), and `TempDir` follows the process umask.
    #[cfg(unix)]
    fn private_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        tmp
    }

    use felis_protocol::messages::AttachFailure;
    use felis_transport::retry::RetryPolicy;

    fn attach_refused(reason: AttachFailure) -> DialError {
        DialError::Attach {
            id: 0x7,
            source: ConnectError::AttachFailed {
                reason,
                detail: String::new(),
            },
        }
    }

    /// A window that keeps re-dialing a session the daemon has an
    /// answer about spends its whole budget learning the same thing.
    #[test]
    fn a_stated_verdict_ends_the_reconnect_ladder_and_a_dead_carrier_does_not() {
        for reason in [
            AttachFailure::UnknownSession,
            AttachFailure::SessionEnding,
            AttachFailure::SessionExited,
        ] {
            let err = attach_refused(reason);
            assert!(
                !err.reconnect_retryable(),
                "{reason:?} is the daemon's answer"
            );
            assert!(matches!(
                classify_reconnect(2, err),
                ReconnectError::SessionGone(_)
            ));
        }

        let refused = DialError::Connect {
            intent: DialIntent::Switch,
            source: ConnectError::MajorMismatch {
                client_major: 1,
                daemon_min: 2,
                daemon_max: 2,
            },
        };
        assert!(!refused.reconnect_retryable());
        assert!(matches!(
            classify_reconnect(1, refused),
            ReconnectError::Refused(_)
        ));

        let carrier_down = DialError::Connect {
            intent: DialIntent::Switch,
            source: ConnectError::EofBeforeWelcome,
        };
        assert!(carrier_down.reconnect_retryable());
        assert!(matches!(
            classify_reconnect(6, carrier_down),
            ReconnectError::Exhausted { attempts: 6, .. }
        ));
    }

    /// The autospawn path reports both a daemon that could not be
    /// started and a daemon that answered "full"; only the first is the
    /// ladder's business to give up on.
    #[test]
    fn an_autospawn_failure_is_terminal_only_when_the_daemon_did_not_answer() {
        let at_capacity = DialError::SpawnDaemon {
            socket: "/run/felis.sock".to_owned(),
            source: SpawnConnectError::Connect(ConnectError::Refused {
                reason: felis_protocol::messages::RefusalReason::AtCapacity,
                detail: String::new(),
            }),
        };
        assert!(
            at_capacity.reconnect_retryable(),
            "a full daemon answers differently once a peer leaves"
        );

        let no_binary = DialError::SpawnDaemon {
            socket: "/run/felis.sock".to_owned(),
            source: SpawnConnectError::Spawn(std::io::Error::from(std::io::ErrorKind::NotFound)),
        };
        assert!(
            !no_binary.reconnect_retryable(),
            "no felis-daemon to start reads the same on every attempt"
        );
        assert!(matches!(
            classify_reconnect(1, no_binary),
            ReconnectError::Refused(_)
        ));
    }

    /// The window is unusable while the ladder runs, so the schedule is
    /// bounded in both attempts and wall-clock wait.
    #[test]
    fn the_reconnect_ladder_is_bounded() {
        let policy = RetryPolicy::WINDOW_RECONNECT;
        assert_eq!(policy.max_attempts.get(), 6);
        assert!(
            policy.total_backoff() <= Duration::from_secs(30),
            "the whole ladder must fit in half a minute: {:?}",
            policy.total_backoff()
        );
        assert!(
            policy.initial_backoff >= Duration::from_secs(1),
            "a retry that spawns an ssh child must not hammer the host"
        );
    }

    /// A carrier that accepts the connection and then says nothing has
    /// no error for the ladder to classify, so only the per-attempt
    /// bound can bring the window to a terminal state.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_carrier_that_never_answers_still_ends_the_ladder() {
        let tmp = private_dir();
        let path = tmp.path().join("mute.sock");
        let server =
            felis_transport::local::Listener::bind(&felis_transport::Endpoint::unix(path.clone()))
                .expect("bind the mute carrier");
        let _accepting = tokio::spawn(async move {
            while let Ok(stream) = server.accept().await {
                tokio::spawn(async move {
                    std::future::pending::<()>().await;
                    drop(stream);
                });
            }
        });

        let err = redial_session(Carrier::Local(path.into()), Offer::window(true), 0x7)
            .await
            .err()
            .expect("a carrier that never answers cannot land");
        assert!(
            matches!(err, ReconnectError::Exhausted { attempts: 6, .. }),
            "got: {err}"
        );
    }

    /// The landing a window makes on a cold *local* socket is the one
    /// non-spawning half of the launch policy: `dial_and_land` opens
    /// the socket directly, so a retarget to a host with no daemon
    /// fails instead of starting one (docs/reference/cli.md
    /// "Auto-spawning").
    #[cfg(unix)]
    #[tokio::test]
    async fn a_landing_on_a_cold_local_socket_fails_without_spawning_a_daemon() {
        let dir = private_dir();
        let socket = dir.path().join("felis.sock");
        let err = dial_and_land(
            Carrier::Local(socket.clone().into()),
            Offer::ops(),
            Landing::Create(SpawnArgs::default()),
        )
        .await;
        let Err(err) = err else {
            panic!("a cold socket has no daemon to land on");
        };
        assert!(
            matches!(err, DialError::Connect { .. }),
            "expected a connect failure, got {err:?}"
        );
        assert!(
            !socket.exists(),
            "no daemon may be started behind the caller's back"
        );
    }

    /// Only a landing that may already have made a session on the far
    /// side is exempt from the whole-dial bound; every attaching form is
    /// wrapped end to end.
    #[test]
    fn only_a_creating_landing_bounds_the_connect_step_alone() {
        assert!(Landing::Create(SpawnArgs::default()).bounds_connect_only());
        assert!(Landing::CreateOnTarget(SpawnArgs::default()).bounds_connect_only());
        for landing in [
            Landing::Attach {
                id: 7,
                intent: AttachIntent::Automatic,
            },
            Landing::AttachExact {
                id: 7,
                intent: AttachIntent::Deliberate,
            },
            Landing::PickExit {
                anchor: RingKey {
                    sequence: std::num::NonZeroU64::MIN,
                    id: 7,
                },
            },
            Landing::ResolveOnTarget("ab12".into()),
        ] {
            assert!(!landing.bounds_connect_only());
        }
    }

    /// A rung that never answers has to consume its budget and hand the
    /// ladder back, naming the bound it was given rather than the
    /// reconnect ladder's own.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_bounded_dial_gives_up_on_the_budget_it_was_handed() {
        let tmp = private_dir();
        let path = tmp.path().join("mute.sock");
        let server =
            felis_transport::local::Listener::bind(&felis_transport::Endpoint::unix(path.clone()))
                .expect("bind the mute carrier");
        let _accepting = tokio::spawn(async move {
            while let Ok(stream) = server.accept().await {
                tokio::spawn(async move {
                    std::future::pending::<()>().await;
                    drop(stream);
                });
            }
        });

        let budget = Duration::from_millis(80);
        let err = dial_and_land_within(
            Carrier::Local(path.into()),
            Offer::window(true),
            Landing::AttachExact {
                id: 0x7,
                intent: AttachIntent::Automatic,
            },
            budget,
        )
        .await
        .err()
        .expect("a carrier that never answers cannot land");
        assert!(
            matches!(err, DialError::AttemptTimeout { timeout } if timeout == budget),
            "got: {err}"
        );
    }

    /// The trail names its session outright, so the roster round trip
    /// `Landing::Attach` spends is skipped: a dead entry must cost one
    /// connect and one refusal, not a listing and its own timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_exact_attach_lands_without_listing_the_roster() {
        use felis_daemon::serve::{SessionFactory, serve_unix_with_factory};

        let tmp = private_dir();
        let path = tmp.path().join("daemon.sock");
        let pool = std::sync::Arc::new(tokio::sync::Mutex::new(felis_daemon::SessionPool::new()));
        let factory: SessionFactory = std::sync::Arc::new(|_| {
            let mut cmd = felis_pty::Command::new("/bin/sh");
            cmd.args(["-c", "sleep 30"]);
            cmd.env_clear();
            cmd.env("PATH", "/bin:/usr/bin");
            cmd
        });
        let server_path = path.clone();
        let _server = tokio::spawn(async move {
            drop(
                serve_unix_with_factory(
                    &server_path,
                    felis_daemon::serve::DaemonCaps::default(),
                    pool,
                    factory,
                )
                .await,
            );
        });
        for _ in 0..200 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(path.exists(), "the test daemon must come up");

        let carrier = Carrier::Local(path.into());
        let created = dial_and_land(
            carrier.clone(),
            Offer::window(true),
            Landing::Create(SpawnArgs::default()),
        )
        .await
        .expect("create a session to attach to");
        let id = created.attach.id;
        drop(created);

        let listed = dial_and_land(
            carrier.clone(),
            Offer::window(true),
            Landing::Attach {
                id,
                intent: AttachIntent::Deliberate,
            },
        )
        .await
        .expect("the ordinary attach lands");
        assert!(
            !listed.sessions.is_empty(),
            "the ordinary attach spends a roster round trip"
        );
        drop(listed);

        let exact = dial_and_land(
            carrier,
            Offer::window(true),
            Landing::AttachExact {
                id,
                intent: AttachIntent::Deliberate,
            },
        )
        .await
        .expect("the exact attach lands");
        assert_eq!(exact.attach.id, id);
        assert!(
            exact.sessions.is_empty(),
            "the exact attach must not have asked for a roster"
        );
    }

    #[test]
    fn spawn_args_from_cli_splits_program_and_args() {
        let empty = spawn_args_from_cli(Vec::new());
        assert_eq!(empty, SpawnArgs::default());
        assert_eq!(empty.command, "");

        let one = spawn_args_from_cli(vec!["htop".to_string()]);
        assert_eq!(one.command, "htop");
        assert_eq!(one.args, Vec::<String>::new());

        let many =
            spawn_args_from_cli(vec!["bash".to_string(), "-c".to_string(), "ls".to_string()]);
        assert_eq!(many.command, "bash");
        assert_eq!(many.args, vec!["-c", "ls"]);
    }

    /// Real cwd, else `$HOME` (also for `/`), else empty so the daemon's
    /// cwd applies.
    #[test]
    fn resolve_launch_cwd_prefers_real_cwd_then_home() {
        let home = Some(PathBuf::from("/home/me"));
        let resolved = |current, home| {
            resolve_launch_cwd(current, home).expect("a UTF-8 launch directory resolves")
        };

        assert_eq!(
            resolved(Some(PathBuf::from("/home/me/proj")), home.clone()),
            "/home/me/proj"
        );
        assert_eq!(resolved(Some(PathBuf::from("/")), home.clone()), "/home/me");
        assert_eq!(resolved(None, home), "/home/me");
        assert_eq!(resolved(Some(PathBuf::from("/")), None), "");
        assert_eq!(resolved(None, None), "");
    }

    /// The chosen directory is refused rather than skipped: falling
    /// through to `$HOME` would open the window somewhere else.
    #[cfg(unix)]
    #[test]
    fn resolve_launch_cwd_refuses_a_non_utf8_directory() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let undecodable = PathBuf::from(OsString::from_vec(b"/tmp/\xff\xfe".to_vec()));
        let home = Some(PathBuf::from("/home/me"));

        assert!(resolve_launch_cwd(Some(undecodable.clone()), home).is_err());
        assert!(resolve_launch_cwd(Some(undecodable.clone()), None).is_err());
        // Only the chosen one matters: a `/` cwd picks `$HOME` and a
        // non-UTF-8 `$HOME` is refused in its turn.
        assert!(resolve_launch_cwd(Some(PathBuf::from("/")), Some(undecodable)).is_err());
    }

    #[test]
    fn each_retarget_carrier_selects_its_own_dial() {
        let default_socket = PathBuf::from("/run/user/1000/felis/daemon.sock");
        let target = |carrier: RetargetCarrier| RetargetTarget {
            carrier,
            landing: felis_protocol::messages::RetargetLanding::Create(SpawnArgs::default()),
        };
        let for_carrier = |carrier: RetargetCarrier| {
            reconnector_for_target(&target(carrier), Some(default_socket.clone()))
                .expect("a resolvable socket lands every carrier")
        };

        assert!(
            reconnector_for_target(&target(RetargetCarrier::DefaultLocal), None).is_none(),
            "the default local carrier is the only one that needs the socket"
        );
        assert!(
            reconnector_for_target(
                &target(RetargetCarrier::Ssh {
                    destination: "devbox".into(),
                    ssh_args: Vec::new(),
                }),
                None,
            )
            .is_some(),
            "a target that names its own endpoint lands without one"
        );

        let default_local = for_carrier(RetargetCarrier::DefaultLocal);
        assert_eq!(
            default_local.carrier.local_socket(),
            Some(default_socket.clone())
        );
        assert!(default_local.offer.pull_paced);

        let explicit = for_carrier(RetargetCarrier::LocalEndpoint("/tmp/alt.sock".into()));
        assert_eq!(
            explicit.carrier.local_socket(),
            Some(PathBuf::from("/tmp/alt.sock"))
        );
        assert!(explicit.offer.pull_paced);

        let ssh = for_carrier(RetargetCarrier::Ssh {
            destination: "user@devbox".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        });
        assert!(matches!(
            ssh.carrier,
            Carrier::Ssh { ref destination, ref ssh_args }
                if destination == "user@devbox" && ssh_args == &["-p", "2222"]
        ));
        assert!(!ssh.offer.pull_paced);
    }

    #[test]
    fn a_creating_landing_captures_at_the_dial_that_lands_it() {
        let local = Carrier::Local(PathBuf::from("/tmp/felis.sock").into());
        let ssh = Carrier::Ssh {
            destination: "devbox".into(),
            ssh_args: Vec::new(),
        };

        for landing in [
            Landing::Create(SpawnArgs::default()),
            Landing::CreateOnTarget(SpawnArgs::default()),
        ] {
            let carried = match landing.with_env_base_for(&local) {
                Landing::Create(args) | Landing::CreateOnTarget(args) => args.env_base,
                _ => panic!("a creating landing stays a creating landing"),
            };
            assert!(
                carried.is_some_and(|base| !base.is_empty()),
                "the dialing window's environment rides a local landing"
            );
        }

        let over_ssh = match Landing::CreateOnTarget(SpawnArgs::default()).with_env_base_for(&ssh) {
            Landing::CreateOnTarget(args) => args.env_base,
            _ => panic!("a creating landing stays a creating landing"),
        };
        assert_eq!(over_ssh, None);
    }

    #[test]
    fn an_attaching_landing_is_left_alone() {
        let local = Carrier::Local(PathBuf::from("/tmp/felis.sock").into());
        assert!(matches!(
            Landing::Attach {
                id: 7,
                intent: AttachIntent::Deliberate,
            }
            .with_env_base_for(&local),
            Landing::Attach { id: 7, .. }
        ));
    }
}

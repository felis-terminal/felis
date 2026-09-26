//! [`SessionToDaemonMsg`] and [`SessionToClientMsg`]: this
//! connection's session lifecycle (kind 4).

use serde::{Deserialize, Serialize};

use super::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet, PhaseSet};

use super::RequestedDims;

/// The client's half of the session-lifecycle family
/// ([`crate::MessageKind::Session`], kind 4): everything about this
/// connection's session. Operating on other named sessions is
/// [`OpsToDaemonMsg`](super::OpsToDaemonMsg).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionToDaemonMsg {
    /// Attach to the named idle session.
    Attach {
        /// Which session to land on, exact or by prefix.
        target: AttachTarget,
        /// Refuse the attach with [`AttachFailure::SessionExited`] if already exited.
        ///
        /// Set on system-selected landings to prevent parking on dead sessions.
        /// Explicit user targets leave it false to inspect the post-exit grace screen.
        live_only: bool,
    },
    /// Create a new session *and* attach this
    /// connection to it; the daemon replies [`SessionToClientMsg::Created`] only once
    /// the subscription landed, so the pool never shows a session whose
    /// creator was not told its id. A headless spawn that wants no
    /// window uses [`OpsToDaemonMsg::Spawn`](super::OpsToDaemonMsg::Spawn) instead.
    Create {
        /// Unset fields fall back to daemon defaults (`$SHELL`, the
        /// daemon's cwd, 24×80).
        args: SpawnArgs,
    },
    /// The window is closing; the session lives on.
    Detach,
    /// Client configured theme colors, sent right after attach.
    ///
    /// Answers `OSC 10/11/12 ; ?` queries with client surface colors.
    /// `None` retains renderer and xterm defaults.
    ConfigureTheme {
        /// Configured default foreground (`OSC 10`), or `None`.
        fg: Option<(u8, u8, u8)>,
        /// Configured default background (`OSC 11`), or `None`.
        bg: Option<(u8, u8, u8)>,
        /// Configured cursor color (`OSC 12`), or `None`.
        cursor: Option<(u8, u8, u8)>,
    },
    /// Barrier over this connection's earlier
    /// [`InputMsg`](super::InputMsg) frames, answered with
    /// [`SessionToClientMsg::InputAccepted`].
    InputFence,
}

/// The daemon's half of the session-lifecycle family: the answers to
/// [`SessionToDaemonMsg`]'s attach, create, and fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionToClientMsg {
    /// [`SessionToDaemonMsg::Attach`] landed; the rehydrate burst
    /// follows.
    Attached {
        /// The full roster row, as an
        /// [`OpsToClientMsg::Listed`](super::OpsToClientMsg::Listed) reply carries it.
        info: super::SessionInfo,
    },
    /// Written on every refusal path of
    /// [`SessionToDaemonMsg::Attach`] and [`SessionToDaemonMsg::Create`] before closing, so a
    /// caller never has to read a bare EOF as a refusal.
    AttachFailed {
        /// Which half refused, and why.
        reason: AttachRefusal,
        /// Human-readable specifics, empty for reasons that carry no
        /// dynamic value. Never parsed.
        detail: String,
    },
    /// Confirmation that [`SessionToDaemonMsg::Create`] landed.
    ///
    /// The session exists and the connection is subscribed; rehydrate follows.
    Created {
        /// The full roster row; `info.dims` is the authoritative
        /// geometry after `SpawnArgs::dims` defaults resolved.
        info: super::SessionInfo,
    },
    /// Every `Input` frame that preceded the fence on
    /// this connection has been processed, and the bytes the
    /// byte-carrying arms encoded to are admitted and queued to the PTY
    /// writer. It does not say the child read anything, and the reports
    /// the other arms generate stay best-effort (`docs/reference/ipc.md`).
    InputAccepted,
}

/// The openers and their answers are uncorrelated: a connection
/// attaches once, so the ack needs no id to tell it from another attach
/// in flight. The fence pair is the family's one exception, since a
/// caller may put several barriers on one connection.
const fn arm(name: &'static str, direction: Direction, phases: PhaseSet) -> ArmMeta {
    ArmMeta::new(
        name,
        direction,
        CorrelationClass::Uncorrelated,
        ModeSet::ATTACHERS,
    )
    .phases(phases)
}

/// The openers and their three answers belong to `Setup` alone: a
/// connection attaches or creates once, so a second opener is the peer
/// losing its place rather than a request to serve.
const fn opener(name: &'static str, direction: Direction) -> ArmMeta {
    arm(name, direction, PhaseSet::SETUP)
}

const fn fence(name: &'static str, direction: Direction, correlation: CorrelationClass) -> ArmMeta {
    ArmMeta::new(name, direction, correlation, ModeSet::ATTACHERS).phases(PhaseSet::ATTACHED)
}

impl Directed for SessionToDaemonMsg {
    const ARMS: &'static [ArmMeta] = &[
        opener("Session::Attach", Direction::ToDaemon),
        opener("Session::Create", Direction::ToDaemon),
        arm("Session::Detach", Direction::ToDaemon, PhaseSet::ATTACHED),
        arm(
            "Session::ConfigureTheme",
            Direction::ToDaemon,
            PhaseSet::ATTACHED,
        ),
        fence(
            "Session::InputFence",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Attach { .. } => 0,
            Self::Create { .. } => 1,
            Self::Detach => 2,
            Self::ConfigureTheme { .. } => 3,
            Self::InputFence => 4,
        }
    }
}

impl Directed for SessionToClientMsg {
    const ARMS: &'static [ArmMeta] = &[
        opener("Session::Attached", Direction::ToClient),
        opener("Session::AttachFailed", Direction::ToClient),
        opener("Session::Created", Direction::ToClient),
        fence(
            "Session::InputAccepted",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Attached { .. } => 0,
            Self::AttachFailed { .. } => 1,
            Self::Created { .. } => 2,
            Self::InputAccepted => 3,
        }
    }
}

/// Which session a [`SessionToDaemonMsg::Attach`] names.
///
/// The exact arm is for the callers that already hold a full id
/// (reconnect, a roster-driven switch), which would otherwise pay a
/// resolution for an id nobody shortened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachTarget {
    Id(u128),
    /// Lowercase hex session-id prefix (1-32 digits), resolved
    /// daemon-side; [`AttachFailure::NoMatch`] and
    /// [`AttachFailure::Ambiguous`] are its two refusals.
    Prefix(String),
}

/// Which half of a [`SessionToClientMsg::AttachFailed`] refused, and why.
///
/// A [`SessionToDaemonMsg::Create`] runs a spawn half and then an attach half,
/// so both arms answer it; a [`SessionToDaemonMsg::Attach`] has no spawn half
/// and only ever carries [`Self::Attach`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachRefusal {
    Attach(AttachFailure),
    Create(CreateFailure),
}

/// Why the attach half failed: a [`SessionToDaemonMsg::Attach`], or the attach
/// that follows a [`SessionToDaemonMsg::Create`]'s spawn. Every value names a
/// session that already exists, so an
/// [`OpsToDaemonMsg::Spawn`](super::OpsToDaemonMsg::Spawn), which subscribes to nothing,
/// has no use for this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachFailure {
    /// No session in the pool holds the requested id.
    UnknownSession,
    /// The session's owner task ended before the subscription landed:
    /// the reap raced the attach.
    SessionEnding,
    /// A `live_only` [`SessionToDaemonMsg::Attach`] named a session whose shell
    /// has already exited; a deliberate attach would still land.
    SessionExited,
    /// An [`AttachTarget::Prefix`] matched no session in the pool.
    /// Distinct from [`Self::UnknownSession`], which answers a full id:
    /// the remedy is a different prefix, not a longer one.
    NoMatch,
    /// An [`AttachTarget::Prefix`] matched more than one session; the
    /// count rides in `detail` so the caller can ask for a longer
    /// prefix.
    Ambiguous,
}

/// Why the daemon refused the spawn half of a [`SessionToDaemonMsg::Create`] or
/// an [`OpsToDaemonMsg::Spawn`](super::OpsToDaemonMsg::Spawn).
///
/// Nothing here names a session the caller asked for: the request is
/// about one that does not exist yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateFailure {
    /// The create could not start its program.
    SpawnFailed,
    /// The create asked for a geometry outside the supported bounds
    /// (REQ-605a); nothing was executed. The offending axis and its
    /// bound ride in `detail`.
    GeometryOutOfRange,
    /// The create arrived while the daemon holds its configured maximum
    /// number of sessions; nothing was executed. The cap and the count
    /// ride in `detail`.
    SessionLimitReached,
    /// The create arrived while the daemon was draining toward exit
    /// ([`OpsToDaemonMsg::Stop`](super::OpsToDaemonMsg::Stop)); nothing was executed.
    /// Distinct from [`Self::SessionLimitReached`]: no reap makes room,
    /// so the remedy is another daemon rather than a retry here.
    DaemonDraining,
}

/// Typed spawn parameters for [`SessionToDaemonMsg::Create`] and
/// [`OpsToDaemonMsg::Spawn`](super::OpsToDaemonMsg::Spawn): a fixed-shape
/// message, no shell-parsed command line (principle 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SpawnArgs {
    /// Program to exec. Empty means the daemon's default (`$SHELL`).
    pub command: String,
    /// Argv after the program name.
    pub args: Vec<String>,
    /// Empty means inherit the daemon's cwd.
    pub cwd: String,
    /// Extra environment pairs, applied last over the inherited env and
    /// the daemon's identity stamps. `FELIS_SESSION_ID` and the daemon's
    /// sanitize denylist are off limits: naming one rejects the whole
    /// spawn (REQ-912).
    pub env: Vec<(String, String)>,
    /// Initial geometry, at wire width and not yet admitted: an axis
    /// outside the REQ-605a bounds is answered with
    /// [`CreateFailure::GeometryOutOfRange`] before anything execs.
    /// `None` asks for the daemon default (24×80); `Some` must name a
    /// real size on both cell axes, and pixel `0` still means unknown.
    pub dims: Option<RequestedDims>,
    /// Replacement child base environment as raw platform bytes (REQ-912).
    ///
    /// Provided on local socket connections; SSH relays leave this `None`.
    /// Bounded by [`MAX_ENV_BASE_ENTRIES`] and [`MAX_ENV_BASE_BYTES`].
    pub env_base: Option<Vec<(Vec<u8>, Vec<u8>)>>,
    /// Labels applied atomically at creation to prevent untagged visibility races.
    ///
    /// Sized per [`MAX_SESSION_TAGS`](super::MAX_SESSION_TAGS) and
    /// [`MAX_TAG_BYTES`](super::MAX_TAG_BYTES).
    pub tags: Vec<String>,
}

/// Maximum entries an [`SpawnArgs::env_base`] snapshot may carry.
///
/// Checked by the client before sending and by the daemon's spawn path
/// rather than in `codec::decode` (to refuse the create instead of dropping the connection).
pub const MAX_ENV_BASE_ENTRIES: usize = 4096;

/// Total key + value bytes an [`SpawnArgs::env_base`] snapshot may
/// carry, sized against the platform's own environment ceiling
/// (`ARG_MAX` counts the environment block on Unix).
pub const MAX_ENV_BASE_BYTES: usize = 1 << 20;

#[cfg(test)]
pub(crate) mod tests {
    use std::num::NonZeroU64;

    use super::*;
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    fn sample_ready_info(id: u128) -> crate::messages::SessionInfo {
        crate::messages::SessionInfo {
            id,
            dims: crate::messages::GridDims {
                rows: 24,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            },
            title: Some("zsh".into()),
            cwd: None,
            idle_seconds: None,
            tags: Vec::new(),
            last_notification: None,
            foreground: None,
            exited: false,
            last_exit_code: None,
            attachments: Vec::new(),
            sequence: NonZeroU64::new(3).unwrap(),
        }
    }

    fn session_to_daemon_cases() -> Vec<SessionToDaemonMsg> {
        vec![
            SessionToDaemonMsg::Attach {
                target: AttachTarget::Id(0x1234_5678),
                live_only: false,
            },
            SessionToDaemonMsg::Attach {
                target: AttachTarget::Id(0x1234_5678),
                live_only: true,
            },
            SessionToDaemonMsg::Attach {
                target: AttachTarget::Prefix("1234".into()),
                live_only: false,
            },
            SessionToDaemonMsg::Attach {
                target: AttachTarget::Prefix("1234abcd".into()),
                live_only: true,
            },
            SessionToDaemonMsg::Create {
                args: SpawnArgs::default(),
            },
            SessionToDaemonMsg::Create {
                args: SpawnArgs {
                    command: "/bin/sh".into(),
                    args: vec!["-c".into(), "echo hi".into()],
                    cwd: "/tmp".into(),
                    env: vec![("KEY".into(), "VAL".into())],
                    dims: Some(RequestedDims {
                        rows: 50,
                        cols: 132,
                        pixel_w: 0,
                        pixel_h: 0,
                    }),
                    tags: vec!["agent".into(), "build".into()],
                    env_base: Some(vec![
                        (b"PATH".to_vec(), b"/bin".to_vec()),
                        (b"SSH_AUTH_SOCK".to_vec(), vec![0x2f, 0xff]),
                    ]),
                },
            },
            SessionToDaemonMsg::Create {
                args: SpawnArgs {
                    env_base: Some(Vec::new()),
                    ..SpawnArgs::default()
                },
            },
            SessionToDaemonMsg::Create {
                args: SpawnArgs {
                    dims: Some(RequestedDims {
                        rows: 70_000,
                        cols: 80,
                        pixel_w: 0,
                        pixel_h: 0,
                    }),
                    ..SpawnArgs::default()
                },
            },
            SessionToDaemonMsg::Detach,
            SessionToDaemonMsg::ConfigureTheme {
                fg: Some((0xE5, 0xE5, 0xE5)),
                bg: Some((0x0D, 0x0D, 0x12)),
                cursor: Some((0xFF, 0xAA, 0x00)),
            },
            SessionToDaemonMsg::ConfigureTheme {
                fg: None,
                bg: Some((0x0D, 0x0D, 0x12)),
                cursor: None,
            },
            SessionToDaemonMsg::InputFence,
        ]
    }

    fn session_to_client_cases() -> Vec<SessionToClientMsg> {
        vec![
            SessionToClientMsg::Attached {
                info: sample_ready_info(0x1234_5678),
            },
            SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Attach(AttachFailure::UnknownSession),
                detail: String::new(),
            },
            SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Create(CreateFailure::SpawnFailed),
                detail: "No such file or directory (os error 2)".into(),
            },
            SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Attach(AttachFailure::SessionEnding),
                detail: String::new(),
            },
            SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Attach(AttachFailure::SessionExited),
                detail: String::new(),
            },
            SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Create(CreateFailure::GeometryOutOfRange),
                detail: "rows 70000 is outside the supported range 1..=2048".into(),
            },
            SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Create(CreateFailure::SessionLimitReached),
                detail: "the daemon is at 8 of 8 sessions".into(),
            },
            SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Create(CreateFailure::DaemonDraining),
                detail: String::new(),
            },
            SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Attach(AttachFailure::NoMatch),
                detail: "no session id starts with `beef`".into(),
            },
            SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Attach(AttachFailure::Ambiguous),
                detail: "3 sessions match".into(),
            },
            SessionToClientMsg::Created {
                info: sample_ready_info(0xFEED),
            },
            SessionToClientMsg::InputAccepted,
        ]
    }

    /// The fence pair is the family's only request/reply, so its two
    /// rows must carry the classes that make a reply pair with its
    /// request rather than the uncorrelated class every other arm has.
    #[test]
    fn the_fence_pair_is_the_only_correlated_arm_of_the_family() {
        use crate::messages::Directed as _;

        for msg in session_to_daemon_cases() {
            let expected = match msg {
                SessionToDaemonMsg::InputFence => CorrelationClass::RequestOpener,
                _ => CorrelationClass::Uncorrelated,
            };
            assert_eq!(msg.meta().correlation, expected, "{msg:?}");
        }
        for msg in session_to_client_cases() {
            let expected = match msg {
                SessionToClientMsg::InputAccepted => CorrelationClass::RequestReply,
                _ => CorrelationClass::Uncorrelated,
            };
            assert_eq!(msg.meta().correlation, expected, "{msg:?}");
        }
    }

    /// The reply travels on the request's id, so the envelope a fence
    /// reply is stamped with must survive the encode the daemon makes.
    #[test]
    fn a_fence_reply_carries_the_request_id_it_answers() {
        use crate::codec::{encode_correlated, peek_correlation};
        use crate::messages::{Correlation, RequestId};

        let correlation = Correlation::request(RequestId::new(9).unwrap());
        let bytes = encode_correlated(&SessionToClientMsg::InputAccepted, correlation);
        assert_eq!(peek_correlation(&bytes).unwrap(), Some(correlation));
        assert_eq!(
            crate::codec::decode::<SessionToClientMsg>(&bytes).unwrap(),
            SessionToClientMsg::InputAccepted
        );
    }

    /// An empty prefix has no wire shape: it encodes as an attach with
    /// neither target field set, which is what the decoder refuses. The
    /// send gate must catch it first, so it never costs a connection.
    #[test]
    fn an_empty_attach_prefix_is_refused_before_it_is_sent() {
        use crate::messages::Validate;

        let empty = SessionToDaemonMsg::Attach {
            target: AttachTarget::Prefix(String::new()),
            live_only: false,
        };
        assert!(Validate::validate(&empty).is_err());
        for bad in ["zz", &"a".repeat(33)] {
            let msg = SessionToDaemonMsg::Attach {
                target: AttachTarget::Prefix(bad.to_owned()),
                live_only: false,
            };
            assert!(
                Validate::validate(&msg).is_err(),
                "`{bad}` is not a prefix any session id can start with"
            );
        }
        let ok = SessionToDaemonMsg::Attach {
            target: AttachTarget::Prefix("0".into()),
            live_only: false,
        };
        assert!(Validate::validate(&ok).is_ok());
    }

    /// Session id `0` is a real id: it encodes as sixteen zero bytes,
    /// not as an absent field, so the exactly-one-target rule must
    /// still read it as the exact arm.
    #[test]
    fn an_attach_by_the_zero_id_round_trips_as_an_exact_target() {
        let zero = SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(0),
            live_only: false,
        };
        assert_eq!(roundtrip(&zero), zero);
    }

    #[test]
    fn session_messages_round_trip() {
        for msg in session_to_daemon_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
        for msg in session_to_client_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn session_cases_cover_every_variant() {
        assert_covers_every_arm(&session_to_daemon_cases());
        assert_covers_every_arm(&session_to_client_cases());
    }
}

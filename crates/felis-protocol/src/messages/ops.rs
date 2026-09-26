//! [`OpsToDaemonMsg`] and [`OpsToClientMsg`]: one-shot ops on *named
//! other* sessions (kind 5), with the session-roster vocabulary its `Listed` reply carries.

use std::num::NonZeroU64;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet, PhaseSet};

use super::{CreateFailure, GridDims, Notification, RetargetTarget, SpawnArgs};
#[cfg(test)]
use super::{RetargetCarrier, RetargetLanding};

/// One row in [`OpsToClientMsg::Listed`]. Every minor that defines this message
/// writes every optional field, so an absent one is a malformed frame
/// rather than an older producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u128,
    pub dims: GridDims,
    /// Last `OSC 0/2` title observed; `None` until one is set.
    pub title: Option<String>,
    /// Last `OSC 7` cwd observed, typically a `file://hostname/path` URL.
    pub cwd: Option<String>,
    /// Seconds since the session was last (re-)inserted into the pool,
    /// evaluated at list-build time; `None` while it is attached, so
    /// `Some(0)` is a session detached for under a second.
    pub idle_seconds: Option<u64>,
    /// Opaque labels set via [`OpsToDaemonMsg::Tag`], never interpreted by felis
    /// (`docs/explanation/architecture/control-surfaces.md`); empty when
    /// untagged.
    pub tags: Vec<String>,
    /// Most recent decoded desktop notification (OSC 9/99/777); `None`
    /// when none has been emitted.
    pub last_notification: Option<SessionNotification>,
    /// `comm` of the foreground process-group leader (`tcgetpgrp`, then
    /// `/proc/<pgid>/comm` on Linux or `libproc` on macOS), e.g.
    /// `claude`; `None` when unresolvable (leader gone, unsupported
    /// platform).
    pub foreground: Option<String>,
    /// The shell has exited and the session is in the post-exit grace
    /// (`architecture/session-lifecycle.md` "Post-exit reaping"). A
    /// client's automatic pick must skip these: attaching keeps the
    /// session alive, but its shell never returns.
    pub exited: bool,
    /// Code of the youngest retained `OSC 133 ; D ; <code>` mark; `None`
    /// without OSC 133 marks, before any command completed, or when the
    /// youngest `D` carried no code.
    pub last_exit_code: Option<u32>,
    /// Live window attachments, oldest first: the set
    /// [`SwitchScope::Attachment`] can name. Empty for a parked session
    /// and for one with only [`ConnectionMode::Ops`](crate::ConnectionMode::Ops)
    /// readers, which have no window to move.
    pub attachments: Vec<Attachment>,
    /// Creation sequence: monotonic over the daemon's lifetime, stamped
    /// once and never reassigned; reaps leave gaps. The switch ring sorts
    /// on it, since random session ids would reshuffle it on every
    /// creation.
    pub sequence: NonZeroU64,
}

/// One live window attachment ([`SessionInfo::attachments`]). The id is
/// on the roster rather than in the child environment: a PTY
/// environment is fixed at spawn and shared by every mirror, so an
/// environment variable cannot name "this window".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Daemon-lifetime id, unique across sessions and never reused.
    pub id: u64,
    /// Absolute, unlike the roster's age stamps: an attach instant is a
    /// fact about the daemon's clock that a cross-host reader cannot
    /// re-derive.
    pub attached_at: SystemTime,
    /// The session's last window input owner, what
    /// [`SwitchScope::Default`] resolves to; at most one attachment
    /// reports `true`.
    pub input_owner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNotification {
    pub notification: Notification,
    /// Seconds since arrival, evaluated at list-build time.
    pub age_seconds: u64,
}

/// Operations on named other sessions ([`crate::MessageKind::Ops`], kind
/// 5), the client's half: each request is answered by its paired
/// [`OpsToClientMsg`] reply, matched on the correlation envelope's
/// `request_id`. [`Self::List`] is admitted to every attach-capable
/// mode; the mutating arms need [`crate::ConnectionMode::Ops`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpsToDaemonMsg {
    /// Replied by [`OpsToClientMsg::Listed`].
    List,
    /// Replied by [`OpsToClientMsg::Destroyed`], after which the
    /// daemon closes the connection.
    Destroy {
        /// Lowercase hex session-id prefix (1–32 digits), resolved
        /// daemon-side ([`ResolvedId`]).
        id_prefix: String,
    },
    /// The daemon sends the live client
    /// [`PushMsg::Evicted`](super::PushMsg::Evicted), pools the session,
    /// then replies [`OpsToClientMsg::Detached`]
    /// (`docs/explanation/architecture/control-surfaces.md`).
    ForceDetach {
        /// Lowercase hex session-id prefix, resolved daemon-side.
        id_prefix: String,
    },
    /// Switch request.
    ///
    /// The daemon relays pushes to scoped windows and replies [`OpsToClientMsg::Switched`]
    /// (`docs/explanation/architecture/control-surfaces.md`).
    Switch {
        /// Session-id prefix, resolved daemon-side.
        from_prefix: String,
        target: SwitchTarget,
        /// Resolved inside the session actor, in the same step that
        /// enqueues the push.
        scope: SwitchScope,
    },
    /// A delta that would breach [`MAX_SESSION_TAGS`]
    /// or [`MAX_TAG_BYTES`] is refused whole via [`OpsToClientMsg::TagsUpdated`]'s
    /// `denied`, never partially applied; the daemon replies
    /// [`OpsToClientMsg::TagsUpdated`] then closes the connection. Tags are
    /// marginalia, not a session group (`docs/explanation/non-goals.md`).
    Tag {
        /// Lowercase hex session-id prefix, resolved daemon-side.
        id_prefix: String,
        /// Idempotent: a tag already present is a no-op.
        add: Vec<String>,
        /// Idempotent: an absent tag is a no-op.
        remove: Vec<String>,
    },
    /// Replied by [`OpsToClientMsg::StatusReply`]. Backs `felis
    /// daemon status` (`docs/reference/cli.md` "Daemon status").
    Status,
    /// Replied by [`OpsToClientMsg::Spawned`]. Creates a session
    /// this connection does not attach to. Here rather than on
    /// [`SessionToDaemonMsg::Create`](super::SessionToDaemonMsg::Create) so several
    /// spawns can be in flight on one connection, each reply attributed
    /// by its `request_id`.
    Spawn { args: SpawnArgs },
    /// Replied by [`OpsToClientMsg::StopReply`]. Backs `felis
    /// daemon stop` (`docs/reference/cli.md` "Daemon stop").
    Stop { mode: StopMode },
    /// Replied by [`OpsToClientMsg::InfoReply`]. One session's
    /// roster row, resolved and shortened against the pool in the same
    /// step, so `felis sessions info` owes no [`Self::List`] round trip
    /// for either (`docs/explanation/architecture/control-surfaces.md`).
    Info {
        /// Lowercase hex session-id prefix (1-32 digits), resolved
        /// daemon-side.
        id_prefix: String,
    },
}

/// The daemon's half of the ops family: one reply per
/// [`OpsToDaemonMsg`] request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpsToClientMsg {
    Listed {
        /// Every session in the pool, attached or idle.
        sessions: Vec<SessionInfo>,
    },
    /// A failed resolution is a typed [`ResolvedId`]
    /// arm, not a hard error.
    Destroyed {
        resolved: ResolvedId,
    },
    Detached {
        resolved: ResolvedId,
        /// True when a live client was evicted, false when the session
        /// was already idle; meaningful only on a successful resolution.
        was_attached: bool,
    },
    Switched {
        from: ResolvedId,
        /// `None` for a [`SwitchTarget::Carrier`]: the target session
        /// lives in another daemon's namespace.
        to: Option<ResolvedId>,
        /// Subscribers of `from` whose outbox took the push. Queue
        /// admission is the whole promise, and a window that never
        /// lands reports nothing back here
        /// (`docs/explanation/architecture/ipc.md`, "What a switch
        /// reply can report").
        queued: u32,
        /// `Some(reason)` when `scope` named no window and nothing was
        /// pushed (`queued` is then `0`). A push that reached zero
        /// windows leaves this `None`: zero is a count, not a failure.
        denied: Option<SwitchDenied>,
    },
    TagsUpdated {
        resolved: ResolvedId,
        /// The full tag set after the delta, sorted and deduplicated;
        /// empty on a failed resolution, unchanged when `denied` is set.
        tags: Vec<String>,
        /// `Some(reason)` when the delta was refused whole for breaching
        /// a cap; `None` on success.
        denied: Option<String>,
    },
    /// Live state only: the daemon's build and wire
    /// pair are the handshake's answer, and a second copy here would
    /// give one connection two authorities for one fact. There is no
    /// separate session count either: it is the
    /// [`ResourceKind::Sessions`] row's `total_used`.
    StatusReply {
        /// One row per accounted resource; order is the daemon's, so a
        /// consumer keys on `resource`.
        resources: Vec<ResourceReport>,
        /// The daemon refuses creates and exits after the last session
        /// ([`OpsToDaemonMsg::Stop`]).
        draining: bool,
        /// Not a [`ResourceReport`] row: the runtime is built at a fixed
        /// size, so nothing is admitted against it.
        worker_threads: u32,
    },
    Spawned {
        outcome: SpawnOutcome,
    },
    /// Written before the daemon acts on the request,
    /// so the outcome reaches the caller of a stop that succeeds.
    StopReply {
        outcome: StopOutcome,
    },
    /// A closed outcome union in the
    /// [`Self::Spawned`] style: only the successful arm carries session
    /// data, so a failed resolution cannot be read as an empty row.
    InfoReply {
        outcome: InfoOutcome,
    },
}

/// What an [`OpsToDaemonMsg::Stop`] asks for. A closed set rather than a pair of
/// flags: "destroy everything" and "wait for empty" are opposite
/// postures, and no request should be able to name both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StopMode {
    /// Stop only while the daemon admits nothing.
    #[default]
    IfEmpty,
    /// Destroy every session, then stop.
    Force,
    /// Refuse new creates and stop after the last session ends.
    WhenEmpty,
}

/// How an [`OpsToDaemonMsg::Stop`] was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopOutcome {
    /// The daemon is going down.
    Stopping,
    /// A [`StopMode::IfEmpty`] found sessions; nothing was touched.
    Refused {
        /// Sessions admitted at the decision: registered plus the
        /// creates in flight, the number
        /// [`ResourceKind::Sessions`] reports.
        sessions: u32,
    },
    /// The daemon refuses creates and exits after the last session. A
    /// second stop while draining answers this again.
    Draining { sessions: u32 },
}

/// How an [`OpsToDaemonMsg::Info`] resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InfoOutcome {
    Found {
        /// The same row [`OpsToClientMsg::Listed`] carries, degraded for the
        /// connection's minor exactly as a roster row is.
        session: Box<SessionInfo>,
        /// The display prefix, shortened against the whole pool at
        /// reply time: checked against the authoritative roster, so it
        /// is never a prefix the daemon would refuse.
        short_id: String,
    },
    NoMatch,
    /// The match count, so the caller can ask for a longer prefix.
    Ambiguous {
        matches: u32,
    },
}

/// How an [`OpsToDaemonMsg::Spawn`] ended. A refusal is a typed arm of the
/// reply, as [`ResolvedId`] is for the prefix verbs, rather than a
/// `ConnToClientMsg::Error`: the connection is fine, the request is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnOutcome {
    Ok {
        /// The new session's roster row; `dims` is the authoritative
        /// geometry after [`SpawnArgs::dims`] defaults resolved.
        info: Box<SessionInfo>,
    },
    Refused {
        /// A spawn never subscribes, so its whole vocabulary is the
        /// spawn half's: the attach-half reasons a
        /// [`SessionToClientMsg::AttachFailed`](super::SessionToClientMsg::AttachFailed)
        /// can also carry are not representable here.
        reason: CreateFailure,
        /// Human-readable specifics, empty for reasons that carry no
        /// dynamic value. Never parsed.
        detail: String,
    },
}

/// One accounted resource ([`OpsToClientMsg::StatusReply`]). Every ratio a
/// reader can form stays inside one denominator, because each
/// per-subject number lives in the scope arm that gives it a meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceReport {
    pub resource: ResourceKind,
    pub unit: ResourceUnit,
    /// Summed over every subject, at the moment the reply was built.
    pub total_used: u64,
    pub scope: ReportScope,
}

/// What one subject of a [`ResourceReport`] is, carrying the numbers
/// that only that answer makes meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportScope {
    /// The daemon is its own single subject, so
    /// [`ResourceReport::total_used`] is already the deepest one.
    Daemon { global_limit: Limit },
    Subject {
        subject: SubjectKind,
        /// The deepest single subject, the one that reaches
        /// `per_subject_limit` first.
        max_subject_used: u64,
        per_subject_limit: Limit,
        global_limit: Limit,
    },
}

/// A ceiling or the absence of one. [`Self::Bounded`] carries `0` for a
/// resource nothing may hold, which [`Self::Unlimited`] does not mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Limit {
    Bounded(u64),
    Unlimited,
}

impl Limit {
    /// The bound, for a renderer that spells the unbounded case its own
    /// way rather than as a number.
    #[must_use]
    pub const fn bound(self) -> Option<u64> {
        match self {
            Self::Bounded(bound) => Some(bound),
            Self::Unlimited => None,
        }
    }
}

/// A closed set: `felis daemon status` renders a row per variant and the
/// reference page documents each; growth is a minor addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    /// Connections the daemon is serving, against its connection cap.
    /// The multiplier every per-connection ceiling is charged against.
    Connections,
    Sessions,
    /// Decoded Kitty-graphics bytes held in the per-session image stores.
    ImageStoreBytes,
    /// Kitty-graphics transmissions past their head chunk but not their
    /// terminator; at most one per session.
    InFlightDecodes,
    /// Bytes those partial transmissions hold.
    InFlightDecodeBytes,
    /// Live subscriber outboxes, in bytes queued but not yet written.
    SubscriberQueueBytes,
    /// Client→child input the daemon has admitted but not yet written to
    /// a PTY, against the per-session input budget.
    PtyInputBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceUnit {
    Count,
    Bytes,
}

/// What a [`ReportScope::Subject`]'s numbers are counted per. The
/// daemon is not a variant: it is the other scope, which has no
/// per-subject number to count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubjectKind {
    Session,
    /// The deepest subscriber is the one evicted first.
    Subscriber,
}

/// Queries remain open to `Window` mode, while mutating verbs require
/// `ConnectionMode::Ops`. Both are legal in `Setup` and `Attached`.
const fn query(name: &'static str, direction: Direction, correlation: CorrelationClass) -> ArmMeta {
    ArmMeta::new(name, direction, correlation, ModeSet::ATTACHERS)
        .phases(PhaseSet::SETUP_OR_ATTACHED)
}

const fn mutation(
    name: &'static str,
    direction: Direction,
    correlation: CorrelationClass,
) -> ArmMeta {
    ArmMeta::new(name, direction, correlation, ModeSet::OPS).phases(PhaseSet::SETUP_OR_ATTACHED)
}

impl Directed for OpsToDaemonMsg {
    const ARMS: &'static [ArmMeta] = &[
        query(
            "Ops::List",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
        mutation(
            "Ops::Destroy",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
        mutation(
            "Ops::ForceDetach",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
        mutation(
            "Ops::Switch",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
        mutation(
            "Ops::Tag",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
        query(
            "Ops::Status",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
        mutation(
            "Ops::Spawn",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
        // Setup only, unlike every other Ops verb: an attached
        // connection queues its replies through an outbox the exiting
        // process would not drain.
        mutation(
            "Ops::Stop",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        )
        .phases(PhaseSet::SETUP),
        query(
            "Ops::Info",
            Direction::ToDaemon,
            CorrelationClass::RequestOpener,
        ),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::List => 0,
            Self::Destroy { .. } => 1,
            Self::ForceDetach { .. } => 2,
            Self::Switch { .. } => 3,
            Self::Tag { .. } => 4,
            Self::Status => 5,
            Self::Spawn { .. } => 6,
            Self::Stop { .. } => 7,
            Self::Info { .. } => 8,
        }
    }
}

impl Directed for OpsToClientMsg {
    const ARMS: &'static [ArmMeta] = &[
        query(
            "Ops::Listed",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
        mutation(
            "Ops::Destroyed",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
        mutation(
            "Ops::Detached",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
        mutation(
            "Ops::Switched",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
        mutation(
            "Ops::TagsUpdated",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
        query(
            "Ops::StatusReply",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
        mutation(
            "Ops::Spawned",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
        mutation(
            "Ops::StopReply",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        )
        .phases(PhaseSet::SETUP),
        query(
            "Ops::InfoReply",
            Direction::ToClient,
            CorrelationClass::RequestReply,
        ),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Listed { .. } => 0,
            Self::Destroyed { .. } => 1,
            Self::Detached { .. } => 2,
            Self::Switched { .. } => 3,
            Self::TagsUpdated { .. } => 4,
            Self::StatusReply { .. } => 5,
            Self::Spawned { .. } => 6,
            Self::StopReply { .. } => 7,
            Self::InfoReply { .. } => 8,
        }
    }
}

/// Each `list` clones and re-encodes the tag set, so a runaway script
/// must not be able to inflate every roster read.
pub const MAX_SESSION_TAGS: usize = 32;

/// With [`MAX_SESSION_TAGS`], bounds a session's tag metadata to 4 KiB.
pub const MAX_TAG_BYTES: usize = 128;

/// Daemon-side resolution of a request's session-id prefix
/// ([`OpsToDaemonMsg::Destroy`] and friends, `NotifyToDaemonMsg::Subscribe`'s filter).
/// Resolving client-side would force every verb to run an `Ops::List`
/// first, doubling latency and giving observer mode a roster surface it
/// never needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolvedId {
    Ok {
        id: u128,
    },
    NoMatch,
    /// The match count, so the caller can ask for a longer prefix.
    Ambiguous {
        matches: u32,
    },
}

/// Where an [`OpsToDaemonMsg::Switch`] sends the windows of `from`. Each arm
/// names both the push it produces and the capability that gates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwitchTarget {
    /// A prefix on this daemon; produces
    /// [`PushMsg::Reattach`](super::PushMsg::Reattach).
    Session(String),
    /// A session on another daemon: the descriptor is relayed verbatim
    /// as [`PushMsg::RetargetHost`](super::PushMsg::RetargetHost), and
    /// cannot be validated here.
    Carrier(RetargetTarget),
}

/// Which of the `from` session's window attachments an
/// [`OpsToDaemonMsg::Switch`] moves. Resolved and enqueued in one step inside
/// the session actor, so nothing observes a half-resolved target; the
/// residual race is a mirror sending input in between, which takes the
/// input-owner marker with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwitchScope {
    /// The last window input owner, else the sole attached window;
    /// neither is [`SwitchDenied::NoInputOwner`], never a guess among
    /// several.
    Default,
    /// One window by [`Attachment::id`]; a detached id is
    /// [`SwitchDenied::NoSuchAttachment`], never redirected.
    Attachment(u64),
}

/// Why an [`OpsToDaemonMsg::Switch`] resolved to no window
/// ([`OpsToClientMsg::Switched::denied`]). Resolve-time only: no arm can arrive
/// once the push is enqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwitchDenied {
    /// [`SwitchScope::Default`] with a clear marker and zero or several
    /// windows.
    NoInputOwner,
    /// Ids are never reused, so this means "gone", never "someone
    /// else's window now".
    NoSuchAttachment { attachment: u64 },
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::messages::Urgency;
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    fn sample_session_info() -> SessionInfo {
        SessionInfo {
            id: 0xCAFE_BABE_DEAD_BEEF,
            dims: GridDims {
                rows: 24,
                cols: 80,
                pixel_w: 1600,
                pixel_h: 1200,
            },
            title: Some("zsh: ~/work".into()),
            cwd: Some("file://localhost/home/me/work".into()),
            idle_seconds: Some(3661),
            tags: vec!["work".into(), "agent".into()],
            last_notification: Some(SessionNotification {
                notification: Notification {
                    title: Some("build".into()),
                    body: "cargo build finished".into(),
                    urgency: Urgency::Normal,
                },
                age_seconds: 12,
            }),
            foreground: Some("claude".into()),
            last_exit_code: Some(130),
            exited: true,
            attachments: vec![
                Attachment {
                    id: 7,
                    attached_at: UNIX_EPOCH + Duration::from_secs(1_788_250_500),
                    input_owner: true,
                },
                Attachment {
                    id: 9,
                    attached_at: UNIX_EPOCH + Duration::from_secs(1_788_250_590),
                    input_owner: false,
                },
            ],
            sequence: NonZeroU64::new(42).unwrap(),
        }
    }

    fn ops_to_daemon_cases() -> Vec<OpsToDaemonMsg> {
        vec![
            OpsToDaemonMsg::List,
            OpsToDaemonMsg::Destroy {
                id_prefix: "cafe".into(),
            },
            OpsToDaemonMsg::ForceDetach {
                id_prefix: "cafebabe".into(),
            },
            OpsToDaemonMsg::Switch {
                from_prefix: "cafe".into(),
                target: SwitchTarget::Session("beef".into()),
                scope: SwitchScope::Default,
            },
            OpsToDaemonMsg::Switch {
                from_prefix: "cafe".into(),
                target: SwitchTarget::Session("beef".into()),
                scope: SwitchScope::Attachment(u64::MAX),
            },
            OpsToDaemonMsg::Switch {
                from_prefix: "cafe".into(),
                target: SwitchTarget::Carrier(RetargetTarget {
                    carrier: RetargetCarrier::Ssh {
                        destination: "user@devbox".into(),
                        ssh_args: vec!["-p".into(), "2222".into()],
                    },
                    landing: RetargetLanding::Attach("3f9c".into()),
                }),
                scope: SwitchScope::Default,
            },
            OpsToDaemonMsg::Switch {
                from_prefix: "cafe".into(),
                target: SwitchTarget::Carrier(RetargetTarget {
                    carrier: RetargetCarrier::DefaultLocal,
                    landing: RetargetLanding::Create(SpawnArgs {
                        command: "htop".into(),
                        ..SpawnArgs::default()
                    }),
                }),
                scope: SwitchScope::Default,
            },
            OpsToDaemonMsg::Switch {
                from_prefix: "cafe".into(),
                target: SwitchTarget::Carrier(RetargetTarget {
                    carrier: RetargetCarrier::LocalEndpoint("/run/user/1000/felis/alt.sock".into()),
                    landing: RetargetLanding::Attach("ab12".into()),
                }),
                scope: SwitchScope::Attachment(3),
            },
            OpsToDaemonMsg::Tag {
                id_prefix: "cafef00d".into(),
                add: vec!["work".into(), "agent".into()],
                remove: vec!["stale".into()],
            },
            OpsToDaemonMsg::Status,
            OpsToDaemonMsg::Spawn {
                args: SpawnArgs::default(),
            },
            OpsToDaemonMsg::Spawn {
                args: SpawnArgs {
                    command: "htop".into(),
                    tags: vec!["agent".into()],
                    ..SpawnArgs::default()
                },
            },
            OpsToDaemonMsg::Stop {
                mode: StopMode::IfEmpty,
            },
            OpsToDaemonMsg::Stop {
                mode: StopMode::Force,
            },
            OpsToDaemonMsg::Stop {
                mode: StopMode::WhenEmpty,
            },
            OpsToDaemonMsg::Info {
                id_prefix: "cafe".into(),
            },
        ]
    }

    fn ops_to_client_cases() -> Vec<OpsToClientMsg> {
        vec![
            OpsToClientMsg::Listed {
                sessions: vec![sample_session_info()],
            },
            OpsToClientMsg::Listed {
                sessions: vec![SessionInfo {
                    id: 0xDEAD_BEEF,
                    dims: GridDims {
                        rows: 30,
                        cols: 100,
                        pixel_w: 0,
                        pixel_h: 0,
                    },
                    title: None,
                    cwd: None,
                    idle_seconds: None,
                    tags: Vec::new(),
                    last_notification: None,
                    foreground: None,
                    last_exit_code: None,
                    exited: false,
                    attachments: Vec::new(),
                    sequence: NonZeroU64::MIN,
                }],
            },
            OpsToClientMsg::Destroyed {
                resolved: ResolvedId::Ok {
                    id: 0xCAFE_BABE_DEAD_BEEF,
                },
            },
            OpsToClientMsg::Destroyed {
                resolved: ResolvedId::NoMatch,
            },
            OpsToClientMsg::Detached {
                resolved: ResolvedId::Ok {
                    id: 0xCAFE_BABE_DEAD_BEEF,
                },
                was_attached: true,
            },
            OpsToClientMsg::Detached {
                resolved: ResolvedId::Ambiguous { matches: 3 },
                was_attached: false,
            },
            OpsToClientMsg::Switched {
                from: ResolvedId::Ok { id: 0xCAFE },
                to: Some(ResolvedId::Ok { id: 0xBEEF }),
                queued: 1,
                denied: None,
            },
            OpsToClientMsg::Switched {
                from: ResolvedId::Ok { id: 0xCAFE },
                to: None,
                queued: 0,
                denied: Some(SwitchDenied::NoInputOwner),
            },
            OpsToClientMsg::Switched {
                from: ResolvedId::NoMatch,
                to: Some(ResolvedId::Ambiguous { matches: 2 }),
                queued: 0,
                denied: Some(SwitchDenied::NoSuchAttachment { attachment: 41 }),
            },
            OpsToClientMsg::TagsUpdated {
                resolved: ResolvedId::Ok { id: 0xCAFE_F00D },
                tags: vec!["agent".into(), "work".into()],
                denied: None,
            },
            OpsToClientMsg::TagsUpdated {
                resolved: ResolvedId::NoMatch,
                tags: Vec::new(),
                denied: None,
            },
            OpsToClientMsg::TagsUpdated {
                resolved: ResolvedId::Ok { id: 0xCAFE_F00D },
                tags: vec!["work".into()],
                denied: Some("a session carries at most 32 tags".into()),
            },
            OpsToClientMsg::StatusReply {
                resources: vec![
                    ResourceReport {
                        resource: ResourceKind::Connections,
                        unit: ResourceUnit::Count,
                        total_used: 7,
                        scope: ReportScope::Daemon {
                            global_limit: Limit::Bounded(1024),
                        },
                    },
                    ResourceReport {
                        resource: ResourceKind::Sessions,
                        unit: ResourceUnit::Count,
                        total_used: 3,
                        scope: ReportScope::Daemon {
                            global_limit: Limit::Unlimited,
                        },
                    },
                    ResourceReport {
                        resource: ResourceKind::ImageStoreBytes,
                        unit: ResourceUnit::Bytes,
                        total_used: 4096,
                        scope: ReportScope::Subject {
                            subject: SubjectKind::Session,
                            max_subject_used: 4096,
                            per_subject_limit: Limit::Bounded(268_435_456),
                            global_limit: Limit::Bounded(1_073_741_824),
                        },
                    },
                    ResourceReport {
                        resource: ResourceKind::SubscriberQueueBytes,
                        unit: ResourceUnit::Bytes,
                        total_used: 0,
                        scope: ReportScope::Subject {
                            subject: SubjectKind::Subscriber,
                            max_subject_used: 0,
                            per_subject_limit: Limit::Unlimited,
                            global_limit: Limit::Unlimited,
                        },
                    },
                ],
                worker_threads: 8,
                draining: false,
            },
            OpsToClientMsg::StatusReply {
                resources: Vec::new(),
                worker_threads: 0,
                draining: true,
            },
            OpsToClientMsg::Spawned {
                outcome: SpawnOutcome::Ok {
                    info: Box::new(sample_session_info()),
                },
            },
            OpsToClientMsg::Spawned {
                outcome: SpawnOutcome::Refused {
                    reason: CreateFailure::SessionLimitReached,
                    detail: "the daemon is at 256 of 256 sessions".into(),
                },
            },
            OpsToClientMsg::StopReply {
                outcome: StopOutcome::Stopping,
            },
            OpsToClientMsg::StopReply {
                outcome: StopOutcome::Refused { sessions: 3 },
            },
            OpsToClientMsg::StopReply {
                outcome: StopOutcome::Draining { sessions: 1 },
            },
            OpsToClientMsg::InfoReply {
                outcome: InfoOutcome::Found {
                    session: Box::new(sample_session_info()),
                    short_id: "0000cafe".into(),
                },
            },
            OpsToClientMsg::InfoReply {
                outcome: InfoOutcome::NoMatch,
            },
            OpsToClientMsg::InfoReply {
                outcome: InfoOutcome::Ambiguous { matches: 4 },
            },
        ]
    }

    #[test]
    fn ops_messages_round_trip() {
        for msg in ops_to_daemon_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
        for msg in ops_to_client_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn ops_cases_cover_every_variant() {
        assert_covers_every_arm(&ops_to_daemon_cases());
        assert_covers_every_arm(&ops_to_client_cases());
    }
}

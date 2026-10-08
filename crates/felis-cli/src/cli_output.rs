//! Machine output: `--format` framing, versioned envelopes, and payload types.
//!
//! Point verbs emit one JSON object; stream verbs emit JSONL lines ending
//! in a terminal object. Top-level objects carry `"v":1` (docs/reference/cli.md).

#![expect(
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "this module owns the machine-output channels"
)]

use std::sync::atomic::{AtomicBool, Ordering};

use clap::builder::TypedValueParser as _;
use felis_client_core::ConnectError;
use felis_protocol::SessionHex;
use felis_protocol::messages::{
    AttachFailure, CreateFailure, RefusalReason, SessionInfo, StreamErrorReason,
};
use felis_protocol::session_prefix::{SHORT_SESSION_PREFIX_MIN, short_session_prefix};
use serde::{Deserialize, Serialize};

/// Versions the CLI output contract, never the daemon wire: a wire
/// minor must be invisible here, or a consumer would re-negotiate for
/// growth it cannot see.
pub(crate) const SURFACE_VERSION: u32 = 1;

/// The output types are plain structs, so serialization cannot fail;
/// the fallback exists because a caller mid-emission has no channel
/// left to report on, and a parseable internal error beats a blank
/// line.
fn render(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| {
        let kind = ErrorKind::Internal.as_str();
        format!(
            r#"{{"v":{SURFACE_VERSION},"error":{{"kind":"{kind}","message":"could not render this object"}}}}"#
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// Prose for a human, explicitly *not* a parse target.
    Human,
    Json,
    Jsonl,
}

impl Format {
    pub(crate) const fn is_machine(self) -> bool {
        !matches!(self, Self::Human)
    }
}

/// A `ValueEnum` rather than a hand-written parser so clap publishes
/// the vocabulary into `--help` and the generated completions; the
/// cost is that `--format jsonl` reads as an unknown value rather than
/// naming the sibling framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PointFormatValue {
    Human,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum StreamFormatValue {
    Human,
    Jsonl,
}

impl From<PointFormatValue> for Format {
    fn from(value: PointFormatValue) -> Self {
        match value {
            PointFormatValue::Human => Self::Human,
            PointFormatValue::Json => Self::Json,
        }
    }
}

impl From<StreamFormatValue> for Format {
    fn from(value: StreamFormatValue) -> Self {
        match value {
            StreamFormatValue::Human => Self::Human,
            StreamFormatValue::Jsonl => Self::Jsonl,
        }
    }
}

/// The `jsonl` refusal is a clap possible-value error: it exits `2`
/// before the verb dials anything, and no machine object can precede
/// it (`docs/reference/cli.md`).
#[derive(Debug, clap::Args)]
pub(crate) struct PointFormat {
    /// Output framing: `human` (default) or `json`: one object on
    /// stdout, with a failure as one error object on stderr.
    #[arg(
        long,
        value_name = "FORMAT",
        default_value = "human",
        value_parser = clap::builder::EnumValueParser::<PointFormatValue>::new().map(Format::from)
    )]
    pub(crate) format: Format,
}

/// `--format` on a stream verb; `json` is refused as in [`PointFormat`].
#[derive(Debug, clap::Args)]
pub(crate) struct StreamFormat {
    /// Output framing: `human` (default) or `jsonl`: one JSON object
    /// per line on stdout, ending in exactly one terminal object.
    #[arg(
        long,
        value_name = "FORMAT",
        default_value = "human",
        value_parser = clap::builder::EnumValueParser::<StreamFormatValue>::new().map(Format::from)
    )]
    pub(crate) format: Format,
}

/// Declares the kinds with their token and exit code together, so the
/// roster, the tokens, and the exit map cannot disagree: a kind added
/// here reaches `ALL` (and with it the golden fixture and the published
/// schema) without a second edit that could be forgotten.
macro_rules! error_kinds {
    ($( $(#[$meta:meta])* $variant:ident => $token:literal, $exit:literal; )*) => {
        /// The vocabulary a consumer branches on, shared with `felis
        /// bridge` so the two surfaces cannot diverge. It grows
        /// additively within a `v`: a consumer that meets an unknown
        /// token reads the generic failure off the exit status. A
        /// token's spelling is frozen within a `v`; its prose is not.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum ErrorKind {
            $( $(#[$meta])* $variant, )*
        }

        impl Serialize for ErrorKind {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl ErrorKind {
            /// Every kind, in the order used by the CLI surface.
            /// The golden fixture and the published schema's
            /// `x-known-values` are rendered from this slice.
            #[cfg(any(test, feature = "schema"))]
            pub(crate) const ALL: &'static [Self] = &[$( Self::$variant, )*];

            pub(crate) const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $token, )*
                }
            }

            /// The one kind → exit-status table: `1` is a typed refusal
            /// answered on the merits, `2` a failure to ask at all (bad
            /// invocation, unreachable or broken peer). Per-site
            /// literals would let one condition exit `1` on one verb and
            /// `2` on another (docs/reference/cli.md "Exit codes").
            pub(crate) const fn exit_code(self) -> i32 {
                match self {
                    $( Self::$variant => $exit, )*
                }
            }
        }
    };
}

error_kinds! {
    /// The invocation itself was wrong, in a way clap could not state
    /// (a global carrier flag on a verb addressed to a window).
    Usage => "usage", 2;
    /// No session matched the given id or prefix.
    NoMatch => "no_match", 1;
    /// The prefix matched more than one session; nothing was chosen.
    Ambiguous => "ambiguous", 1;
    /// A bounded wait elapsed with nothing to report.
    Timeout => "timeout", 1;
    /// The scope named no window: nothing has typed in one, and the
    /// session has no single attached window to fall back on.
    NoInputOwner => "no_input_owner", 1;
    /// The named attachment is not on the session.
    NoSuchAttachment => "no_such_attachment", 1;
    /// The push reached the daemon but no capable window queued it.
    NotQueued => "not_queued", 1;
    /// The session ended before the operation could finish.
    SessionEnded => "session_ended", 1;
    /// The request could not be read or routed (bridge-side).
    MalformedRequest => "malformed_request", 2;
    /// A well-formed request the daemon or the CLI refuses on its
    /// merits.
    InvalidRequest => "invalid_request", 1;
    /// The daemon is at a ceiling and refused rather than exceeded it.
    /// Its own token because the remedy is to free something first
    /// (`docs/reference/cli.md` "Daemon status").
    AtCapacity => "at_capacity", 1;
    /// The request names something this connection's negotiated minor
    /// cannot carry.
    Unsupported => "unsupported", 2;
    /// No daemon answered at the named endpoint.
    DaemonUnreachable => "daemon_unreachable", 2;
    /// The daemon connection died mid-operation.
    DaemonLost => "daemon_lost", 2;
    /// The daemon abandoned a stream, with its own reason in the
    /// message.
    Refused => "refused", 1;
    /// The peer broke the protocol: an unreadable frame, a reply that
    /// answers nothing asked.
    Protocol => "protocol", 2;
    /// The operation was canceled by its caller.
    Canceled => "canceled", 2;
    /// The process's own request pipe failed (bridge-side).
    InputFailed => "input_failed", 2;
    /// The process's own response pipe failed (bridge-side).
    OutputFailed => "output_failed", 2;
    /// Anything the surface cannot name more precisely.
    Internal => "internal", 2;
}

impl ErrorKind {
    /// The one wire-reason → kind table every stream verb reads a
    /// failing terminal through. Per-site mappings would let the same
    /// wire reason answer differently depending on which verb read it.
    pub(crate) const fn from_stream_reason(reason: StreamErrorReason) -> Self {
        match reason {
            StreamErrorReason::InvalidRequest => Self::InvalidRequest,
            StreamErrorReason::TooManyStreams => Self::AtCapacity,
            StreamErrorReason::Unavailable => Self::Refused,
            StreamErrorReason::Internal => Self::Internal,
        }
    }

    /// The one attach-refusal → kind table.
    pub(crate) const fn from_attach_failure(reason: AttachFailure) -> Self {
        match reason {
            // The same two tokens the mutating `Ops` verbs report a
            // failed prefix with: one "which session?" failure, one
            // voice, whichever surface resolved it.
            AttachFailure::NoMatch => Self::NoMatch,
            AttachFailure::Ambiguous => Self::Ambiguous,
            // The daemon answered on the merits about a session it
            // knows, so it is a typed refusal.
            AttachFailure::UnknownSession
            | AttachFailure::SessionEnding
            | AttachFailure::SessionExited => Self::InvalidRequest,
        }
    }

    /// The one create-refusal → kind table. A caller that cannot tell
    /// "the daemon is full" from "felis and the daemon disagree about
    /// the request" cannot decide whether to reap a session and retry.
    pub(crate) const fn from_create_failure(reason: CreateFailure) -> Self {
        match reason {
            CreateFailure::SessionLimitReached => Self::AtCapacity,
            // A geometry outside the REQ-605a bounds, a program the
            // daemon could not exec, a daemon on its way out: the
            // daemon answered on the merits, so it is a typed refusal.
            CreateFailure::SpawnFailed
            | CreateFailure::GeometryOutOfRange
            | CreateFailure::DaemonDraining => Self::InvalidRequest,
        }
    }

    /// The one table for a connection-scoped failure of a request that
    /// names a session. `no_match` and `ambiguous` belong to the
    /// daemon's typed answer alone: a caller that reads a dropped
    /// socket as "no such session" retries the wrong thing. Exhaustive
    /// so a new `ConnectError` variant cannot inherit a kind by default.
    pub(crate) const fn from_connect_error(err: &ConnectError) -> Self {
        match err {
            ConnectError::AttachFailed { reason, .. } => Self::from_attach_failure(*reason),
            ConnectError::CreateFailed { reason, .. } => Self::from_create_failure(*reason),
            ConnectError::Refused { reason, .. } => Self::from_refusal(*reason),
            ConnectError::InvalidSessionPrefix { .. } => Self::InvalidRequest,
            ConnectError::MinorTooOld { .. } => Self::Unsupported,
            ConnectError::Io(_)
            | ConnectError::Connect(_)
            | ConnectError::Transport(_)
            | ConnectError::Preface(_)
            | ConnectError::EofBeforeWelcome
            | ConnectError::EofMidAttach => Self::DaemonLost,
            ConnectError::Codec(_)
            | ConnectError::UnexpectedKind { .. }
            | ConnectError::NotWelcome
            | ConnectError::MajorMismatch { .. }
            | ConnectError::UnknownPrefaceStatus { .. }
            | ConnectError::AcceptedUnofferedMajor { .. }
            | ConnectError::NotSessionAttached
            | ConnectError::InadmissibleRefusal { .. }
            | ConnectError::Abandoned
            | ConnectError::StreamRefused { .. }
            | ConnectError::Driver(_) => Self::Protocol,
        }
    }

    /// The one connection-refusal → kind table. Neither `Role` nor
    /// `UnknownMode` is a typed refusal of the request: the daemon
    /// rejected the connection's stated mode, so this build never got
    /// to ask, and the caller's remedy is the same as for a daemon that
    /// never answered.
    pub(crate) const fn from_refusal(reason: RefusalReason) -> Self {
        match reason {
            RefusalReason::AtCapacity => Self::AtCapacity,
            RefusalReason::Role | RefusalReason::UnknownMode => Self::DaemonUnreachable,
        }
    }

    /// The one "which session?" table, whichever side resolved the
    /// prefix and whichever surface reports it.
    pub(crate) const fn from_prefix_error(err: &crate::SessionPrefixError) -> Self {
        match err {
            crate::SessionPrefixError::NoMatch { .. } => Self::NoMatch,
            crate::SessionPrefixError::Ambiguous { .. } => Self::Ambiguous,
        }
    }
}

/// `kind` is published as an open string, not an `enum`: the vocabulary
/// grows additively within an epoch, so a consumer's validator must not
/// reject an object carrying a token minted after it was written.
#[cfg(feature = "schema")]
pub(crate) struct OpenErrorKind;

#[cfg(feature = "schema")]
impl schemars::JsonSchema for OpenErrorKind {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ErrorKind".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "An additive failure vocabulary; `x-known-values` lists this epoch's tokens.",
            "x-known-values": ErrorKind::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct MachineError {
    #[cfg_attr(feature = "schema", schemars(with = "OpenErrorKind"))]
    pub(crate) kind: ErrorKind,
    pub(crate) message: String,
    /// Sessions the daemon still holds, on `daemon stop`'s refusal
    /// alone: the count is what the caller decides between `--force`
    /// and `--when-empty` on, and re-reading it out of the message
    /// would make prose a parse target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sessions: Option<u32>,
    /// Why the daemon refused, on `daemon upgrade`'s refusal alone:
    /// `unsupported`, `dump_version`, `protocol_major`, `timeout`,
    /// `probe_failed`, `busy`, or `draining`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<&'static str>,
}

#[derive(Serialize)]
struct Versioned<'a, T> {
    v: u32,
    #[serde(flatten)]
    body: &'a T,
}

/// A point verb's failure object: stderr, paired with the exit code.
#[derive(Serialize)]
struct ErrorObject<'a> {
    v: u32,
    error: &'a MachineError,
}

#[derive(Serialize)]
struct EndTerminal {
    v: u32,
    event: &'static str,
    count: u64,
    /// The range-closing `OSC 133 ; D` mark's code, where the verb has
    /// one. Absent otherwise, as everywhere else on this surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<u32>,
}

/// A stream's failing terminal: the point-verb error body under the
/// `event` a consumer watches for.
#[derive(Serialize)]
struct ErrorTerminal<'a> {
    v: u32,
    event: &'static str,
    error: &'a MachineError,
}

/// In-band, non-terminal: the subscription dropped `dropped` items and
/// continues.
#[derive(Serialize)]
struct LagEvent {
    v: u32,
    event: &'static str,
    dropped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Point,
    Stream,
}

/// Everything structured a verb emits goes through here, so the
/// per-class rules are enforced once instead of at every exit.
pub(crate) struct Reporter {
    format: Format,
    class: Class,
    /// A stalled drain and the natural end can both reach a terminal;
    /// two would break the one contract a consumer cannot recover from.
    /// Atomic rather than a `Cell` so the reporter is `Sync`: a verb
    /// body is a boxed `Send` future sharing this with its dispatch.
    terminated: AtomicBool,
}

impl Reporter {
    pub(crate) const fn point(format: Format) -> Self {
        Self {
            format,
            class: Class::Point,
            terminated: AtomicBool::new(false),
        }
    }

    pub(crate) const fn stream(format: Format) -> Self {
        Self {
            format,
            class: Class::Stream,
            terminated: AtomicBool::new(false),
        }
    }

    pub(crate) const fn machine(&self) -> bool {
        self.format.is_machine()
    }

    pub(crate) fn result(&self, body: &impl Serialize) {
        debug_assert_eq!(self.class, Class::Point, "result is a point verb's answer");
        if self.machine() {
            println!(
                "{}",
                render(&Versioned {
                    v: SURFACE_VERSION,
                    body
                })
            );
        }
    }

    pub(crate) fn item(&self, body: &impl Serialize) {
        debug_assert_eq!(self.class, Class::Stream, "items belong to a stream");
        if self.machine() && !self.terminated.load(Ordering::Relaxed) {
            println!(
                "{}",
                render(&Versioned {
                    v: SURFACE_VERSION,
                    body
                })
            );
        }
    }

    /// Human framing reports lag on stderr: a diagnostic there, not one
    /// of the lines the user asked for.
    pub(crate) fn lag(&self, dropped: u64) {
        debug_assert_eq!(self.class, Class::Stream, "lag belongs to a stream");
        if self.machine() {
            if !self.terminated.load(Ordering::Relaxed) {
                println!(
                    "{}",
                    render(&LagEvent {
                        v: SURFACE_VERSION,
                        event: "lag",
                        dropped,
                    })
                );
            }
        } else {
            eprintln!("dropped {dropped} events (the daemon's ring overflowed)");
        }
    }

    pub(crate) fn end(&self, count: u64, exit_code: Option<u32>) {
        debug_assert_eq!(self.class, Class::Stream, "end closes a stream");
        if self.machine() && !self.terminated.swap(true, Ordering::Relaxed) {
            println!(
                "{}",
                render(&EndTerminal {
                    v: SURFACE_VERSION,
                    event: "end",
                    count,
                    exit_code,
                })
            );
        }
    }

    /// A point verb's failure is an error object on stderr beside its
    /// exit code; a stream's is its one terminal on stdout, a failure
    /// before the first item included. The status is the kind's, never
    /// the call site's.
    pub(crate) fn fail(&self, kind: ErrorKind, message: impl std::fmt::Display) -> i32 {
        let code = kind.exit_code();
        let message = message.to_string();
        if !self.machine() {
            eprintln!("{message}");
            return code;
        }
        let error = MachineError {
            kind,
            message,
            sessions: None,
            reason: None,
        };
        match self.class {
            Class::Point => eprintln!(
                "{}",
                render(&ErrorObject {
                    v: SURFACE_VERSION,
                    error: &error,
                })
            ),
            Class::Stream => {
                if !self.terminated.swap(true, Ordering::Relaxed) {
                    println!(
                        "{}",
                        render(&ErrorTerminal {
                            v: SURFACE_VERSION,
                            event: "error",
                            error: &error,
                        })
                    );
                }
            }
        }
        code
    }

    /// `daemon stop`'s refusal: the ordinary `refused` error object
    /// plus the count that names the remedy.
    pub(crate) fn fail_refusal(&self, sessions: u32, message: String) -> i32 {
        self.refused(&MachineError {
            kind: ErrorKind::Refused,
            message,
            sessions: Some(sessions),
            reason: None,
        })
    }

    /// `daemon upgrade`'s refusal: the ordinary `refused` error object
    /// plus the daemon's reason token.
    pub(crate) fn fail_upgrade_refusal(&self, reason: &'static str, message: String) -> i32 {
        self.refused(&MachineError {
            kind: ErrorKind::Refused,
            message,
            sessions: None,
            reason: Some(reason),
        })
    }

    fn refused(&self, error: &MachineError) -> i32 {
        debug_assert!(self.machine(), "the human framing prints its own sentence");
        eprintln!(
            "{}",
            render(&ErrorObject {
                v: SURFACE_VERSION,
                error,
            })
        );
        error.kind.exit_code()
    }

    pub(crate) fn line(&self, text: impl std::fmt::Display) {
        if !self.machine() {
            println!("{text}");
        }
    }
}

/// `short_id` is a display form (the git-style unique prefix) and never
/// a persistence target: prefix uniqueness is a property of the roster
/// it was computed against, so a stored prefix can match a younger
/// session later (docs/reference/cli.md "Machine output").
#[must_use]
pub(crate) fn short_id_in(id: u128, roster: &[SessionInfo]) -> String {
    short_session_prefix(id, roster.iter().map(|s| s.id))
}

/// For human output with no roster to disambiguate against: the
/// floor-length prefix, what a display would show if nothing collided.
/// Machine results never carry it, because a prefix no roster
/// established is an unchecked claim of uniqueness
/// (docs/reference/cli.md "Session identity").
#[must_use]
pub(crate) fn short_id_alone(id: u128) -> String {
    let hex = SessionHex(id).to_string();
    hex.get(..SHORT_SESSION_PREFIX_MIN)
        .unwrap_or(&hex)
        .to_owned()
}

/// The body of a `sessions list` item and a `sessions info` result
/// alike, and what `felis bridge` wraps.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct SessionObject {
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    pub(crate) id: String,
    pub(crate) short_id: String,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) idle_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<String>,
    /// Always present, empty for an untagged session, so a `jq` picker
    /// can rely on `.tags` being iterable.
    pub(crate) tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_notification: Option<LastNotificationObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) foreground: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_exit_code: Option<u32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) exited: bool,
    /// Always present, like `tags`: a picker's `.attachments[]` must be
    /// iterable on a parked session too.
    pub(crate) attachments: Vec<AttachmentObject>,
}

impl SessionObject {
    /// A single-session view passes the same roster the listing did, so
    /// both surfaces show one session the same `short_id`.
    pub(crate) fn new(s: &SessionInfo, roster: &[SessionInfo]) -> Self {
        Self::with_short_id(s, short_id_in(s.id, roster))
    }

    /// The `Ops::Info` form: the daemon shortened the id against its
    /// own pool, so there is no roster here to shorten against.
    pub(crate) fn with_short_id(s: &SessionInfo, short_id: String) -> Self {
        Self {
            id: SessionHex(s.id).to_string(),
            short_id,
            rows: s.dims.rows,
            cols: s.dims.cols,
            idle_seconds: s.idle_seconds,
            title: s.title.clone(),
            cwd: s.cwd.clone(),
            tags: s.tags.clone(),
            last_notification: s
                .last_notification
                .as_ref()
                .map(|n| LastNotificationObject {
                    title: n.notification.title.clone(),
                    body: n.notification.body.clone(),
                    urgency: n.notification.urgency.as_str().to_owned(),
                    age_seconds: n.age_seconds,
                }),
            foreground: s.foreground.clone(),
            last_exit_code: s.last_exit_code,
            exited: s.exited,
            attachments: s
                .attachments
                .iter()
                .map(|a| AttachmentObject {
                    id: a.id.to_string(),
                    attached_at: crate::timestamp::rfc3339_utc(a.attached_at),
                    input_owner: a.input_owner,
                })
                .collect(),
        }
    }
}

/// `sessions list`'s whole answer. The roster is bounded and arrives in
/// one frame, so it is a point result: a stream framing would promise a
/// source that can grow or arrive incrementally, and this one cannot
/// (docs/explanation/architecture/control-surfaces.md).
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct ListResult {
    /// Always present, empty for an empty roster.
    pub(crate) sessions: Vec<SessionObject>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct LastNotificationObject {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    pub(crate) body: String,
    pub(crate) urgency: String,
    pub(crate) age_seconds: u64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct AttachmentObject {
    /// A decimal **string**: the id is a `u64` counter, and a bare JSON
    /// number past 2^53−1 silently rounds in common consumers.
    /// `--attachment` accepts the same spelling.
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9]+$")))]
    pub(crate) id: String,
    /// RFC 3339, UTC.
    pub(crate) attached_at: String,
    /// Unconditional, unlike the optionals elsewhere: an omitted key
    /// would read as "this daemon does not report it".
    pub(crate) input_owner: bool,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct SessionRef {
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    pub(crate) id: String,
    /// `evict`'s flag; absent on other verbs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) was_attached: Option<bool>,
    /// `send --wait`'s observed mark code. Absent for a bare
    /// `OSC 133 ; D`, and on the verbs that never wait.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<u32>,
}

impl SessionRef {
    pub(crate) fn new(id: u128) -> Self {
        Self {
            id: SessionHex(id).to_string(),
            was_attached: None,
            exit_code: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct TagResult {
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    pub(crate) id: String,
    /// Sorted by the daemon and always present, matching `list` / `info`.
    pub(crate) tags: Vec<String>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct SwitchResult {
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    pub(crate) from: String,
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    pub(crate) to: String,
    /// Subscriber outboxes that **took** the push, never the windows
    /// that finished attaching. Exit 0 claims admission and nothing
    /// more: a queued switch can still fail to land, and no field here
    /// or later will say so.
    pub(crate) queued: u32,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct RetargetResult {
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    pub(crate) from: String,
    /// Subscriber outboxes that took the push. As with `switch`, exit 0
    /// claims admission alone; the re-dial onto the target carrier runs
    /// afterwards, out of this invocation's sight.
    pub(crate) queued: u32,
}

/// The session count is not a field of its own: it is the `sessions`
/// row's `total_used`, and two spellings of one number can disagree.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct DaemonStatusResult {
    pub(crate) version: String,
    pub(crate) protocol: ProtocolVersion,
    pub(crate) worker_threads: u32,
    /// The daemon refuses creates and exits after its last session.
    pub(crate) draining: bool,
    /// The order is presentation, not contract; a consumer keys on
    /// `resource`.
    pub(crate) resources: Vec<ResourceObject>,
}

/// `daemon stop`'s answer. The mode rides beside the outcome so a
/// reader can tell a stop that destroyed sessions from one that found
/// none.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct DaemonStopResult {
    /// `stopping`, `refused`, or `draining`.
    pub(crate) outcome: &'static str,
    /// `if_empty`, `force`, or `when_empty`: what was asked for, not
    /// what the daemon decided.
    pub(crate) mode: &'static str,
    /// Sessions the outcome speaks about: those still held on
    /// `draining`, `0` on `stopping`.
    pub(crate) sessions: u32,
}

/// `daemon upgrade`'s answer. A refusal is an error object instead,
/// so `outcome` has one value today; it is spelled out for a reader
/// that dispatches on it as it does on `daemon stop`'s.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct DaemonUpgradeResult {
    /// `upgrading`.
    pub(crate) outcome: &'static str,
    /// The absolute path of the `felis-daemon` the daemon execs.
    pub(crate) successor: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct ProtocolVersion {
    pub(crate) major: u16,
    pub(crate) minor: u16,
}

/// Each number names its own denominator, so a consumer never has to
/// infer one: `total_used` belongs with `global_limit`,
/// `max_subject_used` with `per_subject_limit`, and the two pairs are
/// never mixed.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct ResourceObject {
    /// The stable row key, never a display label.
    pub(crate) resource: String,
    /// `count` or `bytes`.
    pub(crate) unit: String,
    /// `daemon`, `session`, or `subscriber`: what one subject is.
    pub(crate) scope: String,
    /// Summed over every subject. Present on every row.
    pub(crate) total_used: u64,
    /// Absent (not null) on a `daemon`-scope row, whose only subject is
    /// `total_used`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_subject_used: Option<u64>,
    /// Absent on a `daemon`-scope row and where felis admits a subject
    /// without a ceiling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) per_subject_limit: Option<u64>,
    /// Absent where felis budgets no daemon-wide aggregate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) global_limit: Option<u64>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct ConfigPathResult {
    pub(crate) path: String,
    /// The path is reported either way.
    pub(crate) exists: bool,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct CheckResult {
    pub(crate) path: String,
    pub(crate) exists: bool,
    pub(crate) client: String,
    pub(crate) errors: u64,
    pub(crate) warnings: u64,
    /// Always present, empty for a clean file.
    pub(crate) diagnostics: Vec<DiagnosticObject>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct DiagnosticObject {
    /// `error` or `warning`; only the first moves the exit code.
    pub(crate) severity: String,
    /// The category token: `io`, `parse`, `unknown_key`, `value`,
    /// `missing_file`.
    pub(crate) kind: String,
    /// The dotted key path **as written in the document**, so a typo
    /// inside an overlay reads `client.<id>.<path>`. Absent for a
    /// diagnostic about the file rather than a key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) key: Option<String>,
    pub(crate) message: String,
}

/// The config sits under its own key rather than flattened, so `v` and
/// the provenance cannot collide with a config section name.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct EffectiveConfigResult {
    pub(crate) path: String,
    pub(crate) client: String,
    pub(crate) config: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct DoctorResult {
    pub(crate) failed: u64,
    pub(crate) warned: u64,
    pub(crate) checks: Vec<CheckObject>,
}

/// `felis doctor report`: the checklist plus the environment, with every
/// path under `$HOME` already shown as `~`.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct DoctorReportResult {
    pub(crate) failed: u64,
    pub(crate) warned: u64,
    pub(crate) checks: Vec<CheckObject>,
    pub(crate) environment: EnvironmentObject,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct EnvironmentObject {
    pub(crate) builds: BuildsObject,
    /// This `felis` binary's path: tells a Nix store, Homebrew, or a
    /// source build apart.
    pub(crate) binary: Option<String>,
    pub(crate) os: OsObject,
    pub(crate) display: DisplayObject,
    /// `null` when the GUI frontend could not be probed.
    pub(crate) gpu: Option<GpuObject>,
    pub(crate) locale: LocaleObject,
    pub(crate) shell: ShellObject,
    pub(crate) fonts: FontsObject,
    pub(crate) config: ConfigObject,
    pub(crate) logs: Vec<LogObject>,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct BuildsObject {
    pub(crate) cli: String,
    /// `null` when the GUI frontend could not be probed.
    pub(crate) client: Option<String>,
    /// `null` when no daemon answered with an identity; the `daemon`
    /// check says why.
    pub(crate) daemon: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct OsObject {
    /// `std::env::consts::OS`: `linux`, `macos`, `windows`, …
    pub(crate) family: String,
    pub(crate) arch: String,
    /// The distribution or product version, where this platform names
    /// one.
    pub(crate) release: Option<String>,
    pub(crate) kernel: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct DisplayObject {
    /// `XDG_SESSION_TYPE`.
    pub(crate) session_type: Option<String>,
    /// `XDG_CURRENT_DESKTOP`.
    pub(crate) desktop: Option<String>,
    /// Whether `WAYLAND_DISPLAY` is set; its value is not reported.
    pub(crate) wayland: bool,
    /// Whether `DISPLAY` is set; its value is not reported.
    pub(crate) x11: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct GpuObject {
    pub(crate) available: bool,
    pub(crate) name: Option<String>,
    pub(crate) backend: Option<String>,
    pub(crate) device_type: Option<String>,
    pub(crate) driver: Option<String>,
    pub(crate) driver_info: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct LocaleObject {
    pub(crate) lang: Option<String>,
    pub(crate) lc_all: Option<String>,
    pub(crate) lc_ctype: Option<String>,
}

/// The shell `felis doctor report` ran in, which is the affected
/// session's only when `inside_felis` is true.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct ShellObject {
    /// Whether `FELIS_SESSION_ID` is set: the daemon stamps it into every
    /// session it spawns.
    pub(crate) inside_felis: bool,
    pub(crate) term: Option<String>,
    pub(crate) term_program: Option<String>,
    pub(crate) term_program_version: Option<String>,
    pub(crate) colorterm: Option<String>,
    /// Whether `SSH_CONNECTION` is set; its value is not reported.
    pub(crate) ssh: bool,
}

/// The faces a window would draw with. Exactly one of the face fields
/// and `unavailable` is set.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FontsObject {
    pub(crate) regular: Option<String>,
    pub(crate) bold: Option<String>,
    pub(crate) italic: Option<String>,
    pub(crate) bold_italic: Option<String>,
    pub(crate) fallbacks: Vec<String>,
    /// Why no stack is reported: the frontend could not be probed,
    /// predates the font probe, or resolved no font.
    pub(crate) unavailable: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct ConfigObject {
    /// `null` when felis found no home directory to look in.
    pub(crate) path: Option<String>,
    /// `absent` (no file; the defaults apply), `applied`, or `invalid`
    /// (the file has errors and the defaults apply instead).
    pub(crate) state: String,
    /// The keys whose values differ from the defaults, nested as in
    /// `config.toml`. `[client.*]` is never included, and the arguments
    /// of `send_string`, `run`, and command or file `pipe` bindings read
    /// `<redacted>`.
    pub(crate) diff: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct LogObject {
    /// `daemon.log` or `client.log`.
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) present: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct CheckObject {
    /// The check's stable token (`daemon`, `daemon-sibling`, `config`,
    /// `terminfo`, `gpu`, `clipboard`, `remote_helper`).
    pub(crate) check: String,
    /// `ok`, `warn`, `fail`, or `skipped`; the last is neither a pass
    /// nor a problem.
    pub(crate) status: String,
    pub(crate) detail: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct CaptureRow<'a> {
    /// The region's own coordinate space: scrollback negative (`-1` the
    /// youngest), live rows their grid index, mark ranges from 0.
    pub(crate) row: i64,
    pub(crate) text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ansi: Option<&'a str>,
    pub(crate) soft_wrap_continued: bool,
}

/// `col_spans` are per-row highlight segments: a match crossing a
/// soft-wrap edge emits one per touched row, so they are not 1:1 with
/// `byte_spans`.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct SearchMatch<'a> {
    pub(crate) line_index: i64,
    pub(crate) text: &'a str,
    pub(crate) byte_spans: Vec<[u32; 2]>,
    pub(crate) col_spans: Vec<[i64; 3]>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct NotificationObject {
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{32}$")))]
    pub(crate) session_id: String,
    /// Explicitly `null` rather than omitted, here and below: this
    /// object is the whole of what a subscriber learns about an event.
    pub(crate) title: Option<String>,
    pub(crate) body: String,
    pub(crate) urgency: String,
    pub(crate) notification_id: Option<String>,
    pub(crate) session_title: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) attached: bool,
}

impl NotificationObject {
    pub(crate) fn new(
        session_id: u128,
        notification: &felis_protocol::messages::Notification,
        notify_id: Option<&str>,
        session_title: Option<&str>,
        cwd: Option<&str>,
        attached: bool,
    ) -> Self {
        Self {
            session_id: SessionHex(session_id).to_string(),
            title: notification.title.clone(),
            body: notification.body.clone(),
            urgency: notification.urgency.as_str().to_owned(),
            notification_id: notify_id.map(str::to_owned),
            session_title: session_title.map(str::to_owned),
            cwd: cwd.map(str::to_owned),
            attached,
        }
    }
}

/// A payload body with no `"v"`, for `felis bridge`'s own envelope. A
/// `Value` rather than rendered text so the bridge composes one object
/// instead of splicing fragments. Serialization cannot fail (see
/// [`render`]); a failure would surface as a `null` body.
pub(crate) fn body_value(value: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks a wire enum from `first` through an exhaustive successor
    /// match. A hand-written array would let a new wire variant miss
    /// the frozen table silently; linking the variants instead makes
    /// the compiler reject the enum's growth until the new one is
    /// placed in the chain.
    fn chain<T: Copy>(first: T, next: impl Fn(T) -> Option<T>) -> Vec<T> {
        let mut out = vec![first];
        let mut current = first;
        while let Some(following) = next(current) {
            out.push(following);
            current = following;
        }
        out
    }

    fn stream_reasons() -> Vec<StreamErrorReason> {
        chain(StreamErrorReason::InvalidRequest, |reason| match reason {
            StreamErrorReason::InvalidRequest => Some(StreamErrorReason::TooManyStreams),
            StreamErrorReason::TooManyStreams => Some(StreamErrorReason::Unavailable),
            StreamErrorReason::Unavailable => Some(StreamErrorReason::Internal),
            StreamErrorReason::Internal => None,
        })
    }

    fn attach_failures() -> Vec<AttachFailure> {
        chain(AttachFailure::UnknownSession, |reason| match reason {
            AttachFailure::UnknownSession => Some(AttachFailure::SessionEnding),
            AttachFailure::SessionEnding => Some(AttachFailure::SessionExited),
            AttachFailure::SessionExited => Some(AttachFailure::NoMatch),
            AttachFailure::NoMatch => Some(AttachFailure::Ambiguous),
            AttachFailure::Ambiguous => None,
        })
    }

    fn create_failures() -> Vec<CreateFailure> {
        chain(CreateFailure::SpawnFailed, |reason| match reason {
            CreateFailure::SpawnFailed => Some(CreateFailure::GeometryOutOfRange),
            CreateFailure::GeometryOutOfRange => Some(CreateFailure::SessionLimitReached),
            CreateFailure::SessionLimitReached => Some(CreateFailure::DaemonDraining),
            CreateFailure::DaemonDraining => None,
        })
    }

    /// One `ConnectError` per row of the frozen table. Not every
    /// variant: the ones a test cannot build without reaching into
    /// another crate's error types all answer `protocol`, and the
    /// exhaustive match in `from_connect_error` is what keeps a new one
    /// from taking that answer by default.
    fn connect_errors() -> Vec<(&'static str, ConnectError)> {
        use felis_protocol::messages::AttachRefusal;

        vec![
            (
                "AttachFailed(UnknownSession)",
                ConnectError::AttachFailed {
                    reason: AttachFailure::UnknownSession,
                    detail: String::new(),
                },
            ),
            (
                "AttachFailed(NoMatch)",
                ConnectError::AttachFailed {
                    reason: AttachFailure::NoMatch,
                    detail: String::new(),
                },
            ),
            (
                "AttachFailed(Ambiguous)",
                ConnectError::AttachFailed {
                    reason: AttachFailure::Ambiguous,
                    detail: String::new(),
                },
            ),
            (
                "CreateFailed(SpawnFailed)",
                ConnectError::CreateFailed {
                    reason: CreateFailure::SpawnFailed,
                    detail: String::new(),
                },
            ),
            (
                "CreateFailed(SessionLimitReached)",
                ConnectError::CreateFailed {
                    reason: CreateFailure::SessionLimitReached,
                    detail: String::new(),
                },
            ),
            (
                "Refused(Role)",
                ConnectError::Refused {
                    reason: RefusalReason::Role,
                    detail: String::new(),
                },
            ),
            (
                "Refused(AtCapacity)",
                ConnectError::Refused {
                    reason: RefusalReason::AtCapacity,
                    detail: String::new(),
                },
            ),
            (
                "InvalidSessionPrefix",
                ConnectError::InvalidSessionPrefix {
                    prefix: String::new(),
                    reason: String::new(),
                },
            ),
            (
                "MinorTooOld",
                ConnectError::MinorTooOld {
                    needs: 1,
                    effective: 0,
                    feature: "a later addition",
                },
            ),
            ("EofBeforeWelcome", ConnectError::EofBeforeWelcome),
            ("EofMidAttach", ConnectError::EofMidAttach),
            (
                "Connect",
                ConnectError::Connect(std::io::Error::other("no socket")),
            ),
            ("NotWelcome", ConnectError::NotWelcome),
            ("NotSessionAttached", ConnectError::NotSessionAttached),
            (
                "InadmissibleRefusal",
                ConnectError::InadmissibleRefusal {
                    ctx: felis_client_core::SessionRefusalContext::Create,
                    reason: AttachRefusal::Attach(AttachFailure::NoMatch),
                    detail: String::new(),
                },
            ),
            ("Abandoned", ConnectError::Abandoned),
            (
                "StreamRefused",
                ConnectError::StreamRefused {
                    subject: String::new(),
                    reason: StreamErrorReason::Unavailable,
                    detail: String::new(),
                },
            ),
        ]
    }

    fn refusal_reasons() -> Vec<RefusalReason> {
        chain(RefusalReason::Role, |reason| match reason {
            RefusalReason::Role => Some(RefusalReason::AtCapacity),
            RefusalReason::AtCapacity => Some(RefusalReason::UnknownMode),
            RefusalReason::UnknownMode => None,
        })
    }

    /// The frozen error surface, rendered from the maps themselves.
    /// `crates/felis-cli/tests/golden/error_kinds.txt` holds the same
    /// text, so the error vocabulary and reason mappings are derived
    /// rather than hand-synced.
    fn render_error_surface() -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        out.push_str("# Rendered by felis-cli's `error_surface_matches_the_golden` test.\n");
        out.push_str("# The frozen CLI error-kind surface.\n");
        out.push_str("\n[kind -> exit]\n");
        for kind in ErrorKind::ALL {
            let _ = writeln!(out, "{:<20} {}", kind.as_str(), kind.exit_code());
        }
        out.push_str("\n[StreamErrorReason -> kind]\n");
        for reason in stream_reasons() {
            let _ = writeln!(
                out,
                "{:<20} {}",
                format!("{reason:?}"),
                ErrorKind::from_stream_reason(reason).as_str()
            );
        }
        out.push_str("\n[AttachFailure -> kind]\n");
        for reason in attach_failures() {
            let _ = writeln!(
                out,
                "{:<20} {}",
                format!("{reason:?}"),
                ErrorKind::from_attach_failure(reason).as_str()
            );
        }
        out.push_str("\n[CreateFailure -> kind]\n");
        for reason in create_failures() {
            let _ = writeln!(
                out,
                "{:<20} {}",
                format!("{reason:?}"),
                ErrorKind::from_create_failure(reason).as_str()
            );
        }
        out.push_str("\n[RefusalReason -> kind]\n");
        for reason in refusal_reasons() {
            let _ = writeln!(
                out,
                "{:<20} {}",
                format!("{reason:?}"),
                ErrorKind::from_refusal(reason).as_str()
            );
        }
        out.push_str("\n[ConnectError -> kind]\n");
        for (name, err) in connect_errors() {
            let _ = writeln!(
                out,
                "{name:<34} {}",
                ErrorKind::from_connect_error(&err).as_str()
            );
        }
        out
    }

    /// Every kind's exit code and every wire reason's kind, against the
    /// committed fixture.
    #[test]
    fn error_surface_matches_the_golden() {
        assert_eq!(
            render_error_surface(),
            include_str!("../tests/golden/error_kinds.txt"),
            "the CLI error surface moved; update \
             crates/felis-cli/tests/golden/error_kinds.txt"
        );
    }

    /// Two kinds sharing a token would collapse into one table row.
    #[test]
    fn every_kind_has_its_own_token() {
        let mut tokens: Vec<&str> = ErrorKind::ALL.iter().map(|k| k.as_str()).collect();
        let listed = tokens.len();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), listed);
    }

    fn session(
        idle_seconds: Option<u64>,
        attachments: Vec<felis_protocol::messages::Attachment>,
    ) -> SessionInfo {
        SessionInfo {
            id: 0xCAFE,
            dims: felis_protocol::messages::GridDims {
                rows: 24,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            },
            title: None,
            cwd: None,
            idle_seconds,
            tags: Vec::new(),
            last_notification: None,
            foreground: None,
            exited: false,
            last_exit_code: None,
            attachments,
            sequence: std::num::NonZeroU64::MIN,
        }
    }

    /// The wire carries an instant; the machine surface publishes an
    /// RFC 3339 string, and an attached session carries no
    /// `idle_seconds` key at all.
    #[test]
    fn an_attachment_renders_its_stamp_as_rfc_3339() {
        let info = session(
            None,
            vec![felis_protocol::messages::Attachment {
                id: 7,
                attached_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_788_250_500),
                input_owner: true,
            }],
        );
        let body = body_value(&SessionObject::new(&info, std::slice::from_ref(&info)));
        assert_eq!(
            body["attachments"][0]["attached_at"],
            "2026-09-01T08:15:00Z"
        );
        assert_eq!(body.get("idle_seconds"), None);
    }

    /// `idle_seconds: Some(0)` is "detached under a second ago", not
    /// "attached": both framings publish that one meaning.
    #[test]
    fn a_just_detached_session_reads_the_same_in_both_framings() {
        let info = session(Some(0), Vec::new());
        let body = body_value(&SessionObject::new(&info, std::slice::from_ref(&info)));
        assert_eq!(body["idle_seconds"], 0);
        assert_eq!(
            crate::format_session_line(&info, "0000cafe"),
            "0000cafe  24x80  0s"
        );
    }

    /// Each class publishes its own framings and no others.
    #[test]
    fn each_class_publishes_only_its_own_framings() {
        use clap::ValueEnum as _;

        fn spellings<T: clap::ValueEnum>() -> Vec<String> {
            T::value_variants()
                .iter()
                .map(|v| v.to_possible_value().unwrap().get_name().to_owned())
                .collect()
        }
        assert_eq!(spellings::<PointFormatValue>(), ["human", "json"]);
        assert_eq!(spellings::<StreamFormatValue>(), ["human", "jsonl"]);

        assert_eq!(Format::from(PointFormatValue::Json), Format::Json);
        assert_eq!(Format::from(StreamFormatValue::Jsonl), Format::Jsonl);
        assert!(PointFormatValue::from_str("jsonl", false).is_err());
        assert!(StreamFormatValue::from_str("json", false).is_err());
    }

    #[test]
    fn every_top_level_object_carries_the_version() {
        let body = SessionRef::new(0xCAFE);
        assert!(
            render(&Versioned {
                v: SURFACE_VERSION,
                body: &body
            })
            .starts_with(r#"{"v":1,"#)
        );
        let error = MachineError {
            kind: ErrorKind::NoMatch,
            message: "nope".into(),
            sessions: None,
            reason: None,
        };
        assert_eq!(
            render(&ErrorObject {
                v: SURFACE_VERSION,
                error: &error
            }),
            r#"{"v":1,"error":{"kind":"no_match","message":"nope"}}"#
        );
        assert_eq!(
            render(&ErrorTerminal {
                v: SURFACE_VERSION,
                event: "error",
                error: &error
            }),
            r#"{"v":1,"event":"error","error":{"kind":"no_match","message":"nope"}}"#
        );
        // `daemon stop`'s refusal is the one error object with a count,
        // and `daemon upgrade`'s the one with a reason: each present
        // only there, so no other error grows a key.
        assert_eq!(
            render(&ErrorObject {
                v: SURFACE_VERSION,
                error: &MachineError {
                    kind: ErrorKind::Refused,
                    message: "held".into(),
                    sessions: Some(2),
                    reason: None,
                }
            }),
            r#"{"v":1,"error":{"kind":"refused","message":"held","sessions":2}}"#
        );
        assert_eq!(
            render(&ErrorObject {
                v: SURFACE_VERSION,
                error: &MachineError {
                    kind: ErrorKind::Refused,
                    message: "busy".into(),
                    sessions: None,
                    reason: Some("busy"),
                }
            }),
            r#"{"v":1,"error":{"kind":"refused","message":"busy","reason":"busy"}}"#
        );
        assert_eq!(
            render(&EndTerminal {
                v: SURFACE_VERSION,
                event: "end",
                count: 3,
                exit_code: None
            }),
            r#"{"v":1,"event":"end","count":3}"#
        );
        assert_eq!(
            render(&LagEvent {
                v: SURFACE_VERSION,
                event: "lag",
                dropped: 7
            }),
            r#"{"v":1,"event":"lag","dropped":7}"#
        );
    }

    #[test]
    fn payload_strings_are_escaped() {
        let object = TagResult {
            id: "0".repeat(32),
            tags: vec!["a\"b\\c\nd\u{1}".into()],
        };
        assert!(
            render(&object).contains(r#""a\"b\\c\nd\u0001""#),
            "{}",
            render(&object)
        );
    }

    #[test]
    fn a_rosterless_display_shows_the_floor_length_prefix() {
        assert_eq!(short_id_alone(0xCAFE), "00000000");
        assert_eq!(
            short_id_alone(0xCAFE_0000_0000_0000_0000_0000_0000_0000),
            "cafe0000"
        );
    }
}

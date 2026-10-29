//! The JSON protocol's messages (`docs/reference/json-protocol.md`): the
//! version handshake, point requests and their replies, streams with
//! exactly one terminal each, and the payloads felis owns inside them.
//! Host routing is not here: a relay serving several daemons wraps
//! these messages in an envelope of its own.

use std::collections::BTreeMap;

use felis_protocol::messages::{
    AttachFailure, AttachRefusal, AttachTarget as DomainAttachTarget, CreateFailure,
    NotifyToClientMsg, NotifyToDaemonMsg, OpsToDaemonMsg, RegionSource, RegionToClientMsg,
    RegionToDaemonMsg, SearchOptions, SearchSource, SearchToClientMsg, SearchToDaemonMsg,
    SendRefusal, SendStep, SessionToDaemonMsg, StreamErrorReason, Urgency,
};

use super::common::{Base64, DtoError, SessionId, ViewportAnchorJson, session_id_hex};
use super::grid::GridJson;
use super::image::ImageJson;
use super::input::{InputJson, KeyEventJson, MouseEventJson};
use super::session::{SessionObject, SessionRef, TagResult};
use super::spawn::SpawnArgsJson;

/// Every protocol version this build reads and writes, ascending.
pub const SUPPORTED_VERSIONS: [u32; 1] = [1];

/// The key a client correlates a request or a stream by. Client-chosen
/// and unique among the requests and streams it has open; a `u32`
/// because every JSON reader holds it exactly.
pub type CorrelationId = u32;

json_dto! {
    /// The first message a client sends. Its shape is the same in every
    /// version, so a writer can read it before any version is agreed.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ClientHello {
        Hello {
            /// The versions the client reads, in any order.
            versions: Vec<u32>,
        },
    }

    /// The writer's answer to [`ClientHello`], shaped alike in every
    /// version.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ServerHello {
        /// Every later message is spelled as `version` names.
        Welcome { version: u32 },
        /// No offered version is one this writer speaks; nothing follows.
        Refused {
            /// The versions this writer speaks.
            versions: Vec<u32>,
            /// Human-readable specifics. Never parsed.
            message: String,
        },
    }

    /// One session-addressed point operation.
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum Request {
        /// Answered by [`OpResult::List`].
        List,
        /// Answered by [`OpResult::Info`].
        Info {
            session: SessionId,
        },
        /// Spawns a session nobody attaches to; answered by
        /// [`OpResult::Create`].
        Create { args: SpawnArgsJson },
        /// Answered by [`OpResult::Kill`].
        Kill {
            session: SessionId,
        },
        /// Answered by [`OpResult::Tag`].
        Tag {
            session: SessionId,
            #[serde(default)]
            add: Vec<String>,
            #[serde(default)]
            remove: Vec<String>,
        },
        /// Writes every step in order, or none when any is refused;
        /// answered by [`OpResult::Send`] once the session took the last.
        Send {
            session: SessionId,
            steps: Vec<SendStepJson>,
        },
    }

    #[derive(Default)]
    #[serde(rename_all = "snake_case")]
    pub enum SearchSourceJson {
        /// Scrollback plus the live primary screen.
        #[default]
        Scrollback,
        /// The live grid alone.
        Visible,
    }

    #[serde(rename_all = "snake_case")]
    pub enum RegionSourceJson {
        Scrollback,
        Visible,
        /// The most recent `OSC 133 C → D` range.
        CommandOutput,
        /// The most recent `OSC 133 B → D` range.
        LastCommand,
    }

    /// What a stream carries until its terminal.
    #[serde(tag = "stream", rename_all = "snake_case")]
    pub enum Open {
        /// Items: one [`Item::Attached`], then [`Item::Grid`] and
        /// [`Item::Image`]. The client sends input on its id.
        Attach {
            #[serde(flatten)]
            target: AttachTarget,
        },
        /// [`Request::Send`] that then waits for the session's next
        /// `OSC 133 ; D`. Items: one [`Item::Sent`], then one
        /// [`Item::Mark`]. Empty `steps` only wait.
        Wait {
            session: SessionId,
            #[serde(default)]
            steps: Vec<SendStepJson>,
        },
        /// Items: [`Item::Match`], newest line first.
        Search {
            session: SessionId,
            /// A literal substring, or a pattern when `regex` is set.
            query: String,
            #[serde(default)]
            regex: bool,
            #[serde(default)]
            case_insensitive: bool,
            #[serde(default)]
            source: SearchSourceJson,
        },
        /// Items: [`Item::Row`], oldest first; the terminal carries the
        /// region's exit code.
        Capture {
            session: SessionId,
            source: RegionSourceJson,
            /// Also carry each row's SGR reconstruction.
            #[serde(default)]
            ansi: bool,
            /// Only the youngest rows, still oldest first.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            max_rows: Option<u32>,
        },
        /// Items: [`Item::Notification`], with [`ServerMessage::Lag`]
        /// where events were skipped.
        Notifications {
            /// Only this session's notifications.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            session: Option<SessionId>,
        },
    }

    /// One input step of a send, as a window's paste, raw bytes,
    /// keystroke or mouse event would carry it.
    #[serde(tag = "step", rename_all = "snake_case")]
    pub enum SendStepJson {
        Paste { bytes: Base64 },
        Raw { bytes: Base64 },
        Key { event: KeyEventJson },
        Mouse { event: MouseEventJson },
    }

    /// Everything a client sends after the handshake.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ClientMessage {
        Request { id: CorrelationId, request: Request },
        Open { id: CorrelationId, stream: Open },
        /// Input for the attach stream `id` names.
        Input { id: CorrelationId, input: InputJson },
        /// Ends the stream `id` names; its terminal still follows.
        Cancel { id: CorrelationId },
    }

    /// A session row: the object `felis sessions info --format json`
    /// prints, plus the creation order a tab bar sorts on.
    pub struct SessionRow {
        #[serde(flatten)]
        pub session: SessionObject,
        /// The daemon's creation sequence; never `0`.
        #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
        pub sequence: u64,
    }

    /// A successful point reply, tagged with the operation it answers.
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum OpResult {
        List { sessions: Vec<SessionRow> },
        Info { session: Box<SessionRow> },
        Create { session: Box<SessionRow> },
        Kill(SessionRef),
        Tag(TagResult),
        Send(SessionRef),
    }

    #[non_exhaustive]
    pub struct ByteSpanJson {
        pub start: u32,
        /// Half-open.
        pub end: u32,
    }

    /// One highlight segment, on one row.
    #[non_exhaustive]
    pub struct ColSpanJson {
        pub line_index: i64,
        pub col_start: u16,
        /// Half-open.
        pub col_end: u16,
    }

    /// One logical line holding a hit.
    #[non_exhaustive]
    pub struct SearchMatchJson {
        /// The line's topmost row: scrollback negative (`-1` the
        /// youngest), live rows their grid index.
        pub line_index: i64,
        pub text: String,
        /// Byte ranges into `text`.
        pub byte_spans: Vec<ByteSpanJson>,
        /// One per touched row, so not 1:1 with `byte_spans`.
        pub col_spans: Vec<ColSpanJson>,
        /// The history point `line_index` and the segments' rows count
        /// from, which keeps a scrollback hit valid while output
        /// advances; absent from a daemon that does not report it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub anchor: Option<ViewportAnchorJson>,
    }

    /// One region row.
    #[non_exhaustive]
    pub struct CaptureRowJson {
        /// The region's own coordinate space: scrollback negative, live
        /// rows their grid index, mark ranges from `0`.
        pub row: i32,
        /// Trailing blanks trimmed.
        pub text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ansi: Option<String>,
        pub soft_wrap_continued: bool,
    }

    #[serde(rename_all = "snake_case")]
    pub enum UrgencyJson {
        Low,
        Normal,
        Critical,
    }

    /// One desktop notification a session fired.
    #[non_exhaustive]
    pub struct NotificationJson {
        pub session_id: SessionId,
        pub title: Option<String>,
        pub body: String,
        pub urgency: UrgencyJson,
        /// The producer's OSC 99 `i=`. Opaque.
        pub notification_id: Option<String>,
        pub session_title: Option<String>,
        pub cwd: Option<String>,
        /// A window is attached and flashed its own attention.
        pub attached: bool,
    }

    /// One stream item, tagged by what it is.
    #[serde(tag = "item", rename_all = "snake_case")]
    pub enum Item {
        /// An attach stream's first item, whether it attached or created.
        Attached {
            /// The daemon wire minor this stream speaks.
            minor: u32,
            session: Box<SessionRow>,
        },
        Grid { msg: GridJson },
        Image { msg: ImageJson },
        Match(SearchMatchJson),
        Row(CaptureRowJson),
        Notification(NotificationJson),
        /// A wait's steps were all written; carries the full id.
        Sent { id: SessionId },
        /// The `OSC 133 ; D` a wait waited for.
        Mark {
            /// Absent for a bare `D`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            exit_code: Option<u32>,
        },
    }

    /// Why a request or a stream failed. A new token is a new version.
    #[serde(rename_all = "snake_case")]
    pub enum ErrorKind {
        /// The message was malformed or named something it cannot.
        InvalidRequest,
        NoMatch,
        Ambiguous,
        /// The session ended first.
        SessionEnded,
        /// A cap was reached: sessions, streams, connections.
        AtCapacity,
        /// The daemon is too old for the request.
        Unsupported,
        /// The daemon refused on the merits; `message` says why.
        Refused,
        /// Another client took the session away.
        Evicted,
        /// The connection to the daemon ended.
        DaemonLost,
        Internal,
    }

    pub struct ErrorObject {
        pub kind: ErrorKind,
        /// Human-readable specifics. Never parsed.
        pub message: String,
    }

    /// Everything a writer sends after the handshake. A request ends
    /// with exactly one `reply` or `error`; a stream with exactly one
    /// `end`, `error` or `lagged`, after its items.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ServerMessage {
        Reply { id: CorrelationId, result: OpResult },
        Item { id: CorrelationId, item: Item },
        /// Not a terminal: `dropped` items were skipped and the stream
        /// goes on.
        Lag { id: CorrelationId, dropped: u64 },
        /// The stream finished.
        End {
            id: CorrelationId,
            /// Items written.
            count: u64,
            /// A capture region's closing `OSC 133 ; D` code.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            exit_code: Option<u32>,
        },
        /// The request or the stream failed.
        Error { id: CorrelationId, error: ErrorObject },
        /// The stream was cut off because its consumer fell behind.
        Lagged { id: CorrelationId },
    }
}

/// The session an attach stream lands on: exactly one key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", try_from = "AttachKeys")]
pub enum AttachTarget {
    /// An existing session.
    Session(SessionId),
    /// A session spawned for this stream, which the daemon removes
    /// again if the stream cannot start.
    Create(SpawnArgsJson),
}

/// What an attach opener reads before it is checked to name one target:
/// flattened, an enum alone would take the first key and ignore a second.
#[derive(serde::Deserialize)]
struct AttachKeys {
    session: Option<SessionId>,
    create: Option<SpawnArgsJson>,
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for AttachTarget {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AttachTarget".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let session = generator.subschema_for::<SessionId>();
        let create = generator.subschema_for::<SpawnArgsJson>();
        schemars::json_schema!({
            "oneOf": [
                {
                    "description": "An existing session.",
                    "type": "object",
                    "properties": { "session": session },
                    "required": ["session"],
                },
                {
                    "description": "A session spawned for this stream, which the daemon removes again if the stream cannot start.",
                    "type": "object",
                    "properties": { "create": create },
                    "required": ["create"],
                },
            ],
        })
    }
}

impl TryFrom<AttachKeys> for AttachTarget {
    type Error = &'static str;

    fn try_from(keys: AttachKeys) -> Result<Self, &'static str> {
        match (keys.session, keys.create) {
            (Some(session), None) => Ok(Self::Session(session)),
            (None, Some(args)) => Ok(Self::Create(args)),
            _ => Err("an attach names exactly one of `session` and `create`"),
        }
    }
}

plain_enum!(UrgencyJson, Urgency, Low, Normal, Critical);
plain_enum!(SearchSourceJson, SearchSource, Scrollback, Visible);
plain_enum!(
    RegionSourceJson,
    RegionSource,
    Scrollback,
    Visible,
    CommandOutput,
    LastCommand
);

/// The version a writer answers `offered` with: the highest it shares.
#[must_use]
pub fn negotiate(offered: &[u32]) -> ServerHello {
    match offered
        .iter()
        .copied()
        .filter(|version| SUPPORTED_VERSIONS.contains(version))
        .max()
    {
        Some(version) => ServerHello::Welcome { version },
        None => ServerHello::Refused {
            versions: SUPPORTED_VERSIONS.to_vec(),
            message: format!("no offered version in {offered:?} is one this writer speaks"),
        },
    }
}

/// A daemon request a JSON request or stream opener maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonRequest {
    Ops(OpsToDaemonMsg),
    Session(SessionToDaemonMsg),
    Search(SearchToDaemonMsg),
    Region(RegionToDaemonMsg),
    Notify(NotifyToDaemonMsg),
}

fn prefix(session: SessionId) -> String {
    session_id_hex(session.0)
}

impl TryFrom<Request> for OpsToDaemonMsg {
    type Error = DtoError;

    fn try_from(request: Request) -> Result<Self, DtoError> {
        Ok(match request {
            Request::List => Self::List,
            Request::Info { session } => Self::Info {
                id_prefix: prefix(session),
            },
            Request::Create { args } => Self::Spawn { args: args.into() },
            Request::Kill { session } => Self::Destroy {
                id_prefix: prefix(session),
            },
            Request::Tag {
                session,
                add,
                remove,
            } => Self::Tag {
                id_prefix: prefix(session),
                add,
                remove,
            },
            Request::Send { session, steps } => Self::Send {
                id_prefix: prefix(session),
                steps: steps
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
            },
        })
    }
}

impl TryFrom<Open> for DaemonRequest {
    type Error = DtoError;

    fn try_from(open: Open) -> Result<Self, DtoError> {
        Ok(match open {
            Open::Attach {
                target: AttachTarget::Session(session),
            } => Self::Session(SessionToDaemonMsg::Attach {
                target: DomainAttachTarget::Id(session.0),
                live_only: false,
            }),
            Open::Attach {
                target: AttachTarget::Create(args),
            } => Self::Session(SessionToDaemonMsg::Create { args: args.into() }),
            Open::Wait { session, steps } => Self::Ops(OpsToDaemonMsg::SendAndWait {
                id_prefix: prefix(session),
                steps: steps
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
            }),
            Open::Search {
                session,
                query,
                regex,
                case_insensitive,
                source,
            } => Self::Search(SearchToDaemonMsg::Query {
                query,
                options: SearchOptions {
                    regex,
                    case_insensitive,
                },
                source: source.into(),
                session: Some(session.0),
            }),
            Open::Capture {
                session,
                source,
                ansi,
                max_rows,
            } => Self::Region(RegionToDaemonMsg::Rows {
                source: source.into(),
                ansi,
                max_rows,
                session: Some(session.0),
            }),
            Open::Notifications { session } => Self::Notify(NotifyToDaemonMsg::Subscribe {
                session_prefix: session.map(prefix),
            }),
        })
    }
}

impl From<SendStep> for SendStepJson {
    fn from(step: SendStep) -> Self {
        match step {
            SendStep::Paste(bytes) => Self::Paste {
                bytes: bytes.into(),
            },
            SendStep::Raw(bytes) => Self::Raw {
                bytes: bytes.into(),
            },
            SendStep::Key(event) => Self::Key {
                event: event.into(),
            },
            SendStep::Mouse(event) => Self::Mouse {
                event: event.into(),
            },
        }
    }
}

impl TryFrom<SendStepJson> for SendStep {
    type Error = DtoError;

    fn try_from(step: SendStepJson) -> Result<Self, DtoError> {
        Ok(match step {
            SendStepJson::Paste { bytes } => Self::Paste(bytes.0),
            SendStepJson::Raw { bytes } => Self::Raw(bytes.0),
            SendStepJson::Key { event } => Self::Key(event.try_into()?),
            SendStepJson::Mouse { event } => Self::Mouse(event.try_into()?),
        })
    }
}

impl From<SearchToClientMsg> for SearchMatchJson {
    fn from(msg: SearchToClientMsg) -> Self {
        let SearchToClientMsg::Match {
            line_index,
            text,
            byte_spans,
            col_spans,
            anchor,
        } = msg;
        Self {
            line_index,
            text,
            byte_spans: byte_spans
                .into_iter()
                .map(|span| ByteSpanJson {
                    start: span.start,
                    end: span.end,
                })
                .collect(),
            col_spans: col_spans
                .into_iter()
                .map(|span| ColSpanJson {
                    line_index: span.line_index,
                    col_start: span.col_start,
                    col_end: span.col_end,
                })
                .collect(),
            anchor: anchor.map(Into::into),
        }
    }
}

impl TryFrom<RegionToClientMsg> for CaptureRowJson {
    type Error = DtoError;

    fn try_from(msg: RegionToClientMsg) -> Result<Self, DtoError> {
        match msg {
            RegionToClientMsg::Row {
                row,
                text,
                ansi,
                soft_wrap_continued,
            } => Ok(Self {
                row,
                text,
                ansi,
                soft_wrap_continued,
            }),
            // The row stream's closing code rides the `end` terminal,
            // and the one-shot reply is no stream item at all.
            RegionToClientMsg::RowsDone { .. } => Err(DtoError::Outside("RowsDone")),
            RegionToClientMsg::Reply { .. } => Err(DtoError::Outside("Reply")),
        }
    }
}

impl TryFrom<NotifyToClientMsg> for NotificationJson {
    type Error = DtoError;

    fn try_from(msg: NotifyToClientMsg) -> Result<Self, DtoError> {
        match msg {
            NotifyToClientMsg::Event {
                session_id,
                notification,
                notify_id,
                session_title,
                cwd,
                attached,
            } => Ok(Self {
                session_id: SessionId(session_id),
                title: notification.title,
                body: notification.body,
                urgency: notification.urgency.into(),
                notification_id: notify_id,
                session_title,
                cwd,
                attached,
            }),
            // The acknowledgement opens the stream and the daemon's lag
            // report is a `lag` message, so neither is an item.
            NotifyToClientMsg::Subscribed { .. } => Err(DtoError::Outside("Subscribed")),
            NotifyToClientMsg::Lagged { .. } => Err(DtoError::Outside("Lagged")),
        }
    }
}

impl From<AttachRefusal> for ErrorKind {
    fn from(refusal: AttachRefusal) -> Self {
        match refusal {
            AttachRefusal::Attach(AttachFailure::UnknownSession | AttachFailure::NoMatch) => {
                Self::NoMatch
            }
            AttachRefusal::Attach(AttachFailure::Ambiguous) => Self::Ambiguous,
            AttachRefusal::Attach(AttachFailure::SessionEnding | AttachFailure::SessionExited) => {
                Self::SessionEnded
            }
            AttachRefusal::Create(failure) => failure.into(),
        }
    }
}

impl From<CreateFailure> for ErrorKind {
    fn from(failure: CreateFailure) -> Self {
        match failure {
            CreateFailure::SessionLimitReached => Self::AtCapacity,
            CreateFailure::SpawnFailed
            | CreateFailure::GeometryOutOfRange
            | CreateFailure::DaemonDraining => Self::Refused,
        }
    }
}

impl From<SendRefusal> for ErrorKind {
    fn from(refusal: SendRefusal) -> Self {
        match refusal {
            SendRefusal::SessionEnded => Self::SessionEnded,
            SendRefusal::InvalidStep | SendRefusal::Unreported | SendRefusal::TooLarge => {
                Self::InvalidRequest
            }
        }
    }
}

/// For a session-addressed stream, where the daemon's `Unavailable`
/// means the session is gone.
impl From<StreamErrorReason> for ErrorKind {
    fn from(reason: StreamErrorReason) -> Self {
        match reason {
            StreamErrorReason::InvalidRequest => Self::InvalidRequest,
            StreamErrorReason::TooManyStreams => Self::AtCapacity,
            StreamErrorReason::Unavailable => Self::SessionEnded,
            StreamErrorReason::Internal => Self::Internal,
        }
    }
}

/// Which opener a stream came from, which fixes the items it may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Attach,
    Wait,
    Search,
    Capture,
    Notifications,
}

impl Open {
    #[must_use]
    pub const fn kind(&self) -> StreamKind {
        match self {
            Self::Attach { .. } => StreamKind::Attach,
            Self::Wait { .. } => StreamKind::Wait,
            Self::Search { .. } => StreamKind::Search,
            Self::Capture { .. } => StreamKind::Capture,
            Self::Notifications { .. } => StreamKind::Notifications,
        }
    }
}

impl Request {
    /// The `op` token, which the result answering it repeats.
    #[must_use]
    pub const fn op(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Info { .. } => "info",
            Self::Create { .. } => "create",
            Self::Kill { .. } => "kill",
            Self::Tag { .. } => "tag",
            Self::Send { .. } => "send",
        }
    }
}

impl OpResult {
    /// The `op` token of the request this answers.
    #[must_use]
    pub const fn op(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Info { .. } => "info",
            Self::Create { .. } => "create",
            Self::Kill(_) => "kill",
            Self::Tag(_) => "tag",
            Self::Send(_) => "send",
        }
    }
}

impl Item {
    /// The one stream kind this item belongs to.
    #[must_use]
    pub const fn stream_kind(&self) -> StreamKind {
        match self {
            Self::Attached { .. } | Self::Grid { .. } | Self::Image { .. } => StreamKind::Attach,
            Self::Match(_) => StreamKind::Search,
            Self::Row(_) => StreamKind::Capture,
            Self::Notification(_) => StreamKind::Notifications,
            Self::Sent { .. } | Self::Mark { .. } => StreamKind::Wait,
        }
    }
}

/// A message that breaks the correlation rules.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FramingError {
    #[error("id {0} is already open")]
    InUse(CorrelationId),
    #[error("id {0} names nothing open")]
    Unknown(CorrelationId),
    #[error("id {0} is a request, not a stream")]
    NotAStream(CorrelationId),
    #[error("id {0} is a stream, not a request")]
    NotARequest(CorrelationId),
    #[error("id {0} is answered with another operation's result")]
    WrongResult(CorrelationId),
    #[error("id {0} is not a stream this message belongs to")]
    WrongStream(CorrelationId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// The operation the reply must answer.
    Request(&'static str),
    Stream(StreamKind),
}

/// The open requests and streams of one connection. Either side runs
/// every message through it: anything it refuses breaks the rule that a
/// request or a stream ends exactly once and nothing names it after.
#[derive(Debug, Default)]
pub struct Ledger {
    open: BTreeMap<CorrelationId, Pending>,
}

impl Ledger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `id` names an open request or stream.
    #[must_use]
    pub fn is_open(&self, id: CorrelationId) -> bool {
        self.open.contains_key(&id)
    }

    /// Admits a client message; input needs an open attach stream.
    ///
    /// # Errors
    ///
    /// When the message breaks the correlation rules; nothing changes.
    pub fn client(&mut self, message: &ClientMessage) -> Result<(), FramingError> {
        match message {
            ClientMessage::Request { id, request } => {
                self.take(*id, Pending::Request(request.op()))
            }
            ClientMessage::Open { id, stream } => self.take(*id, Pending::Stream(stream.kind())),
            ClientMessage::Input { id, .. } => self.stream(*id, Some(StreamKind::Attach)),
            ClientMessage::Cancel { id } => self.stream(*id, None),
        }
    }

    /// Admits a writer message; a terminal closes what it ends.
    ///
    /// # Errors
    ///
    /// When the message breaks the correlation rules; nothing changes.
    pub fn server(&mut self, message: &ServerMessage) -> Result<(), FramingError> {
        match message {
            ServerMessage::Item { id, item } => self.stream(*id, Some(item.stream_kind())),
            ServerMessage::Lag { id, .. } => self.stream(*id, None),
            ServerMessage::Reply { id, result } => match self.open.get(id) {
                Some(Pending::Request(op)) if *op != result.op() => {
                    Err(FramingError::WrongResult(*id))
                }
                Some(Pending::Request(_)) => {
                    self.open.remove(id);
                    Ok(())
                }
                Some(Pending::Stream(_)) => Err(FramingError::NotARequest(*id)),
                None => Err(FramingError::Unknown(*id)),
            },
            ServerMessage::End { id, .. } | ServerMessage::Lagged { id } => {
                self.stream(*id, None)?;
                self.open.remove(id);
                Ok(())
            }
            ServerMessage::Error { id, .. } => self
                .open
                .remove(id)
                .map(|_| ())
                .ok_or(FramingError::Unknown(*id)),
        }
    }

    fn take(&mut self, id: CorrelationId, pending: Pending) -> Result<(), FramingError> {
        if self.open.contains_key(&id) {
            return Err(FramingError::InUse(id));
        }
        self.open.insert(id, pending);
        Ok(())
    }

    fn stream(&self, id: CorrelationId, kind: Option<StreamKind>) -> Result<(), FramingError> {
        match self.open.get(&id) {
            Some(Pending::Stream(open)) if kind.is_none_or(|kind| kind == *open) => Ok(()),
            Some(Pending::Stream(_)) => Err(FramingError::WrongStream(id)),
            Some(Pending::Request(_)) => Err(FramingError::NotAStream(id)),
            None => Err(FramingError::Unknown(id)),
        }
    }
}

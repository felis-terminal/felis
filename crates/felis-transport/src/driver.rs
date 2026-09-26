//! Typed connection driver: state machine for phase, direction, and correlation
//! (`docs/explanation/architecture/ipc.md`). Connection failures end only this
//! connection with a typed error; sessions and other connections survive.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::marker::PhantomData;

use bytes::Bytes;
use felis_protocol::{
    ConnectionMode, MessageKind,
    codec::{self, Correlated, WireCodec},
    messages::{
        ArmMeta, ConnToClientMsg, ConnToDaemonMsg, Correlation, CorrelationClass, Directed,
        Direction, RequestId, StreamErrorReason, StreamId, Subject,
    },
};

use crate::framing::{OwnedFrame, TransportError};

/// Defined in `felis-protocol` beside the arm table, so the phase
/// column and its enforcement here cannot drift.
pub use felis_protocol::messages::Phase;

/// Per connection rather than global, so a misbehaving peer starves only
/// itself; exceeding it refuses the new stream, not the connection
/// (`docs/reference/ipc.md`).
pub const MAX_OUTSTANDING_STREAMS: usize = 32;

/// Which half of every request/reply pair this process writes; not the
/// peer's stated mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Client,
    Daemon,
}

impl Side {
    #[must_use]
    pub const fn inbound(self) -> Direction {
        match self {
            Self::Client => Direction::ToClient,
            Self::Daemon => Direction::ToDaemon,
        }
    }

    const fn outbound(self) -> Direction {
        match self {
            Self::Client => Direction::ToDaemon,
            Self::Daemon => Direction::ToClient,
        }
    }
}

mod sealed {
    use felis_protocol::messages::{
        ConnToClientMsg, ConnToDaemonMsg, RequestId, StreamId, Subject,
    };

    pub trait Sealed {}

    /// What an admitted control arm does to the stream table.
    pub enum Effect {
        None,
        Cancel(StreamId),
        Terminal(StreamId),
        /// A typed refusal answers its request as finally as a reply.
        Answer(RequestId),
    }

    pub trait ControlArm {
        fn effect(&self) -> Effect;
    }

    impl ControlArm for ConnToDaemonMsg {
        fn effect(&self) -> Effect {
            match self {
                Self::Hello { .. } => Effect::None,
                Self::Cancel { stream_id } => Effect::Cancel(*stream_id),
            }
        }
    }

    impl ControlArm for ConnToClientMsg {
        fn effect(&self) -> Effect {
            match self {
                Self::Welcome { .. } | Self::Refused { .. } => Effect::None,
                Self::End { stream_id, .. }
                | Self::Error {
                    subject: Subject::Stream(stream_id),
                    ..
                } => Effect::Terminal(*stream_id),
                Self::Error {
                    subject: Subject::Request(id),
                    ..
                } => Effect::Answer(*id),
            }
        }
    }
}

/// Which end of the connection a driver reads for, fixed in its type so
/// a handler decodes its own side's wrapper of each family: the daemon
/// reads [`ConnToDaemonMsg`], a client [`ConnToClientMsg`], and a
/// wrapper of the other direction does not type-check as input.
pub trait Role: sealed::Sealed + Debug {
    const SIDE: Side;
    /// The connection-family wrapper this side reads.
    type Control: WireCodec + Directed + Debug + sealed::ControlArm;
}

/// The client end: it writes requests and reads replies.
#[derive(Debug)]
pub struct ClientSide;

/// The daemon end: it reads requests and writes replies.
#[derive(Debug)]
pub struct DaemonSide;

impl sealed::Sealed for ClientSide {}
impl sealed::Sealed for DaemonSide {}

impl Role for ClientSide {
    const SIDE: Side = Side::Client;
    type Control = ConnToClientMsg;
}

impl Role for DaemonSide {
    const SIDE: Side = Side::Daemon;
    type Control = ConnToDaemonMsg;
}

pub type ClientDriver = ConnectionDriver<ClientSide>;
pub type DaemonDriver = ConnectionDriver<DaemonSide>;

/// Terminated ids stay countable: with only a live set, a late cancel
/// for a finished stream and a cancel for a fabricated id would be the
/// same lookup miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamClass {
    Active,
    Canceled,
    Terminated,
    NeverOpened,
}

#[derive(Debug)]
pub enum Incoming<C> {
    Control(C),
    Payload(Payload),
    /// The wire contract's idempotent no-op for a cancel naming a
    /// terminated stream, kept out of [`Self::Control`] so a caller
    /// cannot act on it by accident.
    CancelIgnored {
        stream_id: StreamId,
    },
}

#[derive(Debug, Clone)]
pub struct Payload {
    pub kind: MessageKind,
    pub body: Bytes,
    pub correlation: Option<Correlation>,
    /// Envelope peek failure reason. [`ConnectionDriver::decode`] reports this
    /// fault only after family decode succeeds, ensuring corrupt frames are not
    /// misreported as correlation faults.
    pub envelope_fault: Option<String>,
}

/// A message the driver admitted, with the identity it validated
/// against the arm's [`CorrelationClass`]. The accessors are total for
/// the class the arm declares, so a handler reads the id off the
/// delivery instead of re-deriving it from the raw envelope.
#[derive(Debug)]
pub struct Delivered<M> {
    pub msg: M,
    correlation: Option<Correlation>,
}

impl<M: Directed> Delivered<M> {
    /// The request this arm is or answers.
    ///
    /// # Panics
    /// If the arm's class names no request. Unreachable through
    /// [`ConnectionDriver::decode`], which is the only constructor.
    #[must_use]
    pub fn request(&self) -> RequestId {
        match self.correlation {
            Some(Correlation::Request(id)) => id,
            found => unreachable!(
                "{} declares {:?}, so decode admitted it only with a request id, not {found:?}",
                self.msg.variant(),
                self.msg.meta().correlation,
            ),
        }
    }

    /// The stream this arm opens or belongs to.
    ///
    /// # Panics
    /// If the arm's class names no stream. Unreachable through
    /// [`ConnectionDriver::decode`], which is the only constructor.
    #[must_use]
    pub fn stream(&self) -> StreamId {
        match self.correlation {
            Some(Correlation::Stream(id)) => id,
            found => unreachable!(
                "{} declares {:?}, so decode admitted it only with a stream id, not {found:?}",
                self.msg.variant(),
                self.msg.meta().correlation,
            ),
        }
    }

    #[must_use]
    pub const fn correlation(&self) -> Option<Correlation> {
        self.correlation
    }
}

#[derive(Debug)]
pub enum Delivery<M> {
    Deliver(Delivered<M>),
    /// An item for a stream this side already canceled: the cancel
    /// raced the items already on the wire.
    DroppedAfterCancel,
    /// Send `reply` and keep the connection.
    RefuseStream {
        /// Also the stream's one terminal, so the client's table closes
        /// the id on receipt.
        reply: ConnToClientMsg,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("unexpected frame kind: expected {expected}, found {found}")]
    UnexpectedKind {
        expected: &'static str,
        found: String,
    },
    #[error("wrong direction: expected {arm} from the {expected}, found it from the {actual}")]
    WrongDirection {
        arm: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
    #[error("undecodable body: expected a {expected} message, found {found}")]
    UndecodableBody {
        expected: MessageKind,
        found: String,
    },
    #[error("correlation violation on {kind}: expected {expected}, found {found}")]
    Correlation {
        kind: MessageKind,
        expected: &'static str,
        found: String,
    },
    /// Wrapping instead would reuse an id the peer already retired, and a
    /// stale `Cancel` for it would kill an unrelated stream; re-dialing
    /// is the recovery.
    #[error("the {which} sequence is exhausted on this connection")]
    SequenceExhausted { which: &'static str },
    /// The kind-level gate, before the body is decoded. The daemon
    /// answers `Conn::Refused` and the connection ends.
    #[error("mode denied: expected a mode admitting {kind}, found {mode:?}")]
    ModeDenied {
        mode: ConnectionMode,
        kind: MessageKind,
    },
    /// The arm-level gate, after the body is decoded. Whether the
    /// caller may still answer through the envelope depends on where it
    /// sits (pre-attach there is a writer for a `Conn::Refused`,
    /// attached there is not), so the driver reports rather than picks
    /// (`docs/explanation/architecture/ipc.md` "Kind or arm?").
    #[error("arm denied: a {mode:?} connection may not carry {arm} in the {phase:?} phase")]
    ArmDenied {
        mode: ConnectionMode,
        arm: &'static str,
        phase: Phase,
    },
    /// The phase column, at either level: the kind fold before the
    /// decode, the arm's own row after it.
    #[error("out of phase: {arm} is legal in {legal}, but this connection is in {phase:?}")]
    OutOfPhase {
        arm: &'static str,
        phase: Phase,
        legal: String,
    },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
    Open,
    Canceled,
}

/// Both ends keep `next_stream` in step: the client advances it by
/// allocating, the daemon by observing an open, and a disagreement is
/// corruption.
#[derive(Debug)]
struct StreamTable {
    next_stream: u64,
    next_request: u64,
    active: BTreeMap<u64, StreamState>,
    outstanding_requests: BTreeSet<u64>,
}

impl StreamTable {
    const fn new() -> Self {
        Self {
            // 0 is how the wire spells "unset".
            next_stream: 1,
            next_request: 1,
            active: BTreeMap::new(),
            outstanding_requests: BTreeSet::new(),
        }
    }

    fn classify(&self, id: StreamId) -> StreamClass {
        match self.active.get(&id.get()) {
            Some(StreamState::Open) => StreamClass::Active,
            Some(StreamState::Canceled) => StreamClass::Canceled,
            None if id.get() < self.next_stream => StreamClass::Terminated,
            None => StreamClass::NeverOpened,
        }
    }
}

/// Sans-IO: the daemon's two pump loops, the GUI's reader task and the
/// CLI's deadline-wrapped reads could not share a driver that owned the
/// socket.
#[derive(Debug)]
pub struct ConnectionDriver<R: Role> {
    phase: Phase,
    /// `None` on the daemon until the `Hello` lands.
    mode: Option<ConnectionMode>,
    table: StreamTable,
    stream_bound: usize,
    role: PhantomData<R>,
}

impl ConnectionDriver<ClientSide> {
    #[must_use]
    pub const fn client(mode: ConnectionMode) -> Self {
        Self::fresh(Some(mode))
    }
}

impl ConnectionDriver<DaemonSide> {
    #[must_use]
    pub const fn daemon() -> Self {
        Self::fresh(None)
    }
}

impl<R: Role> ConnectionDriver<R> {
    const fn fresh(mode: Option<ConnectionMode>) -> Self {
        Self {
            phase: Phase::Preface,
            mode,
            table: StreamTable::new(),
            stream_bound: MAX_OUTSTANDING_STREAMS,
            role: PhantomData,
        }
    }

    #[must_use]
    pub const fn with_stream_bound(mut self, bound: usize) -> Self {
        self.stream_bound = bound;
        self
    }

    pub const fn preface_done(&mut self) {
        self.phase = Phase::Handshake;
    }

    pub const fn handshake_done(&mut self, mode: ConnectionMode) {
        self.mode = Some(mode);
        self.phase = Phase::Setup;
    }

    /// The daemon calls this once the session task accepted the
    /// subscriber and before it writes the ack; a client calls it on
    /// decoding that ack, so the rehydrate burst that follows on the
    /// same reader is judged in the phase it belongs to.
    pub const fn attached(&mut self) {
        debug_assert!(
            matches!(self.phase, Phase::Setup),
            "a connection attaches from Setup"
        );
        self.phase = Phase::Attached;
    }

    /// The session surfaces stop being admitted from here on, so an
    /// observer cannot be fed grid traffic.
    pub const fn observing(&mut self) {
        debug_assert!(
            matches!(self.phase, Phase::Setup),
            "a connection starts observing from Setup"
        );
        self.phase = Phase::Observing;
    }

    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    #[must_use]
    pub const fn mode(&self) -> Option<ConnectionMode> {
        self.mode
    }

    #[must_use]
    pub fn classify_stream(&self, id: StreamId) -> StreamClass {
        self.table.classify(id)
    }

    #[must_use]
    pub fn outstanding_streams(&self) -> usize {
        self.table.active.len()
    }

    /// Ids issued and not yet answered: a caller that allocates outside
    /// the lock ordering its writes can read here that it has opened a
    /// hole in the request sequence.
    #[must_use]
    pub fn outstanding_requests(&self) -> usize {
        self.table.outstanding_requests.len()
    }

    pub fn issue_request(&mut self) -> Result<RequestId, DriverError> {
        let id = self.table.next_request;
        self.table.next_request = self
            .table
            .next_request
            .checked_add(1)
            .ok_or(DriverError::SequenceExhausted { which: "request" })?;
        let _fresh = self.table.outstanding_requests.insert(id);
        Ok(RequestId::new(id).unwrap_or_else(|| unreachable!("the request sequence starts at 1")))
    }

    /// Retire an outstanding request. A reply naming an id this side
    /// already retired or never issued is a correlation violation, not
    /// a frame to drop: taken as an answer it would hand the caller
    /// another request's result.
    pub fn match_reply(&mut self, kind: MessageKind, id: RequestId) -> Result<(), DriverError> {
        if self.table.outstanding_requests.remove(&id.get()) {
            Ok(())
        } else {
            Err(DriverError::Correlation {
                kind,
                expected: "a reply echoing an outstanding request_id",
                found: format!("request {id}, which is not outstanding"),
            })
        }
    }

    /// Client-allocated rather than daemon-assigned ids: the client can
    /// cancel a stream that stalled before its first item without a
    /// round trip.
    pub fn open_stream(&mut self) -> Result<StreamId, DriverError> {
        let id = self.table.next_stream;
        self.table.next_stream = self
            .table
            .next_stream
            .checked_add(1)
            .ok_or(DriverError::SequenceExhausted { which: "stream" })?;
        let _fresh = self.table.active.insert(id, StreamState::Open);
        Ok(StreamId::new(id).unwrap_or_else(|| unreachable!("the stream sequence starts at 1")))
    }

    pub fn cancel_stream(&mut self, id: StreamId) {
        if let Some(state) = self.table.active.get_mut(&id.get()) {
            *state = StreamState::Canceled;
        }
    }

    #[must_use]
    pub fn unexpected(expected: &'static str, found: impl Into<String>) -> DriverError {
        DriverError::UnexpectedKind {
            expected,
            found: found.into(),
        }
    }

    /// The driver validates the envelope's shape; which ids a given arm
    /// requires is the family's own rule, reported through here.
    #[must_use]
    pub fn correlation_violation(
        kind: MessageKind,
        expected: &'static str,
        found: impl Into<String>,
    ) -> DriverError {
        DriverError::Correlation {
            kind,
            expected,
            found: found.into(),
        }
    }

    #[must_use]
    pub fn refuse_request(
        id: RequestId,
        reason: StreamErrorReason,
        detail: impl Into<String>,
    ) -> ConnToClientMsg {
        ConnToClientMsg::Error {
            subject: Subject::Request(id),
            reason,
            detail: detail.into(),
        }
    }

    /// `false` when the stream was not live. Whoever writes a stream's
    /// terminal must report it here, or the slot stays taken for the
    /// connection's life.
    pub fn retire_stream(&mut self, id: StreamId) -> bool {
        self.table.active.remove(&id.get()).is_some()
    }

    /// `None` when the stream is not live: a producer that raced an
    /// eviction must not write a second terminal.
    pub fn finish_stream(&mut self, id: StreamId, count: u32) -> Option<ConnToClientMsg> {
        self.retire_stream(id).then_some(ConnToClientMsg::End {
            stream_id: id,
            count,
        })
    }

    pub fn fail_stream(
        &mut self,
        id: StreamId,
        reason: StreamErrorReason,
        detail: impl Into<String>,
    ) -> Option<ConnToClientMsg> {
        self.retire_stream(id).then(|| ConnToClientMsg::Error {
            subject: Subject::Stream(id),
            reason,
            detail: detail.into(),
        })
    }

    pub fn classify(&mut self, frame: &OwnedFrame) -> Result<Incoming<R::Control>, DriverError> {
        let kind = self.admit_kind(frame.kind)?;
        let (correlation, envelope_fault) = Self::peek_envelope(kind, &frame.body)?;
        if kind == MessageKind::Conn {
            return self.classify_control(&frame.body, correlation, envelope_fault.as_deref());
        }
        Ok(Incoming::Payload(Payload {
            kind,
            body: frame.body.clone(),
            correlation,
            envelope_fault,
        }))
    }

    /// Peeked on every family, not only the ones whose wrapper declares
    /// the slot: a hand-built body can carry field 100 whatever the
    /// schema reserves it for, and an envelope no arm of the family
    /// claims is unattributable correlation, which ends the connection
    /// (REQ-114) rather than being skipped.
    fn peek_envelope(
        kind: MessageKind,
        body: &Bytes,
    ) -> Result<(Option<Correlation>, Option<String>), DriverError> {
        match codec::peek_correlation(body) {
            Ok(correlation) => Ok((correlation, None)),
            Err(err @ codec::CodecError::Prost(_)) => Ok((None, Some(err.to_string()))),
            Err(err) => Err(DriverError::Correlation {
                kind,
                expected: "a well-formed correlation envelope",
                found: err.to_string(),
            }),
        }
    }

    /// Validate arm-table rules for parked frames held back for correlation.
    ///
    /// Prevents routing violations on parked frames (REQ-114). Stateless here
    /// because [`Self::classify`] re-examines the frame on final read.
    pub fn admit_parked(&self, frame: &OwnedFrame) -> Result<(), DriverError> {
        let kind = self.admit_kind(frame.kind)?;
        let (correlation, envelope_fault) = Self::peek_envelope(kind, &frame.body)?;
        let payload = Payload {
            kind,
            body: frame.body.clone(),
            correlation,
            envelope_fault,
        };
        let meta = if kind == MessageKind::Conn {
            let msg: R::Control = codec::decode(&payload.body)
                .map_err(|err| Self::undecodable(kind, &payload.body, &err))?;
            Self::refuse_foreign_arm(kind, &payload.body)?;
            msg.meta()
        } else {
            Self::inbound_arm(&payload)?
        };
        Self::admit_envelope(payload.kind, payload.envelope_fault.as_deref())?;
        self.admit_arm(&meta)?;
        Self::admit_class(payload.kind, &meta, payload.correlation).map(|_paired| ())
    }

    /// The peek's failure, raised once a family decode has succeeded
    /// ([`Payload::envelope_fault`]).
    fn admit_envelope(kind: MessageKind, fault: Option<&str>) -> Result<(), DriverError> {
        match fault {
            None => Ok(()),
            Some(found) => Err(DriverError::Correlation {
                kind,
                expected: "a well-formed correlation envelope",
                found: found.to_owned(),
            }),
        }
    }

    pub fn decode<M: WireCodec + Directed>(
        &mut self,
        payload: &Payload,
    ) -> Result<Delivery<M>, DriverError> {
        const {
            assert!(
                M::DIRECTION as u8 == R::SIDE.inbound() as u8,
                "a side decodes only the wrapper that travels toward it"
            );
        }
        debug_assert_eq!(
            payload.kind,
            M::KIND,
            "decoded a payload as the wrong family"
        );
        let msg: M = codec::decode(&payload.body)
            .map_err(|err| Self::undecodable(payload.kind, &payload.body, &err))?;
        Self::refuse_foreign_arm(payload.kind, &payload.body)?;
        match self.admit_meta(payload, &msg.meta())? {
            Accounting::Deliver => Ok(Delivery::Deliver(Delivered {
                msg,
                correlation: payload.correlation,
            })),
            Accounting::Drop => Ok(Delivery::DroppedAfterCancel),
            Accounting::Refuse(reply) => Ok(Delivery::RefuseStream { reply }),
        }
    }

    /// The arm-table checks and the accounting a decoded arm passes
    /// before it is delivered, shared by [`Self::decode`] and the drain.
    fn admit_meta(&mut self, payload: &Payload, meta: &ArmMeta) -> Result<Accounting, DriverError> {
        Self::admit_envelope(payload.kind, payload.envelope_fault.as_deref())?;
        // Before the mode and phase columns, not after: a mode-denied
        // arm is refused through its own envelope and the connection
        // lives on, so the id has to be accounted for or the next
        // request would look like a skip.
        let accounting = self.account_for(payload.kind, meta, payload.correlation)?;
        self.admit_arm(meta)?;
        Ok(accounting)
    }

    /// Run arm-table checks over an unread payload before dropping it.
    ///
    /// Draining exempts a frame from nothing: body validation, direction,
    /// phase, mode, and correlation all hold (REQ-113a, REQ-114). Drops
    /// deliveries, so a caller that owes a reply uses [`Self::decode`].
    pub fn admit_drained(&mut self, payload: &Payload) -> Result<(), DriverError> {
        // `classify` answers the control family as `Incoming::Control`,
        // having already accounted for it; no payload of it exists to
        // admit twice.
        if payload.kind == MessageKind::Conn {
            return Ok(());
        }
        if payload.kind.arms_are_uniform() && !payload.kind.is_correlated() {
            return self.admit_drained_uniform(payload);
        }
        let meta = Self::inbound_arm(payload)?;
        self.admit_meta(payload, &meta).map(drop)
    }

    /// The drain path for a uniform uncorrelated family (`Input`,
    /// `Grid`, `Image`): the kind columns are exact where every arm
    /// declares the same row, so the arm-table walk folds into them and
    /// the body is validated rather than skipped. Wire tier only
    /// (`docs/reference/ipc.md` "Corruption").
    fn admit_drained_uniform(&self, payload: &Payload) -> Result<(), DriverError> {
        self.admit_kind_columns(payload.kind)?;
        // Before the envelope, as in `decode`: a body that is not
        // protobuf at all would otherwise be reported as the
        // correlation fault the peek inferred from it.
        Self::inbound_arm(payload)?;
        Self::admit_envelope(payload.kind, payload.envelope_fault.as_deref())?;
        match payload.correlation {
            None => Ok(()),
            found => Err(DriverError::Correlation {
                kind: payload.kind,
                expected: CorrelationClass::Uncorrelated.expects(),
                found: format!("{} on {}", describe(found), payload.kind),
            }),
        }
    }

    /// The envelope has to be exactly what the arm's class declares
    /// (`docs/reference/ipc.md` "The arm table"), so a handler receives
    /// a validated identity instead of recovering one. Only the client
    /// allocates ids, which is why an opener is legal only inbound at
    /// the daemon and a reply or item only inbound at a client.
    fn account_for(
        &mut self,
        kind: MessageKind,
        meta: &ArmMeta,
        correlation: Option<Correlation>,
    ) -> Result<Accounting, DriverError> {
        use CorrelationClass as Class;
        match (
            meta.correlation,
            Self::admit_class(kind, meta, correlation)?,
        ) {
            (Class::Uncorrelated, _) => Ok(Accounting::Deliver),
            (Class::RequestOpener, Some(Correlation::Request(id))) => self.accept_request(kind, id),
            (Class::RequestReply, Some(Correlation::Request(id))) => {
                self.match_reply(kind, id)?;
                Ok(Accounting::Deliver)
            }
            (Class::StreamOpener, Some(Correlation::Stream(id))) => self.accept_open(kind, id),
            (Class::StreamItem, Some(Correlation::Stream(id))) => self.accept_item(kind, id),
            pairing => unreachable!("admit_class passed an unpaired class: {pairing:?}"),
        }
    }

    /// The pairing alone, with no table moved: which class may carry
    /// which id, and inbound at which side. The parked path runs only
    /// this, because the frame it holds will be read again.
    fn admit_class(
        kind: MessageKind,
        meta: &ArmMeta,
        correlation: Option<Correlation>,
    ) -> Result<Option<Correlation>, DriverError> {
        use CorrelationClass as Class;
        let paired = matches!(
            (meta.correlation, correlation, R::SIDE),
            (Class::Uncorrelated, None, _)
                | (
                    Class::RequestOpener,
                    Some(Correlation::Request(_)),
                    Side::Daemon
                )
                | (
                    Class::RequestReply,
                    Some(Correlation::Request(_)),
                    Side::Client
                )
                | (
                    Class::StreamOpener,
                    Some(Correlation::Stream(_)),
                    Side::Daemon
                )
                | (
                    Class::StreamItem,
                    Some(Correlation::Stream(_)),
                    Side::Client
                )
        );
        if paired {
            Ok(correlation)
        } else {
            Err(DriverError::Correlation {
                kind,
                expected: meta.correlation.expects(),
                found: format!("{} on {}", describe(correlation), meta.name),
            })
        }
    }

    /// Check strictly sequential request ids allocated by the client.
    ///
    /// The receiver tracks only `next_request`: skipped or reused ids fail
    /// immediately, without maintaining an unbounded outstanding-request set.
    fn accept_request(
        &mut self,
        kind: MessageKind,
        id: RequestId,
    ) -> Result<Accounting, DriverError> {
        if id.get() != self.table.next_request {
            return Err(DriverError::Correlation {
                kind,
                expected: "a request_id equal to the next unissued id",
                found: format!("request {id}, while {} was next", self.table.next_request),
            });
        }
        self.table.next_request = self
            .table
            .next_request
            .checked_add(1)
            .ok_or(DriverError::SequenceExhausted { which: "request" })?;
        Ok(Accounting::Deliver)
    }

    /// The counter advances whether or not the stream is served, so a
    /// refusal cannot desync the two ends.
    fn accept_open(
        &mut self,
        kind: MessageKind,
        stream_id: StreamId,
    ) -> Result<Accounting, DriverError> {
        if stream_id.get() != self.table.next_stream {
            return Err(DriverError::Correlation {
                kind,
                expected: "a stream_id equal to the next unopened id",
                found: format!(
                    "stream {stream_id}, while {} was next ({:?})",
                    self.table.next_stream,
                    self.table.classify(stream_id)
                ),
            });
        }
        self.table.next_stream = self
            .table
            .next_stream
            .checked_add(1)
            .ok_or(DriverError::SequenceExhausted { which: "stream" })?;
        if self.table.active.len() >= self.stream_bound {
            return Ok(Accounting::Refuse(ConnToClientMsg::Error {
                subject: Subject::Stream(stream_id),
                reason: StreamErrorReason::TooManyStreams,
                detail: format!(
                    "this connection already holds {} open streams",
                    self.stream_bound
                ),
            }));
        }
        let _fresh = self.table.active.insert(stream_id.get(), StreamState::Open);
        Ok(Accounting::Deliver)
    }

    fn accept_item(
        &self,
        kind: MessageKind,
        stream_id: StreamId,
    ) -> Result<Accounting, DriverError> {
        match self.table.classify(stream_id) {
            StreamClass::Active => Ok(Accounting::Deliver),
            StreamClass::Canceled => Ok(Accounting::Drop),
            StreamClass::Terminated | StreamClass::NeverOpened => Err(DriverError::Correlation {
                kind,
                expected: "an item on a live stream",
                found: format!(
                    "stream {stream_id}, which is {:?}",
                    self.table.classify(stream_id)
                ),
            }),
        }
    }

    fn classify_control(
        &mut self,
        body: &Bytes,
        correlation: Option<Correlation>,
        envelope_fault: Option<&str>,
    ) -> Result<Incoming<R::Control>, DriverError> {
        use sealed::{ControlArm as _, Effect};

        let msg: R::Control =
            codec::decode(body).map_err(|err| Self::undecodable(MessageKind::Conn, body, &err))?;
        Self::refuse_foreign_arm(MessageKind::Conn, body)?;
        Self::admit_envelope(MessageKind::Conn, envelope_fault)?;
        let meta = msg.meta();
        match self.account_for(MessageKind::Conn, &meta, correlation)? {
            Accounting::Deliver => {}
            // Every control arm is `Uncorrelated`, so the class check
            // can only pass or refuse; a stream accounting here would
            // mean the table and this branch disagree.
            Accounting::Drop | Accounting::Refuse(_) => {
                return Err(Self::unexpected("an uncorrelated control arm", meta.name));
            }
        }
        self.admit_arm(&meta)?;
        match msg.effect() {
            Effect::None => Ok(Incoming::Control(msg)),
            Effect::Cancel(stream_id) => self.apply_cancel(stream_id, msg),
            Effect::Terminal(stream_id) => {
                self.apply_terminal(stream_id)?;
                Ok(Incoming::Control(msg))
            }
            Effect::Answer(id) => {
                self.match_reply(MessageKind::Conn, id)?;
                Ok(Incoming::Control(msg))
            }
        }
    }

    fn apply_cancel(
        &mut self,
        stream_id: StreamId,
        cancel: R::Control,
    ) -> Result<Incoming<R::Control>, DriverError> {
        match self.table.classify(stream_id) {
            StreamClass::Active | StreamClass::Canceled => {
                self.cancel_stream(stream_id);
                Ok(Incoming::Control(cancel))
            }
            StreamClass::Terminated => Ok(Incoming::CancelIgnored { stream_id }),
            StreamClass::NeverOpened => Err(DriverError::Correlation {
                kind: MessageKind::Conn,
                expected: "a cancel for a stream that was opened",
                found: format!("stream {stream_id}, which was never opened"),
            }),
        }
    }

    /// Exactly one terminal per stream; a second is corruption.
    fn apply_terminal(&mut self, stream_id: StreamId) -> Result<(), DriverError> {
        match self.table.classify(stream_id) {
            StreamClass::Active | StreamClass::Canceled => {
                let _closed = self.table.active.remove(&stream_id.get());
                Ok(())
            }
            class => Err(DriverError::Correlation {
                kind: MessageKind::Conn,
                expected: "the one terminal of a live stream",
                found: format!("stream {stream_id}, which is {class:?}"),
            }),
        }
    }

    fn admit_kind(&self, raw: u16) -> Result<MessageKind, DriverError> {
        let Some(kind) = MessageKind::from_u16(raw) else {
            return Err(DriverError::UnexpectedKind {
                expected: "a known frame kind",
                found: format!("kind {raw}"),
            });
        };
        if self.phase == Phase::Preface {
            return Err(DriverError::UnexpectedKind {
                expected: "no frame before the preface completes",
                found: kind.to_string(),
            });
        }
        self.admit_kind_columns(kind)?;
        Ok(kind)
    }

    /// The kind-level folds of the arm table. Coarser than the arm's
    /// own row for a family whose arms disagree, exact for one whose
    /// [`MessageKind::arms_are_uniform`].
    fn admit_kind_columns(&self, kind: MessageKind) -> Result<(), DriverError> {
        // Mode is refused first and separately: folded into the phase
        // column it would report a phase violation as a mode denial,
        // and reached after it a peer asking for a surface its mode
        // never carries would be told to wait for a state it can never
        // reach.
        if let Some(mode) = self.mode
            && !kind.modes().contains(mode)
        {
            return Err(DriverError::ModeDenied { mode, kind });
        }
        // Unconditionally enforce kind phases folded from the arm table.
        // Filters unexpected kinds early, including during handshake before
        // the connection mode is established.
        if !kind.phases().contains(self.phase) {
            return Err(DriverError::OutOfPhase {
                arm: kind.as_str(),
                phase: self.phase,
                legal: kind.phases().tokens().join("/"),
            });
        }
        if let Some(one_way) = kind.sole_direction()
            && one_way != R::SIDE.inbound()
        {
            return Err(DriverError::WrongDirection {
                arm: kind.as_str(),
                expected: one_way.as_str(),
                actual: R::SIDE.inbound().as_str(),
            });
        }
        Ok(())
    }

    /// The row of the arm a payload carries, read as this side's
    /// wrapper of its family, for the paths that judge a frame without
    /// delivering it.
    fn inbound_arm(payload: &Payload) -> Result<ArmMeta, DriverError> {
        match codec::arm_of(payload.kind, R::SIDE.inbound(), &payload.body) {
            Some(Ok(meta)) => {
                Self::refuse_foreign_arm(payload.kind, &payload.body)?;
                Ok(meta)
            }
            Some(Err(err)) => Err(Self::undecodable(payload.kind, &payload.body, &err)),
            // The kind columns refused a one-way family arriving
            // backwards before any caller reaches here.
            None => Err(Self::unexpected(
                "a family that travels toward this side",
                payload.kind.as_str(),
            )),
        }
    }

    /// A body this side's wrapper reads may still carry an arm of the
    /// other wrapper, which prost skips as unknown; in either order it
    /// is a wrong-direction arm, not a field to ignore.
    fn refuse_foreign_arm(kind: MessageKind, body: &[u8]) -> Result<(), DriverError> {
        match codec::arm_in(kind, R::SIDE.outbound(), body) {
            Ok(None) => Ok(()),
            Ok(Some(meta)) => Err(Self::wrong_direction(&meta)),
            Err(err) => Err(DriverError::UndecodableBody {
                expected: kind,
                found: err.to_string(),
            }),
        }
    }

    const fn wrong_direction(meta: &ArmMeta) -> DriverError {
        DriverError::WrongDirection {
            arm: meta.name,
            expected: meta.direction.as_str(),
            actual: R::SIDE.inbound().as_str(),
        }
    }

    /// A body this side's wrapper cannot read. An arm of the other
    /// direction is a direction fault before any accounting: its ids
    /// come from the wrong side's sequences.
    fn undecodable(kind: MessageKind, body: &Bytes, err: &codec::CodecError) -> DriverError {
        match codec::arm_in(kind, R::SIDE.outbound(), body) {
            Ok(Some(meta)) => Self::wrong_direction(&meta),
            _ => DriverError::UndecodableBody {
                expected: kind,
                found: err.to_string(),
            },
        }
    }

    /// The arm-level half of the routing table: the mode and phase
    /// columns the kind fold could only answer coarsely
    /// (`docs/reference/ipc.md` "The arm table").
    fn admit_arm(&self, meta: &ArmMeta) -> Result<(), DriverError> {
        if !meta.phases.contains(self.phase) {
            return Err(DriverError::OutOfPhase {
                arm: meta.name,
                phase: self.phase,
                legal: meta.phases.tokens().join("/"),
            });
        }
        if let Some(mode) = self.mode
            && !meta.modes.contains(mode)
        {
            return Err(DriverError::ArmDenied {
                mode,
                arm: meta.name,
                phase: self.phase,
            });
        }
        Ok(())
    }
}

enum Accounting {
    Deliver,
    Drop,
    Refuse(ConnToClientMsg),
}

fn describe(correlation: Option<Correlation>) -> String {
    correlation.map_or_else(
        || "no correlation envelope".to_owned(),
        |correlation| correlation.to_string(),
    )
}

#[must_use]
pub fn frame_correlated<M: Correlated>(msg: &M, correlation: Correlation) -> OwnedFrame {
    OwnedFrame {
        kind: M::KIND.as_u16(),
        body: Bytes::from(codec::encode_correlated(msg, correlation)),
    }
}

#[cfg(test)]
mod tests;

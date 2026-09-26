//! IPC client connector: connect to a daemon socket, exchange the
//! frozen version preface, then run the `Hello` / `Welcome` handshake.

mod carrier;
mod requests;
#[cfg(test)]
mod tests;

pub(crate) use self::carrier::connect_carrier_with_retry;
pub use self::carrier::{
    BoundedDialError, Carrier, CarrierConnection, CarrierReader, CarrierWriter, Offer, RemoteSpawn,
    connect, connect_carrier, dial_bounded,
};
pub use self::requests::{AttachIntent, DaemonStatus, SessionRefusalContext, SwitchReply, admits};

use felis_protocol::{
    BuildIdentity, ConnectionMode, MessageKind,
    codec::{self, CodecError, WireCodec},
    messages::{
        AttachFailure, AttachRefusal, ConnToClientMsg, Correlation, CreateFailure, Directed,
        RefusalReason, RequestId, StreamErrorReason, StreamId, Subject,
    },
    minor::MinorGated,
    preface::{self},
};
use felis_transport::{
    ClientDriver, ConnectionDriver, Delivery, DriverError, FrameReader, FrameWriter, Incoming,
    OwnedFrame, PrefaceExchangeError, TransportError,
    local::{ReadHalf, WriteHalf},
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use self::requests::{ControlRequest, CorrelatedRequest};
use crate::stream::{OpenFrom, OpenStreamError, begin_stream};

/// The daemon's own refusal behind whatever wrapper a dial helper
/// returned: [`crate::DialError`] and [`crate::SpawnConnectError`] both
/// carry the [`ConnectError`] as a `#[source]`, and only the innermost
/// one knows the daemon's answer.
#[must_use]
pub fn refusal_detail<'e>(
    err: &'e (dyn std::error::Error + 'static),
) -> Option<(RefusalReason, &'e str)> {
    std::iter::successors(Some(err), |e| e.source()).find_map(|e| {
        e.downcast_ref::<ConnectError>()
            .and_then(ConnectError::refusal)
    })
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("connect: {0}")]
    Io(#[from] std::io::Error),
    /// The raw connect to the endpoint, kept apart from every other I/O
    /// failure because only its kind can say whether nothing is
    /// listening there (`docs/reference/cli.md` "Auto-spawning"). An
    /// `ssh` child that fails to start is [`Self::Io`]: a missing `ssh`
    /// binary is `NotFound` too, and must not read as a cold socket.
    #[error("connect: {0}")]
    Connect(#[source] std::io::Error),
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    #[error("daemon closed before Welcome")]
    EofBeforeWelcome,
    #[error("daemon reply arrived on an unexpected kind (kind={kind})")]
    UnexpectedKind { kind: u16 },
    #[error("daemon's first reply was not Welcome")]
    NotWelcome,
    /// Version skew never lands here: it is settled in the preface,
    /// before a frame exists.
    #[error("daemon refused the connection ({reason:?}): {detail}")]
    Refused {
        reason: RefusalReason,
        detail: String,
    },
    #[error("preface: {0}")]
    Preface(#[from] PrefaceExchangeError),
    #[error(
        "protocol major mismatch: client speaks {client_major}, daemon serves {daemon_min}-{daemon_max} — rebuild and restart felis (both halves)"
    )]
    MajorMismatch {
        client_major: u16,
        daemon_min: u16,
        daemon_max: u16,
    },
    /// Not folded into [`Self::MajorMismatch`]: the two trailing words
    /// mean whatever a future felis decided, and reading them as a
    /// major range would invent a fact.
    #[error(
        "daemon refused in the preface with status {status} (words {}, {}), which this build (protocol major {client_major}) cannot name — rebuild and restart felis (both halves)",
        words[0],
        words[1]
    )]
    UnknownPrefaceStatus {
        status: u16,
        words: [u16; 2],
        client_major: u16,
    },
    /// Not a refusal, and not [`Self::MajorMismatch`]: that one is the
    /// daemon's honest answer, this one is an accept that names a major
    /// the client never offered, so the frames after it would be
    /// decoded under a schema the two never agreed on.
    #[error(
        "daemon accepted protocol major {accepted} but this client offered {offered}; the daemon is not speaking felis's negotiation, rebuild both halves"
    )]
    AcceptedUnofferedMajor { offered: u16, accepted: u16 },
    /// The attach half refused. Only an attach (or the attach a create
    /// runs after its spawn landed) can answer this.
    #[error("daemon refused the attach ({reason:?}): {detail}")]
    AttachFailed {
        reason: AttachFailure,
        detail: String,
    },
    /// The spawn half refused, so nothing was executed and no session
    /// exists to attach to.
    #[error("daemon refused the create ({reason:?}): {detail}")]
    CreateFailed {
        reason: CreateFailure,
        detail: String,
    },
    #[error("unexpected reply variant for the request")]
    NotSessionAttached,
    /// A refusal no daemon serving this request could have reached.
    /// Reported as a protocol failure rather than as the refusal it
    /// claims to be: a merits decision felis passes on is one the
    /// daemon actually took.
    #[error("daemon refused a {ctx:?} request with {reason:?}, which it cannot answer: {detail}")]
    InadmissibleRefusal {
        ctx: SessionRefusalContext,
        reason: AttachRefusal,
        detail: String,
    },
    /// A protocol failure already ended this connection's usefulness.
    /// Kept apart from the failure itself so the second caller is not
    /// told the first one's story.
    #[error("this connection was abandoned after a protocol failure; reconnect")]
    Abandoned,
    /// A caller-supplied prefix that is not one: empty, over 32 digits,
    /// or not hex. Refused here rather than sent, since an empty prefix
    /// encodes as an attach naming no session at all.
    #[error("`{prefix}` is not a session id prefix: {reason}")]
    InvalidSessionPrefix { prefix: String, reason: String },
    /// Refused rather than degraded: an older daemon ignores the field
    /// it does not define and falls back to its own default, which for
    /// a switch scope moves every window (`docs/reference/ipc.md`
    /// "Versioning").
    #[error(
        "this daemon's protocol minor is {effective}; {feature} needs minor {needs} — restart the daemon to pick up the newer build"
    )]
    MinorTooOld {
        needs: u16,
        effective: u16,
        feature: &'static str,
    },
    /// Unlike [`Self::Refused`], leaves the connection usable.
    #[error("daemon refused {subject} ({reason:?}): {detail}")]
    StreamRefused {
        subject: String,
        reason: StreamErrorReason,
        detail: String,
    },
    #[error("driver: {0}")]
    Driver(#[from] DriverError),
    #[error("peer closed mid-attach")]
    EofMidAttach,
}

impl ConnectError {
    /// The daemon's own refusal, with the detail it named. A caller
    /// that cannot tell a refusal from an unreachable daemon cannot
    /// decide whether waiting is worth anything.
    #[must_use]
    pub fn refusal(&self) -> Option<(RefusalReason, &str)> {
        match self {
            Self::Refused { reason, detail } => Some((*reason, detail)),
            _ => None,
        }
    }

    #[must_use]
    pub fn at_capacity(&self) -> Option<&str> {
        match self.refusal() {
            Some((RefusalReason::AtCapacity, detail)) => Some(detail),
            _ => None,
        }
    }

    /// Whether retrying the same daemon can plausibly answer differently:
    /// capacity limits or mid-handshake drops may clear, but frame/version
    /// verdicts do not (`docs/reference/ipc.md` "Handshake"). Uses exhaustive
    /// matching so new variants are explicitly classified.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        match self {
            Self::Refused { reason, .. } => match reason {
                RefusalReason::AtCapacity => true,
                // The daemon cannot grow a mode between two dials.
                RefusalReason::Role | RefusalReason::UnknownMode => false,
            },
            // A carrier that died mid-preface is a daemon restarting
            // between the accept and the reply, like an EOF before `Welcome`.
            Self::Io(_)
            | Self::Connect(_)
            | Self::Transport(_)
            | Self::EofBeforeWelcome
            | Self::EofMidAttach
            | Self::Preface(PrefaceExchangeError::Io(_)) => true,
            // A malformed relay carrier block is a verdict too: a client
            // never writes one, so the peer is not a felis relay.
            Self::Preface(PrefaceExchangeError::Preface(_) | PrefaceExchangeError::Carrier(_))
            | Self::Codec(_)
            | Self::UnexpectedKind { .. }
            | Self::NotWelcome
            | Self::MajorMismatch { .. }
            | Self::UnknownPrefaceStatus { .. }
            | Self::AcceptedUnofferedMajor { .. }
            | Self::AttachFailed { .. }
            | Self::CreateFailed { .. }
            | Self::Driver(_)
            | Self::StreamRefused { .. }
            | Self::NotSessionAttached
            | Self::InadmissibleRefusal { .. }
            | Self::Abandoned
            | Self::InvalidSessionPrefix { .. }
            | Self::MinorTooOld { .. } => false,
        }
    }

    /// Whether autospawning a daemon can answer this failure: every
    /// transient failure except a refusal. Refusals mean a daemon already
    /// holds the socket; mid-handshake EOFs remain spawnable to handle
    /// stale listener sockets after daemon exits.
    #[must_use]
    pub fn may_be_a_cold_socket(&self) -> bool {
        match self {
            // `ENOENT` and `ECONNREFUSED` are the only connect failures
            // that prove nothing is listening; spawning over any other
            // one would put a second daemon beside a live one.
            Self::Connect(err) => felis_transport::preface::connect_error_is_absent(err),
            // Never a dial of the endpoint: an `ssh` child that will
            // not start raises `NotFound` of its own, and a refusal
            // came from a daemon already holding the socket.
            Self::Io(_) | Self::Refused { .. } => false,
            other => other.is_transient(),
        }
    }
}

/// The send-side gate for the mode a connection declares: a daemon
/// below the minor that defined one decodes the `Hello` as no mode at
/// all and drops the connection, so refusing here reports the skew
/// instead of losing it in an EOF (`docs/reference/ipc.md`
/// "Connection modes").
pub const fn check_mode_minor(since_minor: u16, effective_minor: u16) -> Result<(), ConnectError> {
    if effective_minor < since_minor {
        return Err(ConnectError::MinorTooOld {
            needs: since_minor,
            effective: effective_minor,
            feature: "the connection mode this client asks for",
        });
    }
    Ok(())
}

/// The send-side gate for an arm the wire gained after the first
/// release, read off that arm's own row (`ArmMeta::since_minor`)
/// rather than restated here: a second copy of the minor is what lets
/// the table declare a gate nothing applies.
pub fn check_arm_minor(
    arm: &impl Directed,
    effective_minor: u16,
    feature: &'static str,
) -> Result<(), ConnectError> {
    let needs = arm.meta().since_minor;
    if effective_minor < needs {
        return Err(ConnectError::MinorTooOld {
            needs,
            effective: effective_minor,
            feature,
        });
    }
    Ok(())
}

pub struct Connection<R = ReadHalf, W = WriteHalf> {
    pub reader: FrameReader<R>,
    pub writer: FrameWriter<W>,
    /// One driver per connection, wherever the reading happens: the GUI
    /// runs its own reader loop over it.
    pub driver: ClientDriver,
    /// Frames read past while waiting for a correlated reply, in
    /// arrival order; [`Self::next_frame`] takes them first so a verb on
    /// a streaming connection never consumes the stream's items.
    pending: std::collections::VecDeque<OwnedFrame>,
    /// `min(ours, the daemon's)`: this client may use no addition a
    /// later minor introduced.
    pub effective_minor: u16,
    /// The major the preface accepted, and the daemon's own minor as it
    /// advertised it. Kept beside [`Self::effective_minor`], not
    /// derived from it: on a daemon newer than this client the two
    /// minors differ, and it is the daemon's that `felis daemon status`
    /// reports.
    pub accepted_major: u16,
    pub daemon_minor: u16,
    /// The daemon's build. Informational only, never a compatibility
    /// gate.
    pub daemon_identity: Option<BuildIdentity>,
    /// Set once a reply broke the protocol: every later byte comes from
    /// a peer whose framing is not trusted, so the next request is
    /// refused instead of read.
    abandoned: bool,
}

impl<R, W> Connection<R, W> {
    /// A connection whose handshake already happened elsewhere. Both
    /// peers are taken to be this build: the daemon's advertised numbers
    /// only exist in a real preface exchange, so a test that models
    /// version skew goes through `connect`.
    pub const fn from_halves(
        reader: FrameReader<R>,
        writer: FrameWriter<W>,
        mode: ConnectionMode,
    ) -> Self {
        let effective_minor = writer.effective_minor();
        let mut driver = ConnectionDriver::client(mode);
        driver.preface_done();
        driver.handshake_done(mode);
        Self {
            reader,
            writer,
            driver,
            pending: std::collections::VecDeque::new(),
            effective_minor,
            accepted_major: preface::PROTOCOL_MAJOR,
            daemon_minor: effective_minor,
            daemon_identity: None,
            abandoned: false,
        }
    }
}

enum ReplyMatch {
    Yes,
    No,
    /// A request-scoped `Conn::Error`: still an answer, so it retires
    /// the request id. Which id it names, and whether the frame is
    /// admissible at all, is left to the driver: the refusal carries no
    /// reply arm to read it off.
    Refusal,
    /// An answer to some *other* request. Parking it would queue it for
    /// the connection's life: this connector runs one request at a
    /// time, so nothing will ever await that id.
    Foreign {
        kind: MessageKind,
        named: RequestId,
    },
    /// A frame of the awaited family carrying no envelope. Every arm of
    /// a family this connector requests on declares a correlation
    /// class, so parking it would leave the verb waiting for an answer
    /// already on the wire.
    Envelopeless,
    /// A frame the peek could not read. The driver, not the peek, names
    /// the fault: a wrong-direction arm reads as one there.
    Unreadable(CodecError),
}

impl<R, W> Connection<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    async fn request<Q: ControlRequest>(&mut self, mut req: Q) -> Result<Q::Reply, ConnectError> {
        self.usable()?;
        self.send_msg(&req.take_msg()).await?;
        let reply: Q::ReplyMsg = self.recv_msg().await?;
        self.settle(req.interpret(reply))
    }

    /// A protocol failure retires the connection; a typed refusal is an
    /// answer, and leaves it as usable as the daemon's own accounting
    /// says it is.
    const fn settle<T>(&mut self, outcome: Result<T, ConnectError>) -> Result<T, ConnectError> {
        if let Err(ConnectError::NotSessionAttached | ConnectError::InadmissibleRefusal { .. }) =
            &outcome
        {
            self.abandoned = true;
        }
        outcome
    }

    const fn usable(&self) -> Result<(), ConnectError> {
        if self.abandoned {
            return Err(ConnectError::Abandoned);
        }
        Ok(())
    }

    async fn correlated_request<Q>(&mut self, req: Q) -> Result<Q::Reply, ConnectError>
    where
        Q: CorrelatedRequest,
        Q::Msg: codec::Correlated + Directed,
    {
        let outcome = self.await_correlated(req).await;
        // After a protocol fault the id and stream accounting do not
        // describe what the peer sent, so no later verb may trust it.
        if let Err(ConnectError::Driver(_) | ConnectError::Codec(_)) = &outcome {
            self.abandoned = true;
        }
        outcome
    }

    async fn await_correlated<Q>(&mut self, req: Q) -> Result<Q::Reply, ConnectError>
    where
        Q: CorrelatedRequest,
        Q::Msg: codec::Correlated + Directed,
    {
        self.usable()?;
        let mut req = req;
        let msg = req.take_msg();
        // Validated before an id is issued, not left to the send: an
        // over-limit body is refused without a byte leaving, and an id
        // issued for a frame that never went out stays outstanding for
        // the rest of the connection's life.
        WireCodec::validate(&msg).map_err(TransportError::from)?;
        let id = self.driver.issue_request()?;
        self.writer
            .send_correlated(&msg, Correlation::request(id))
            .await?;
        // Not pushed straight back onto `self.pending`: the loop would
        // spin on its own leavings instead of reading the wire.
        let mut parked = Vec::new();
        loop {
            let frame = match self.next_frame().await {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    self.requeue(parked);
                    return Err(ConnectError::EofMidAttach);
                }
                Err(err) => {
                    self.requeue(parked);
                    return Err(err);
                }
            };
            // Peek only: a frame that is not this reply must reach the
            // driver exactly once, or a stream transition settles twice.
            match Self::is_reply_to(&frame, Q::Msg::KIND, id) {
                ReplyMatch::Yes => {}
                ReplyMatch::No => {
                    // Validates routing before parking to avoid reporting
                    // success over routing violations (REQ-114). Stateless,
                    // so the frame is accounted exactly once when read.
                    if let Err(err) = self.driver.admit_parked(&frame) {
                        self.requeue(parked);
                        return Err(err.into());
                    }
                    parked.push(frame);
                    continue;
                }
                // Through the driver, not around it: the arm table
                // decides whether the refusal is admissible before it
                // counts as an answer, and its `match_reply` is what
                // retires the id exactly once.
                ReplyMatch::Refusal => {
                    self.requeue(parked);
                    let Incoming::Control(ConnToClientMsg::Error {
                        subject: Subject::Request(named),
                        reason,
                        detail,
                    }) = self.driver.classify(&frame)?
                    else {
                        unreachable!("is_reply_to matched a request-scoped refusal");
                    };
                    if named != id {
                        return Err(ConnectError::Driver(ClientDriver::correlation_violation(
                            MessageKind::Conn,
                            "a refusal naming the outstanding request_id",
                            format!("request {named}, while {id} was awaited"),
                        )));
                    }
                    return Err(ConnectError::StreamRefused {
                        subject: Subject::Request(id).to_string(),
                        reason,
                        detail,
                    });
                }
                // Request accounting decides this one, not the park
                // queue: a reply naming a retired id is a duplicate or
                // late answer, which the driver ends the connection on
                // (`docs/reference/ipc.md` "The arm table"). Parked, it
                // would be requeued past every later request.
                ReplyMatch::Foreign { kind: found, named } => {
                    self.requeue(parked);
                    self.driver.match_reply(found, named)?;
                    return Err(ConnectError::Driver(ClientDriver::correlation_violation(
                        found,
                        "a reply echoing the outstanding request_id",
                        format!("request {named}, while {id} was awaited"),
                    )));
                }
                // The driver states the verdict rather than this
                // peek: the reply arm's class is what makes the
                // envelope mandatory, and its error names the arm.
                ReplyMatch::Envelopeless => {
                    self.requeue(parked);
                    let Incoming::Payload(payload) = self.driver.classify(&frame)? else {
                        unreachable!("is_reply_to matched a payload frame");
                    };
                    let _refused = self.driver.decode::<Q::ReplyMsg>(&payload)?;
                    return Err(ConnectError::Driver(ClientDriver::correlation_violation(
                        Q::Msg::KIND,
                        "a reply carrying the outstanding request_id",
                        format!("no correlation envelope, while {id} was awaited"),
                    )));
                }
                ReplyMatch::Unreadable(peeked) => {
                    self.requeue(parked);
                    return Err(self.verdict_on_unreadable(&frame, peeked));
                }
            }
            self.requeue(parked);
            let Incoming::Payload(payload) = self.driver.classify(&frame)? else {
                unreachable!("is_reply_to matched a payload frame");
            };
            // The decode is what retires the id: the reply arm's
            // correlation class makes the match part of admitting it.
            let Delivery::Deliver(reply) = self.driver.decode::<Q::ReplyMsg>(&payload)? else {
                return Err(ConnectError::NotSessionAttached);
            };
            debug_assert_eq!(
                reply.request(),
                id,
                "the driver matched a reply to another request"
            );
            return self.settle(req.interpret(reply.msg));
        }
    }

    /// At the front, in order: out-of-order frames are what the driver's
    /// stream table reads as corruption.
    fn requeue(&mut self, parked: Vec<OwnedFrame>) {
        for frame in parked.into_iter().rev() {
            self.pending.push_front(frame);
        }
    }

    /// The driver's stateless verdict; where it finds no fault, the
    /// peek's own error stands.
    fn verdict_on_unreadable(&self, frame: &OwnedFrame, peeked: CodecError) -> ConnectError {
        self.driver
            .admit_parked(frame)
            .err()
            .map_or(ConnectError::Codec(peeked), ConnectError::Driver)
    }

    fn is_reply_to(frame: &OwnedFrame, kind: MessageKind, id: RequestId) -> ReplyMatch {
        match Self::peek_reply(frame, kind, id) {
            Ok(matched) => matched,
            Err(err) => ReplyMatch::Unreadable(err),
        }
    }

    fn peek_reply(
        frame: &OwnedFrame,
        kind: MessageKind,
        id: RequestId,
    ) -> Result<ReplyMatch, CodecError> {
        if frame.kind == MessageKind::Conn.as_u16() {
            // Which id it names is not read here: the caller re-reads
            // the frame through the driver, whose verdict on a refusal
            // naming another request is the same correlation violation
            // this peek would have to invent.
            let refusal = matches!(
                codec::decode::<ConnToClientMsg>(&frame.body)?,
                ConnToClientMsg::Error {
                    subject: Subject::Request(_),
                    ..
                }
            );
            return Ok(if refusal {
                ReplyMatch::Refusal
            } else {
                ReplyMatch::No
            });
        }
        // Request-correlated on any family, not only the awaited one: a
        // frame of another family naming a request is an answer this
        // side cannot attribute either. Stream traffic and the
        // uncorrelated push families peek as something other than a
        // request and stay parkable.
        let peeked = codec::peek_correlation(&frame.body)?;
        let Some(named) = peeked.and_then(Correlation::request_id) else {
            return Ok(if peeked.is_none() && frame.kind == kind.as_u16() {
                ReplyMatch::Envelopeless
            } else {
                ReplyMatch::No
            });
        };
        if named != id {
            let kind = MessageKind::from_u16(frame.kind).unwrap_or(kind);
            return Ok(ReplyMatch::Foreign { kind, named });
        }
        Ok(if frame.kind == kind.as_u16() {
            ReplyMatch::Yes
        } else {
            // The awaited id on the wrong family answers nothing this
            // caller can interpret, so it fails as an unattributable
            // reply rather than being parked.
            ReplyMatch::Foreign {
                kind: MessageKind::from_u16(frame.kind).unwrap_or(kind),
                named,
            }
        })
    }

    /// Every reader on this connection must come through here, not
    /// `reader` directly, or it skips frames parked while a verb was
    /// outstanding.
    pub async fn next_frame(&mut self) -> Result<Option<OwnedFrame>, ConnectError> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }
        Ok(self.reader.next_frame().await?)
    }

    /// Open a stream on an attached connection, under the next unopened
    /// stream id (`reference/ipc.md` "Correlation, requests, and streams").
    ///
    /// # Errors
    /// [`ConnectError::Driver`] with no id left, otherwise the write's.
    pub async fn open_stream<M>(&mut self, msg: &M) -> Result<StreamId, ConnectError>
    where
        M: codec::Correlated + MinorGated + Directed + Sync,
    {
        self.open_stream_from(OpenFrom::Attached, msg).await
    }

    async fn open_stream_from<M>(
        &mut self,
        from: OpenFrom,
        msg: &M,
    ) -> Result<StreamId, ConnectError>
    where
        M: codec::Correlated + MinorGated + Directed + Sync,
    {
        let Self { driver, writer, .. } = self;
        crate::stream::open_stream(writer, msg, || begin_stream(driver, from))
            .await
            .map_err(|err| match err {
                OpenStreamError::Invalid(err) | OpenStreamError::Write(err) => err.into(),
                OpenStreamError::Begin(err) => err.into(),
            })
    }

    /// Through the driver rather than straight to `codec::decode`,
    /// which would skip the direction, phase and mode columns: a reply
    /// that contradicts its arm's row must end this connection here
    /// exactly as it would on the attached client's read loop.
    async fn recv_msg<M: WireCodec + Directed + Send>(&mut self) -> Result<M, ConnectError> {
        let frame = self.next_frame().await?.ok_or(ConnectError::EofMidAttach)?;
        let payload = match self.driver.classify(&frame)? {
            Incoming::Payload(payload) => payload,
            Incoming::Control(ConnToClientMsg::Refused { reason, detail }) => {
                return Err(ConnectError::Refused { reason, detail });
            }
            Incoming::Control(_) | Incoming::CancelIgnored { .. } => {
                return Err(ConnectError::UnexpectedKind { kind: frame.kind });
            }
        };
        // `decode` debug-asserts the family, so the mismatch is caught
        // before it, as the caller's own error rather than a panic.
        if payload.kind != M::KIND {
            return Err(ConnectError::UnexpectedKind { kind: frame.kind });
        }
        match self.driver.decode::<M>(&payload)? {
            Delivery::Deliver(delivered) => Ok(delivered.msg),
            // Unreachable for an uncorrelated family; the match stays
            // total without a panic on the wire path.
            Delivery::DroppedAfterCancel | Delivery::RefuseStream { .. } => {
                Err(ConnectError::NotSessionAttached)
            }
        }
    }

    /// `M: Sync` keeps the future `Send` (`&M` crosses the write await).
    async fn send_msg<M: WireCodec + MinorGated + Directed + Clone + Sync>(
        &mut self,
        msg: &M,
    ) -> Result<(), ConnectError> {
        self.writer.send(msg).await?;
        Ok(())
    }
}

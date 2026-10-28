//! An unattached `Ops` or observer connection shared among callers
//! (`docs/reference/ipc.md` "A shared correlated connection"). The
//! reading task never awaits a caller: items go out with `try_send`.

use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use felis_protocol::{
    ConnectionMode, MessageKind,
    codec::{self, Correlated, WireCodec},
    frame::{DEFAULT_MAX_BODY, FrameError, HEADER_LEN},
    messages::{
        ConnToClientMsg, ConnToDaemonMsg, Correlation, Directed, InfoOutcome, NotifyToClientMsg,
        NotifyToDaemonMsg, OpsToClientMsg, RefusalReason, RegionSource, RegionToClientMsg,
        RegionToDaemonMsg, RequestId, ResolvedId, SearchOptions, SearchSource, SearchToClientMsg,
        SearchToDaemonMsg, SessionInfo, SpawnArgs, StreamErrorReason, StreamId, Subject,
    },
    minor::MinorGated,
};
use felis_transport::{
    CheckedFrame, ClientDriver, Delivery, DriverError, FrameReader, FrameWriter, Incoming,
    MAX_OUTSTANDING_STREAMS, OwnedFrame, Payload, Phase, TransportError,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};

use super::requests::{
    CorrelatedRequest, DestroySessionReq, InfoReq, ListReq, SpawnSessionReq, TagReq,
};
use super::{ConnectError, Connection, check_session_addressed};
use crate::stream::{OpenFrom, begin_stream};

/// A stream buffer depth for a caller with no reason to pick another:
/// the items a stream may hold unread before it is retired as lagged.
pub const DEFAULT_STREAM_BUFFER: usize = 1024;

/// Frame bytes a stream may hold unread before it is retired as
/// lagged. An item reaching an empty buffer is taken whatever its
/// size, so one large item never lags a consumer that keeps up.
pub const STREAM_BYTE_BUDGET: usize = 16 * 1024 * 1024;

/// Point calls in flight at once; a further call waits for a reply.
/// With [`MAX_OUTSTANDING_STREAMS`] for opens, this bounds the frames
/// queued for a writer the daemon has stopped reading.
pub const MAX_OUTSTANDING_REQUESTS: usize = 64;

/// Bytes of encoded calls queued for the writer at once: one frame at
/// the ceiling, so any sendable call is admitted once the queue drains.
pub const MAX_QUEUED_BYTES: usize = DEFAULT_MAX_BODY as usize + HEADER_LEN;

/// Why a correlated connection ended. Every request still waiting and
/// every stream still open receives it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Closed {
    #[error("the daemon closed the connection")]
    Eof,
    #[error("the connection failed: {0}")]
    Transport(String),
    #[error("the daemon broke the protocol: {0}")]
    Protocol(String),
    #[error("the daemon refused the connection ({reason:?}): {detail}")]
    Refused {
        reason: RefusalReason,
        detail: String,
    },
    /// Only the reading task sees this one: no handle is left to ask.
    #[error("every handle was dropped")]
    Dropped,
}

impl Closed {
    /// Whether a fresh dial may fare better, as
    /// [`ConnectError::is_transient`] reads it.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Eof
                | Self::Transport(_)
                | Self::Refused {
                    reason: RefusalReason::AtCapacity,
                    ..
                }
        )
    }
}

/// A stream's one terminal, as its receiver reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEnd {
    /// The daemon's `Conn::End`, with its own tally.
    End { count: u32 },
    /// The daemon's stream-scoped `Conn::Error`.
    Failed {
        reason: StreamErrorReason,
        detail: String,
    },
    /// The receiver fell past its item or byte bound, so the stream was
    /// canceled; what it had already queued came before this.
    Lagged,
    /// The connection ended first.
    Closed(Closed),
}

/// What [`StreamReceiver::next`] yields: items, then the terminal,
/// which repeats on every later call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent<I> {
    Item(I),
    End(StreamEnd),
}

#[doc(hidden)]
#[derive(Debug)]
pub enum Inbound {
    Ops(OpsToClientMsg),
    Region(RegionToClientMsg),
    Search(SearchToClientMsg),
    Notify(NotifyToClientMsg),
}

/// A daemon-to-client family this connection may carry.
pub trait Family: WireCodec + Directed + Send + Sized + 'static {
    #[doc(hidden)]
    fn wrap(self) -> Inbound;
    #[doc(hidden)]
    fn unwrap(inbound: Inbound) -> Option<Self>;
}

macro_rules! family {
    ($ty:ty, $arm:ident) => {
        impl Family for $ty {
            fn wrap(self) -> Inbound {
                Inbound::$arm(self)
            }
            fn unwrap(inbound: Inbound) -> Option<Self> {
                match inbound {
                    Inbound::$arm(msg) => Some(msg),
                    _ => None,
                }
            }
        }
    };
}

family!(OpsToClientMsg, Ops);
family!(RegionToClientMsg, Region);
family!(SearchToClientMsg, Search);
family!(NotifyToClientMsg, Notify);

type Reply = Result<Inbound, ConnectError>;

struct Pending {
    kind: MessageKind,
    reply: oneshot::Sender<Reply>,
    _admitted: OwnedSemaphorePermit,
}

/// An item and the frame bytes it is charged against its stream's
/// [`STREAM_BYTE_BUDGET`].
type Item = (Inbound, usize);

struct Slot {
    items: mpsc::Sender<Item>,
    /// Charged before the send, so the receiver's release never
    /// underflows it.
    held: Arc<AtomicUsize>,
    end: oneshot::Sender<StreamEnd>,
}

impl Slot {
    /// The terminal goes in before `items` closes, so a receiver that
    /// drains to the close finds it waiting.
    fn finish(self, end: StreamEnd) {
        let _unread = self.end.send(end);
    }
}

struct State {
    driver: ClientDriver,
    requests: HashMap<u64, Pending>,
    streams: HashMap<u64, Slot>,
    /// Held until the daemon's terminal, not the receiver's: a stream
    /// opened and dropped still owes a cancel and its terminal, and its
    /// in-flight items are still held to its family.
    admitted_streams: HashMap<u64, (MessageKind, OwnedSemaphorePermit)>,
    outbox: Option<mpsc::UnboundedSender<Queued>>,
    closed: Option<Closed>,
    admit_requests: Arc<Semaphore>,
    admit_streams: Arc<Semaphore>,
    admit_bytes: Arc<Semaphore>,
    stop: watch::Sender<bool>,
}

/// A frame and its share of [`MAX_QUEUED_BYTES`], returned once
/// written. A cancel has none: one per admitted stream at most.
type Queued = (CheckedFrame, Option<OwnedSemaphorePermit>);

struct Shared {
    state: Mutex<State>,
    effective_minor: u16,
    stream_buffer: usize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn close(&self, reason: Closed) {
        self.lock().close(reason);
    }

    fn stopped(&self) -> watch::Receiver<bool> {
        self.lock().stop.subscribe()
    }

    async fn admit(
        &self,
        gate: fn(&State) -> &Arc<Semaphore>,
        permits: u32,
    ) -> Result<OwnedSemaphorePermit, ConnectError> {
        let gate = Arc::clone(gate(&self.lock()));
        gate.acquire_many_owned(permits)
            .await
            .map_err(|_closed| ConnectError::ConnectionEnded(self.closed()))
    }

    fn closed(&self) -> Closed {
        self.lock().closed.clone().unwrap_or(Closed::Dropped)
    }

    /// `false` once the connection is over.
    fn route(&self, frame: &OwnedFrame) -> bool {
        let mut state = self.lock();
        if state.closed.is_some() {
            return false;
        }
        match state.route(frame) {
            Ok(()) => true,
            Err(reason) => {
                state.close(reason);
                false
            }
        }
    }
}

fn protocol(what: impl std::fmt::Display) -> Closed {
    Closed::Protocol(what.to_string())
}

impl State {
    fn usable(&self) -> Result<(), ConnectError> {
        match &self.closed {
            Some(reason) => Err(ConnectError::ConnectionEnded(reason.clone())),
            None => Ok(()),
        }
    }

    fn close(&mut self, reason: Closed) {
        if self.closed.is_some() {
            return;
        }
        self.requests.clear();
        for (_, slot) in self.streams.drain() {
            slot.finish(StreamEnd::Closed(reason.clone()));
        }
        self.admitted_streams.clear();
        self.admit_requests.close();
        self.admit_streams.close();
        self.admit_bytes.close();
        self.outbox = None;
        self.closed = Some(reason);
        self.stop.send_replace(true);
    }

    fn enqueue(&self, frame: CheckedFrame, bytes: Option<OwnedSemaphorePermit>) {
        if let Some(outbox) = &self.outbox {
            let _closing = outbox.send((frame, bytes));
        }
    }

    /// Take `id` off this side's books and tell the daemon to stop it;
    /// `end` is what its receiver reads, if it still has one.
    fn retire(&mut self, id: StreamId, end: Option<StreamEnd>) {
        let Some(slot) = self.streams.remove(&id.get()) else {
            return;
        };
        if let Some(end) = end {
            slot.finish(end);
        }
        self.driver.cancel_stream(id);
        match CheckedFrame::encode(&ConnToDaemonMsg::Cancel { stream_id: id }) {
            Ok(frame) => self.enqueue(frame, None),
            Err(err) => self.close(protocol(format_args!("encode a cancel: {err}"))),
        }
    }

    fn terminate(&mut self, id: StreamId, end: StreamEnd) {
        self.admitted_streams.remove(&id.get());
        if let Some(slot) = self.streams.remove(&id.get()) {
            slot.finish(end);
        }
    }

    fn route(&mut self, frame: &OwnedFrame) -> Result<(), Closed> {
        match self.driver.classify(frame).map_err(protocol)? {
            Incoming::Payload(payload) => self.payload(&payload),
            Incoming::Control(msg) => self.control(msg),
            Incoming::CancelIgnored { stream_id } => Err(protocol(format_args!(
                "a cancel for stream {stream_id}, which only a client sends"
            ))),
        }
    }

    fn control(&mut self, msg: ConnToClientMsg) -> Result<(), Closed> {
        match msg {
            ConnToClientMsg::End { stream_id, count } => {
                self.terminate(stream_id, StreamEnd::End { count });
                Ok(())
            }
            ConnToClientMsg::Error {
                subject: Subject::Stream(stream_id),
                reason,
                detail,
            } => {
                self.terminate(stream_id, StreamEnd::Failed { reason, detail });
                Ok(())
            }
            ConnToClientMsg::Error {
                subject: Subject::Request(id),
                reason,
                detail,
            } => {
                if let Some(pending) = self.requests.remove(&id.get()) {
                    let _abandoned = pending.reply.send(Err(ConnectError::StreamRefused {
                        subject: Subject::Request(id).to_string(),
                        reason,
                        detail,
                    }));
                }
                Ok(())
            }
            // The daemon writes it as it ends the connection.
            ConnToClientMsg::Refused { reason, detail } => Err(Closed::Refused { reason, detail }),
            ConnToClientMsg::Welcome { .. } => Err(protocol("a second Welcome")),
        }
    }

    fn payload(&mut self, payload: &Payload) -> Result<(), Closed> {
        let Some(correlation) = payload.correlation else {
            return Err(protocol(format_args!(
                "an uncorrelated {} frame on a connection that carries correlated traffic only",
                payload.kind
            )));
        };
        if let Correlation::Stream(id) = correlation
            && let Some((kind, _)) = self.admitted_streams.get(&id.get())
            && *kind != payload.kind
        {
            return Err(protocol(format_args!(
                "a {} item on a {kind} stream",
                payload.kind
            )));
        }
        let msg = match payload.kind {
            MessageKind::Ops => self.decode::<OpsToClientMsg>(payload)?,
            MessageKind::Region => self.decode::<RegionToClientMsg>(payload)?,
            MessageKind::Search => self.decode::<SearchToClientMsg>(payload)?,
            MessageKind::Notify => self.decode::<NotifyToClientMsg>(payload)?,
            kind => {
                return Err(protocol(format_args!(
                    "a {kind} frame, which this connection never carries"
                )));
            }
        };
        let Some(msg) = msg else {
            return Ok(());
        };
        match correlation {
            Correlation::Request(id) => {
                let Some(pending) = self.requests.remove(&id.get()) else {
                    return Ok(());
                };
                if pending.kind != payload.kind {
                    return Err(protocol(format_args!(
                        "a {} reply to a {} request",
                        payload.kind, pending.kind
                    )));
                }
                let _abandoned = pending.reply.send(Ok(msg));
                Ok(())
            }
            Correlation::Stream(id) => {
                let Some(slot) = self.streams.get(&id.get()) else {
                    return Ok(());
                };
                let len = payload.body.len();
                let held = slot.held.fetch_add(len, Ordering::AcqRel);
                if held > 0 && held + len > STREAM_BYTE_BUDGET {
                    self.retire(id, Some(StreamEnd::Lagged));
                    return Ok(());
                }
                match slot.items.try_send((msg, len)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        self.retire(id, Some(StreamEnd::Lagged));
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => self.retire(id, None),
                }
                Ok(())
            }
        }
    }

    /// `None` for an item the driver drops after this side's cancel.
    fn decode<M: Family>(&mut self, payload: &Payload) -> Result<Option<Inbound>, Closed> {
        match self.driver.decode::<M>(payload).map_err(protocol)? {
            Delivery::Deliver(delivered) => Ok(Some(delivered.msg.wrap())),
            Delivery::DroppedAfterCancel => Ok(None),
            Delivery::RefuseStream { .. } => Err(protocol("a stream refusal only a daemon makes")),
        }
    }

    /// The send-side checks the writer would make, before any id is
    /// issued: an id spent on a frame that never goes out is a hole the
    /// daemon ends the connection over. Sized under the widest id, an
    /// upper bound for the id the frame gets, and not encoded: a caller
    /// waiting for admission holds no frame.
    fn sendable<M: Correlated + MinorGated + Directed>(
        msg: &M,
        widest: Correlation,
        effective_minor: u16,
    ) -> Result<u32, ConnectError> {
        let requires = msg.requires();
        if requires.minor > effective_minor {
            return Err(ConnectError::MinorTooOld {
                needs: requires.minor,
                effective: effective_minor,
                feature: requires.what,
            });
        }
        WireCodec::validate(msg).map_err(TransportError::from)?;
        let body_len = codec::encoded_len_correlated(msg, widest);
        let body = u32::try_from(body_len)
            .ok()
            .filter(|len| *len <= DEFAULT_MAX_BODY)
            .ok_or(TransportError::Frame(FrameError::BodyTooLarge {
                body_len: body_len as u64,
                ceiling: DEFAULT_MAX_BODY,
            }))?;
        Ok(body + HEADER_LEN as u32)
    }

    fn require_mode(&self, mode: ConnectionMode, kind: MessageKind) -> Result<(), ConnectError> {
        match self.driver.mode() {
            Some(have) if have == mode => Ok(()),
            have => Err(ConnectError::Driver(DriverError::ModeDenied {
                mode: have.unwrap_or(mode),
                kind,
            })),
        }
    }
}

/// Ends the connection when the last handle or receiver goes.
struct Keepalive(Arc<Shared>);

impl Drop for Keepalive {
    fn drop(&mut self) {
        self.0.close(Closed::Dropped);
    }
}

/// Ends with the daemon's end, the first protocol fault, or the last
/// handle and [`StreamReceiver`]; never redialed. No `Send` method: a
/// send keeps a connection of its own (`docs/reference/ipc.md` "Ops").
#[derive(Clone)]
pub struct CorrelatedHandle {
    shared: Arc<Shared>,
    keepalive: Arc<Keepalive>,
}

impl std::fmt::Debug for CorrelatedHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CorrelatedHandle")
            .field("effective_minor", &self.shared.effective_minor)
            .field("closed", &self.shared.lock().closed)
            .finish_non_exhaustive()
    }
}

impl<R, W> Connection<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// `stream_buffer`: unread items a stream may hold before it lags.
    ///
    /// # Errors
    /// [`ConnectError::Abandoned`], or [`ConnectError::Driver`] unless an
    /// `Ops` or observer connection in `Setup`.
    pub fn into_correlated(self, stream_buffer: usize) -> Result<CorrelatedHandle, ConnectError> {
        self.usable()?;
        let mode = self.driver.mode();
        let fits = matches!(mode, Some(ConnectionMode::Ops | ConnectionMode::Observer))
            && self.driver.phase() == Phase::Setup;
        if !fits {
            return Err(ConnectError::Driver(ClientDriver::unexpected(
                "an Ops or observer connection in Setup",
                format!("{mode:?} in {:?}", self.driver.phase()),
            )));
        }
        let Self {
            reader,
            writer,
            driver,
            pending,
            effective_minor,
            ..
        } = self;
        let (outbox, out_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                driver,
                requests: HashMap::new(),
                streams: HashMap::new(),
                admitted_streams: HashMap::new(),
                outbox: Some(outbox),
                closed: None,
                admit_requests: Arc::new(Semaphore::new(MAX_OUTSTANDING_REQUESTS)),
                admit_streams: Arc::new(Semaphore::new(MAX_OUTSTANDING_STREAMS)),
                admit_bytes: Arc::new(Semaphore::new(MAX_QUEUED_BYTES)),
                stop: watch::Sender::new(false),
            }),
            effective_minor,
            stream_buffer: stream_buffer.max(1),
        });
        drop(tokio::spawn(read_loop(
            reader,
            pending,
            Arc::clone(&shared),
        )));
        drop(tokio::spawn(write_loop(
            writer,
            out_rx,
            Arc::clone(&shared),
        )));
        Ok(CorrelatedHandle {
            keepalive: Arc::new(Keepalive(Arc::clone(&shared))),
            shared,
        })
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: FrameReader<R>,
    parked: VecDeque<OwnedFrame>,
    shared: Arc<Shared>,
) {
    let mut stopped = shared.stopped();
    for frame in &parked {
        if !shared.route(frame) {
            return;
        }
    }
    loop {
        let next = tokio::select! {
            biased;
            _ = stopped.wait_for(|stopped| *stopped) => return,
            next = reader.next_frame() => next,
        };
        let frame = match next {
            Ok(Some(frame)) => frame,
            Ok(None) => return shared.close(Closed::Eof),
            Err(err) => return shared.close(Closed::Transport(err.to_string())),
        };
        if !shared.route(&frame) {
            return;
        }
    }
}

/// Dropping `writer` on close, even mid-write, is what shuts the
/// socket: a frame still queued then belongs to a caller who is gone.
async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: FrameWriter<W>,
    mut outbox: mpsc::UnboundedReceiver<Queued>,
    shared: Arc<Shared>,
) {
    let mut stopped = shared.stopped();
    let drain = async {
        while let Some((frame, bytes)) = outbox.recv().await {
            let mut written = writer.send_checked_unflushed(&frame).await;
            drop(bytes);
            while written.is_ok()
                && let Ok((frame, bytes)) = outbox.try_recv()
            {
                written = writer.send_checked_unflushed(&frame).await;
                drop(bytes);
            }
            if written.is_ok() {
                written = writer.flush().await;
            }
            if let Err(err) = written {
                shared.close(Closed::Transport(err.to_string()));
                return;
            }
        }
    };
    tokio::select! {
        biased;
        _ = stopped.wait_for(|stopped| *stopped) => {}
        () = drain => {}
    }
}

impl CorrelatedHandle {
    #[must_use]
    pub fn effective_minor(&self) -> u16 {
        self.shared.effective_minor
    }

    /// Why the connection ended, once it has.
    #[must_use]
    pub fn closed(&self) -> Option<Closed> {
        self.shared.lock().closed.clone()
    }

    #[cfg(test)]
    fn issued_requests(&self) -> usize {
        self.shared.lock().requests.len()
    }

    #[cfg(test)]
    fn liveness(&self) -> std::sync::Weak<Shared> {
        Arc::downgrade(&self.shared)
    }

    /// Streams whose terminal has not reached their receiver's buffer
    /// yet.
    #[must_use]
    pub fn open_streams(&self) -> usize {
        self.shared.lock().streams.len()
    }

    async fn request<Q>(&self, mut req: Q) -> Result<Q::Reply, ConnectError>
    where
        Q: CorrelatedRequest,
        Q::Msg: Correlated,
        Q::ReplyMsg: Family,
    {
        let msg = req.take_msg();
        {
            let state = self.shared.lock();
            state.usable()?;
            state.require_mode(ConnectionMode::Ops, <Q::Msg as WireCodec>::KIND)?;
        }
        let widest =
            Correlation::request(RequestId::new(u64::MAX).unwrap_or_else(|| unreachable!()));
        let size = State::sendable(&msg, widest, self.shared.effective_minor)?;
        let admitted = self.shared.admit(|s| &s.admit_requests, 1).await?;
        let bytes = self.shared.admit(|s| &s.admit_bytes, size).await?;
        let reply = {
            let mut state = self.shared.lock();
            state.usable()?;
            let id = state.driver.issue_request()?;
            let frame = match CheckedFrame::encode_correlated(&msg, Correlation::request(id)) {
                Ok(frame) => {
                    debug_assert!(frame.body().len() + HEADER_LEN <= size as usize);
                    frame
                }
                Err(err) => {
                    state.close(protocol(format_args!("encode request {id}: {err}")));
                    return Err(err.into());
                }
            };
            let (reply, answer) = oneshot::channel();
            let pending = Pending {
                kind: <Q::ReplyMsg as WireCodec>::KIND,
                reply,
                _admitted: admitted,
            };
            let _fresh = state.requests.insert(id.get(), pending);
            state.enqueue(frame, Some(bytes));
            answer
        };
        let inbound = match reply.await {
            Ok(answer) => answer?,
            Err(_closed) => return Err(ConnectError::ConnectionEnded(self.shared.closed())),
        };
        let Some(reply) = Q::ReplyMsg::unwrap(inbound) else {
            unreachable!("the reading task checks a reply's family against its request")
        };
        let outcome = req.interpret(reply);
        if let Err(
            err @ (ConnectError::NotSessionAttached | ConnectError::InadmissibleRefusal { .. }),
        ) = &outcome
        {
            self.shared.close(protocol(err));
        }
        outcome
    }

    async fn open<M, I>(
        &self,
        mode: ConnectionMode,
        from: OpenFrom,
        msg: &M,
    ) -> Result<StreamReceiver<I>, ConnectError>
    where
        M: Correlated + MinorGated + Directed + Sync,
        I: Family,
    {
        let observed = || {
            Err(ConnectError::Driver(ClientDriver::unexpected(
                "one notification subscription per observer connection",
                "a second subscription",
            )))
        };
        {
            let state = self.shared.lock();
            state.usable()?;
            state.require_mode(mode, M::KIND)?;
            if from == OpenFrom::Setup && state.driver.phase() != Phase::Setup {
                return observed();
            }
        }
        let widest = Correlation::stream(StreamId::new(u64::MAX).unwrap_or_else(|| unreachable!()));
        let size = State::sendable(msg, widest, self.shared.effective_minor)?;
        let admitted = self.shared.admit(|s| &s.admit_streams, 1).await?;
        let bytes = self.shared.admit(|s| &s.admit_bytes, size).await?;
        let mut state = self.shared.lock();
        state.usable()?;
        if from == OpenFrom::Setup && state.driver.phase() != Phase::Setup {
            return observed();
        }
        let id = begin_stream(&mut state.driver, from)?;
        let frame = match CheckedFrame::encode_correlated(msg, Correlation::stream(id)) {
            Ok(frame) => {
                debug_assert!(frame.body().len() + HEADER_LEN <= size as usize);
                frame
            }
            Err(err) => {
                state.close(protocol(format_args!("encode stream {id}: {err}")));
                return Err(err.into());
            }
        };
        let (items_tx, items) = mpsc::channel(self.shared.stream_buffer);
        let (end_tx, end) = oneshot::channel();
        let held = Arc::new(AtomicUsize::new(0));
        let slot = Slot {
            items: items_tx,
            held: Arc::clone(&held),
            end: end_tx,
        };
        let _fresh = state.streams.insert(id.get(), slot);
        let _fresh = state.admitted_streams.insert(id.get(), (I::KIND, admitted));
        state.enqueue(frame, Some(bytes));
        drop(state);
        Ok(StreamReceiver {
            id,
            items,
            held,
            end,
            finished: None,
            shared: Arc::clone(&self.shared),
            _keepalive: Arc::clone(&self.keepalive),
            item: PhantomData,
        })
    }

    /// # Errors
    /// [`ConnectError::ConnectionEnded`] once the connection is over,
    /// otherwise as [`Connection::list_sessions`].
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>, ConnectError> {
        self.request(ListReq).await
    }

    /// # Errors
    /// As [`Self::list_sessions`].
    pub async fn session_info(&self, id_prefix: String) -> Result<InfoOutcome, ConnectError> {
        self.request(InfoReq { id_prefix }).await
    }

    /// Spawn a session without attaching to it.
    ///
    /// # Errors
    /// As [`Self::list_sessions`], or the daemon's refusal.
    pub async fn spawn_session(&self, args: SpawnArgs) -> Result<SessionInfo, ConnectError> {
        self.request(SpawnSessionReq { args }).await
    }

    /// # Errors
    /// As [`Self::list_sessions`].
    pub async fn destroy_session(&self, id_prefix: String) -> Result<ResolvedId, ConnectError> {
        self.request(DestroySessionReq { id_prefix }).await
    }

    /// # Errors
    /// As [`Self::list_sessions`].
    pub async fn set_tags(
        &self,
        id_prefix: String,
        add: Vec<String>,
        remove: Vec<String>,
    ) -> Result<(ResolvedId, Vec<String>, Option<String>), ConnectError> {
        self.request(TagReq {
            id_prefix,
            add,
            remove,
        })
        .await
    }

    /// Read from where a fresh attach would land. Waits while
    /// [`MAX_OUTSTANDING_STREAMS`] streams await their terminal.
    ///
    /// # Errors
    /// [`ConnectError::MinorTooOld`] or [`ConnectError::ConnectionEnded`].
    pub async fn search(
        &self,
        session: u128,
        query: String,
        options: SearchOptions,
        source: SearchSource,
    ) -> Result<StreamReceiver<SearchToClientMsg>, ConnectError> {
        self.session_stream(&SearchToDaemonMsg::Query {
            query,
            options,
            source,
            session: Some(session),
        })
        .await
    }

    /// Stream one session's rows.
    ///
    /// # Errors
    /// As [`Self::search`].
    pub async fn capture(
        &self,
        session: u128,
        source: RegionSource,
        ansi: bool,
        max_rows: Option<u32>,
    ) -> Result<StreamReceiver<RegionToClientMsg>, ConnectError> {
        self.session_stream(&RegionToDaemonMsg::Rows {
            source,
            ansi,
            max_rows,
            session: Some(session),
        })
        .await
    }

    async fn session_stream<M, I>(&self, msg: &M) -> Result<StreamReceiver<I>, ConnectError>
    where
        M: Correlated + MinorGated + Directed + Sync,
        I: Family,
    {
        check_session_addressed(self.shared.effective_minor)?;
        self.open(ConnectionMode::Ops, OpenFrom::Unattached, msg)
            .await
    }

    /// An observer connection carries this one stream and ends with it.
    ///
    /// # Errors
    /// [`ConnectError::Driver`] off an observer or for a second one.
    pub async fn subscribe_notifications(
        &self,
        session_prefix: Option<String>,
    ) -> Result<StreamReceiver<NotifyToClientMsg>, ConnectError> {
        self.open(
            ConnectionMode::Observer,
            OpenFrom::Setup,
            &NotifyToDaemonMsg::Subscribe { session_prefix },
        )
        .await
    }
}

/// One stream's items and then its terminal. Dropping it before the
/// terminal cancels the stream.
pub struct StreamReceiver<I> {
    id: StreamId,
    items: mpsc::Receiver<Item>,
    held: Arc<AtomicUsize>,
    end: oneshot::Receiver<StreamEnd>,
    finished: Option<StreamEnd>,
    shared: Arc<Shared>,
    _keepalive: Arc<Keepalive>,
    item: PhantomData<fn() -> I>,
}

impl<I> std::fmt::Debug for StreamReceiver<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamReceiver")
            .field("id", &self.id)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl<I: Family> StreamReceiver<I> {
    #[must_use]
    pub const fn id(&self) -> StreamId {
        self.id
    }

    /// The next item, or the terminal once every queued item was read.
    pub async fn next(&mut self) -> StreamEvent<I> {
        if let Some(end) = &self.finished {
            return StreamEvent::End(end.clone());
        }
        if let Some((inbound, len)) = self.items.recv().await {
            self.held.fetch_sub(len, Ordering::AcqRel);
            let Some(item) = I::unwrap(inbound) else {
                unreachable!("the reading task checks an item's family against its stream")
            };
            return StreamEvent::Item(item);
        }
        let end = match (&mut self.end).await {
            Ok(end) => end,
            Err(_dropped) => StreamEnd::Closed(self.shared.closed()),
        };
        self.finished = Some(end.clone());
        StreamEvent::End(end)
    }
}

impl<I> Drop for StreamReceiver<I> {
    fn drop(&mut self) {
        if self.finished.is_none() {
            self.shared.lock().retire(self.id, None);
        }
    }
}

#[cfg(test)]
mod tests;

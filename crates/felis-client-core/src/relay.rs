//! The JSON protocol served to one embedder against one daemon
//! (`docs/reference/json-protocol.md` "Relay"): which daemon connection
//! each request and stream rides, and how each stream's items and
//! terminal come off it. The transport to the view, its framing of
//! correlation ids and any host routing stay the embedder's.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use felis_protocol::MessageKind;
use felis_protocol::frame::FrameError;
use felis_protocol::frame::HEADER_LEN;
use felis_protocol::messages::{
    ConnToClientMsg, GridMsg, ImageMsg, InfoOutcome, InputMsg, NotifyToClientMsg, OpsToClientMsg,
    PushMsg, RefusalReason, RegionToClientMsg, SearchOptions, SearchToClientMsg, SendStep,
    SessionInfo, SessionToDaemonMsg, StreamId, Subject,
};
use felis_transport::{
    CheckedFrame, ClientDriver, Delivery, FrameReader, FrameWriter, Incoming, OwnedFrame,
    TransportError,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::connector::{
    AttachIntent, Carrier, CarrierConnection, Closed, ConnectError, Connection, CorrelatedHandle,
    DEFAULT_STREAM_BUFFER, Offer, RemoteSpawn, StreamEnd, StreamEvent, StreamReceiver,
    connect_carrier,
};
use crate::json::{
    AttachTarget, CaptureRowJson, CorrelationId, DtoError, ErrorKind, ErrorObject, GridJson,
    ImageJson, InputJson, Item, NotificationJson, OpResult, Open, Request, SendStepJson,
    ServerMessage, SessionId, SessionRef, SessionRow, TagResult, session_id_hex,
};

/// Attach items read ahead of a consumer: the daemon's subscriber
/// cap, not this, bounds what a consumer that stops reading costs.
const ATTACH_BUFFER: usize = 16;

/// How long an attach's first item waits on the shared connection for
/// the session's short id: a stalled shared connection must not hold
/// back an attach that has its own.
const ROW_LOOKUP_BOUND: Duration = Duration::from_secs(1);

/// Input frames queued for an attach connection's writer.
const INPUT_BUFFER: usize = 64;

/// Encoded input bytes queued for an attach connection's writer: what a
/// window queues toward the daemon, so a largest paste is admitted with
/// room left for keys behind it. An input larger than this is admitted
/// once the queue is empty.
const INPUT_BYTE_BUDGET: usize = felis_protocol::limits::CLIENT_OUTGOING_CAP;

const _: () = assert!(INPUT_BYTE_BUDGET <= u32::MAX as usize);

/// An encoded input and its share of [`INPUT_BYTE_BUDGET`], returned
/// once the frame is written.
type QueuedInput = (CheckedFrame, OwnedSemaphorePermit);

/// The colors an attach reports with `Session::ConfigureTheme`, so the
/// session answers `OSC 10/11/12` and `OSC 4` queries with what the view
/// paints. `None` and an absent slot keep the daemon's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Theme {
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    pub cursor: Option<(u8, u8, u8)>,
    pub palette: BTreeMap<u8, (u8, u8, u8)>,
}

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub carrier: Carrier,
    /// Whether a dial over SSH may start the remote daemon.
    pub remote_spawn: RemoteSpawn,
    pub theme: Theme,
}

struct Host {
    config: RelayConfig,
    ops: CorrelatedHandle,
}

/// One daemon's half of the protocol. Cloning shares its connections;
/// it does not redial, so it ends with the shared `Ops` connection
/// ([`Self::closed`]), while each stream ends with its own.
#[derive(Clone)]
pub struct Relay {
    host: Arc<Host>,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("carrier", &self.host.config.carrier)
            .field("ops", &self.host.ops)
            .finish()
    }
}

impl Relay {
    /// Dials the shared unattached `Ops` connection.
    ///
    /// # Errors
    /// The dial's or the handshake's.
    pub async fn connect(config: RelayConfig) -> Result<Self, ConnectError> {
        let ops = connect_carrier(config.carrier.clone(), Offer::ops(), config.remote_spawn)
            .await?
            .into_correlated(DEFAULT_STREAM_BUFFER)?;
        Ok(Self {
            host: Arc::new(Host { config, ops }),
        })
    }

    /// The daemon wire minor the shared connection speaks.
    #[must_use]
    pub fn effective_minor(&self) -> u16 {
        self.host.ops.effective_minor()
    }

    /// Why the shared connection ended, once it has. Every later
    /// request on it fails `daemon_lost`.
    #[must_use]
    pub fn closed(&self) -> Option<Closed> {
        self.host.ops.closed()
    }

    /// Answers one request. A `send` dials a connection of its own and
    /// closes it with the reply, so dropping this future cancels it.
    ///
    /// # Errors
    /// The `error` the request ends with.
    pub async fn request(&self, request: Request) -> Result<OpResult, ErrorObject> {
        let ops = &self.host.ops;
        Ok(match request {
            Request::List => {
                let roster = ops.list_sessions().await.map_err(|e| connect_error(&e))?;
                OpResult::List {
                    sessions: roster
                        .iter()
                        .map(|info| SessionRow::in_roster(info, &roster))
                        .collect(),
                }
            }
            Request::Info { session } => {
                let outcome = ops
                    .session_info(session_id_hex(session.0))
                    .await
                    .map_err(|e| connect_error(&e))?;
                OpResult::Info {
                    session: Box::new(outcome.try_into()?),
                }
            }
            Request::Create { args } => {
                let info = ops
                    .spawn_session(args.into())
                    .await
                    .map_err(|e| connect_error(&e))?;
                OpResult::Create {
                    session: Box::new(self.row(&info).await),
                }
            }
            Request::Kill { session } => {
                let resolved = ops
                    .destroy_session(session_id_hex(session.0))
                    .await
                    .map_err(|e| connect_error(&e))?;
                OpResult::Kill(SessionRef::new(SessionId::try_from(resolved)?.0))
            }
            Request::Tag {
                session,
                add,
                remove,
            } => {
                let (resolved, tags, denied) = ops
                    .set_tags(session_id_hex(session.0), add, remove)
                    .await
                    .map_err(|e| connect_error(&e))?;
                let id = SessionId::try_from(resolved)?;
                if let Some(reason) = denied {
                    return Err(ErrorObject::new(ErrorKind::InvalidRequest, reason));
                }
                OpResult::Tag(TagResult {
                    id: session_id_hex(id.0),
                    tags,
                })
            }
            Request::Send { session, steps } => {
                let steps = wire_steps(steps)?;
                let mut conn = self.dial(Offer::ops()).await?;
                let outcome = conn
                    .send_steps(session_id_hex(session.0), steps)
                    .await
                    .map_err(|e| connect_error(&e))?;
                OpResult::Send(SessionRef::new(SessionId::try_from(outcome)?.0))
            }
        })
    }

    /// Opens one stream. An `attach` returns as soon as the daemon
    /// acknowledged it, with [`RelayStream::session`] set.
    ///
    /// # Errors
    /// The `error` that ends the stream before any item.
    pub async fn open(&self, open: Open) -> Result<RelayStream, ErrorObject> {
        let ops = &self.host.ops;
        match open {
            Open::Attach { target } => self.open_attach(target).await,
            Open::Wait { session, steps } => self.open_wait(session, steps).await,
            Open::Search {
                session,
                query,
                regex,
                case_insensitive,
                source,
            } => {
                let items = ops
                    .search(
                        session.0,
                        query,
                        SearchOptions {
                            regex,
                            case_insensitive,
                        },
                        source.into(),
                    )
                    .await
                    .map_err(|e| connect_error(&e))?;
                Ok(RelayStream::new(Source::Search(items)))
            }
            Open::Capture {
                session,
                source,
                ansi,
                max_rows,
            } => {
                let items = ops
                    .capture(session.0, source.into(), ansi, max_rows)
                    .await
                    .map_err(|e| connect_error(&e))?;
                Ok(RelayStream::new(Source::Capture {
                    items,
                    exit_code: None,
                }))
            }
            Open::Notifications { session } => self.open_notifications(session).await,
        }
    }

    async fn dial(&self, offer: Offer) -> Result<CarrierConnection, ErrorObject> {
        let config = &self.host.config;
        connect_carrier(config.carrier.clone(), offer, config.remote_spawn)
            .await
            .map_err(|e| connect_error(&e))
    }

    /// `info` with the short id the daemon shortens against its pool,
    /// or the full id, which is unique in any pool, when it cannot say
    /// within [`ROW_LOOKUP_BOUND`].
    async fn row(&self, info: &SessionInfo) -> SessionRow {
        let lookup = self.host.ops.session_info(session_id_hex(info.id));
        let short_id = match tokio::time::timeout(ROW_LOOKUP_BOUND, lookup).await {
            Ok(Ok(InfoOutcome::Found { short_id, .. })) => short_id,
            _ => session_id_hex(info.id),
        };
        SessionRow::from_info(info, short_id)
    }

    async fn open_attach(&self, target: AttachTarget) -> Result<RelayStream, ErrorObject> {
        let paced = matches!(self.host.config.carrier, Carrier::Local(_));
        let mut conn = self.dial(Offer::window(paced)).await?;
        let info = match target {
            AttachTarget::Session(id) => conn.attach(id.0, AttachIntent::Deliberate).await,
            AttachTarget::Create(args) => conn.create_with(args.into()).await,
        }
        .map_err(|e| connect_error(&e))?;
        // Nothing past the ack awaits before the stream is handed out:
        // a create canceled there would leave a session whose id its
        // caller never learns, past the point the daemon rolls back.
        let theme = self.host.config.theme.clone();
        let theme = SessionToDaemonMsg::ConfigureTheme {
            fg: theme.fg,
            bg: theme.bg,
            cursor: theme.cursor,
            palette: theme.palette,
        };
        let (items_tx, items) = mpsc::channel(ATTACH_BUFFER);
        let (input_tx, input) = mpsc::channel(INPUT_BUFFER);
        let mut stream = RelayStream::new(Source::Channel(items));
        stream.session = Some(SessionId(info.id));
        stream.input = Some(AttachInput {
            tx: input_tx,
            budget: Arc::new(Semaphore::new(INPUT_BYTE_BUDGET)),
            paced,
            effective_minor: conn.effective_minor,
        });
        let relay = self.clone();
        let row = async move { relay.row(&info).await };
        stream
            .tasks
            .spawn(run_attach(conn, theme, row, input, items_tx));
        Ok(stream)
    }

    async fn open_wait(
        &self,
        session: SessionId,
        steps: Vec<SendStepJson>,
    ) -> Result<RelayStream, ErrorObject> {
        let steps = wire_steps(steps)?;
        let mut conn = self.dial(Offer::ops()).await?;
        let stream = conn
            .open_send_and_wait(session_id_hex(session.0), steps)
            .await
            .map_err(|e| connect_error(&e))?;
        let (items_tx, items) = mpsc::channel(2);
        let mut relay_stream = RelayStream::new(Source::Channel(items));
        relay_stream
            .tasks
            .spawn(forward_wait(conn, stream, items_tx));
        Ok(relay_stream)
    }

    async fn open_notifications(
        &self,
        session: Option<SessionId>,
    ) -> Result<RelayStream, ErrorObject> {
        let observer = self
            .dial(Offer::observer())
            .await?
            .into_correlated(DEFAULT_STREAM_BUFFER)
            .map_err(|e| connect_error(&e))?;
        let items = observer
            .subscribe_notifications(session.map(|id| session_id_hex(id.0)))
            .await
            .map_err(|e| connect_error(&e))?;
        let queue = Arc::new(Lossy::new(DEFAULT_STREAM_BUFFER));
        let mut stream = RelayStream::new(Source::Lossy(Arc::clone(&queue)));
        stream.tasks.spawn(forward_notifications(items, queue));
        Ok(stream)
    }
}

/// What a stream yields: items, lag reports, then its one terminal,
/// which repeats on every later call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamMessage {
    Item(Item),
    Lag { dropped: u64 },
    End { count: u64, exit_code: Option<u32> },
    Error(ErrorObject),
    Lagged,
}

impl StreamMessage {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::End { .. } | Self::Error(_) | Self::Lagged)
    }

    /// The writer message carrying this for the stream `id`.
    #[must_use]
    pub fn into_server(self, id: CorrelationId) -> ServerMessage {
        match self {
            Self::Item(item) => ServerMessage::Item { id, item },
            Self::Lag { dropped } => ServerMessage::Lag { id, dropped },
            Self::End { count, exit_code } => ServerMessage::End {
                id,
                count,
                exit_code,
            },
            Self::Error(error) => ServerMessage::Error { id, error },
            Self::Lagged => ServerMessage::Lagged { id },
        }
    }
}

/// What a stream's source yields before the count is attached.
#[derive(Debug)]
enum Out {
    Item(Item),
    Lag(u64),
    End { exit_code: Option<u32> },
    Error(ErrorObject),
    Lagged,
}

enum Source {
    /// Fed by a task that waits on the consumer.
    Channel(mpsc::Receiver<Out>),
    Lossy(Arc<Lossy>),
    Search(StreamReceiver<SearchToClientMsg>),
    Capture {
        items: StreamReceiver<RegionToClientMsg>,
        exit_code: Option<u32>,
    },
    Done,
}

/// One open stream. Dropping it, like [`Self::cancel`], releases its
/// daemon side at once: an attach or a wait closes its connection, a
/// search or a capture is canceled.
pub struct RelayStream {
    session: Option<SessionId>,
    source: Source,
    input: Option<AttachInput>,
    /// Aborted rather than signaled to stop: dropping a task's
    /// connection mid-write is what releases a daemon that stopped
    /// reading.
    tasks: JoinSet<()>,
    count: u64,
    finished: Option<StreamMessage>,
}

impl std::fmt::Debug for RelayStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayStream")
            .field("count", &self.count)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl RelayStream {
    fn new(source: Source) -> Self {
        Self {
            session: None,
            source,
            input: None,
            tasks: JoinSet::new(),
            count: 0,
            finished: None,
        }
    }

    /// The session an `attach` stream landed on, known from the moment
    /// it opens, a created one included; `None` for other streams.
    #[must_use]
    pub const fn session(&self) -> Option<SessionId> {
        self.session
    }

    /// Where an `attach` stream's input goes; `None` for other streams.
    #[must_use]
    pub fn input(&self) -> Option<AttachInput> {
        self.input.clone()
    }

    /// Ends the stream now: the next call yields `end` with the items
    /// already yielded, and what was still queued is discarded.
    pub fn cancel(&mut self) {
        if self.finished.is_none() {
            self.finish(StreamMessage::End {
                count: self.count,
                exit_code: None,
            });
        }
    }

    fn finish(&mut self, end: StreamMessage) {
        self.finished = Some(end);
        self.source = Source::Done;
        self.input = None;
        self.tasks.abort_all();
    }

    /// Cancel-safe.
    pub async fn next(&mut self) -> StreamMessage {
        if let Some(end) = &self.finished {
            return end.clone();
        }
        let out = match &mut self.source {
            Source::Channel(items) => items.recv().await.unwrap_or_else(|| {
                Out::Error(internal("the stream's task ended without a terminal"))
            }),
            Source::Lossy(queue) => queue.pop().await,
            Source::Search(items) => match items.next().await {
                StreamEvent::Item(found) => Out::Item(Item::Match(found.into())),
                StreamEvent::End(end) => stream_end(end, None),
            },
            Source::Capture { items, exit_code } => capture_next(items, exit_code).await,
            Source::Done => unreachable!("a finished stream returns its terminal above"),
        };
        let message = match out {
            Out::Item(item) => {
                self.count += 1;
                StreamMessage::Item(item)
            }
            Out::Lag(dropped) => StreamMessage::Lag { dropped },
            Out::End { exit_code } => StreamMessage::End {
                count: self.count,
                exit_code,
            },
            Out::Error(error) => StreamMessage::Error(error),
            Out::Lagged => StreamMessage::Lagged,
        };
        if message.is_terminal() {
            self.finish(message.clone());
        }
        message
    }
}

async fn capture_next(
    items: &mut StreamReceiver<RegionToClientMsg>,
    exit_code: &mut Option<u32>,
) -> Out {
    loop {
        match items.next().await {
            StreamEvent::Item(RegionToClientMsg::RowsDone { exit_code: code }) => *exit_code = code,
            StreamEvent::Item(row) => {
                return match CaptureRowJson::try_from(row) {
                    Ok(row) => Out::Item(Item::Row(row)),
                    Err(err) => Out::Error(internal(err)),
                };
            }
            StreamEvent::End(end) => return stream_end(end, *exit_code),
        }
    }
}

/// Where an attach stream's input goes. It does not wait on output, so a
/// view that stops reading output still reaches the session.
#[derive(Debug, Clone)]
pub struct AttachInput {
    tx: mpsc::Sender<QueuedInput>,
    budget: Arc<Semaphore>,
    paced: bool,
    effective_minor: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("input: {0}")]
    Invalid(#[from] DtoError),
    /// Past a wire limit, such as a paste over its cap.
    #[error("input: {0}")]
    TooLarge(#[from] felis_protocol::convert::WireError),
    /// Something this connection's daemon wire minor does not define.
    #[error("input: {feature} needs protocol minor {needs}, but the daemon speaks {effective}")]
    Unsupported {
        feature: &'static str,
        needs: u16,
        effective: u16,
    },
    #[error("the attach stream has ended")]
    Ended,
}

impl AttachInput {
    /// Waits, cancel-safe, while the queue is full by frames or bytes. A
    /// `next_grid_frame` is dropped where the daemon pushes eagerly.
    ///
    /// # Errors
    /// Each but [`InputError::Ended`] means nothing was queued.
    pub async fn send(&self, input: InputJson) -> Result<(), InputError> {
        let msg = InputMsg::try_from(input)?;
        if !self.paced && matches!(msg, InputMsg::NextGridFrame) {
            return Ok(());
        }
        let requires = felis_protocol::minor::MinorGated::requires(&msg);
        if requires.minor > self.effective_minor {
            return Err(InputError::Unsupported {
                feature: requires.what,
                needs: requires.minor,
                effective: self.effective_minor,
            });
        }
        let frame = CheckedFrame::encode(&msg).map_err(|err| match err {
            TransportError::Wire(err) => InputError::TooLarge(err),
            err => unreachable!("every input arm is uncorrelated: {err}"),
        })?;
        let size = (frame.body().len() + HEADER_LEN).min(INPUT_BYTE_BUDGET);
        let bytes = Arc::clone(&self.budget)
            .acquire_many_owned(u32::try_from(size).unwrap_or(u32::MAX))
            .await
            .map_err(|_closed| InputError::Ended)?;
        self.tx
            .send((frame, bytes))
            .await
            .map_err(|_closed| InputError::Ended)
    }
}

/// A buffer that drops what does not fit and reports it as a `lag`
/// before the next item, or once the buffer is read empty.
struct Lossy {
    state: Mutex<LossyState>,
    ready: Notify,
    capacity: usize,
}

struct LossyState {
    queue: VecDeque<Out>,
    dropped: u64,
}

impl LossyState {
    fn report_lag(&mut self) {
        if self.dropped > 0 {
            self.queue
                .push_back(Out::Lag(std::mem::take(&mut self.dropped)));
        }
    }
}

impl Lossy {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(LossyState {
                queue: VecDeque::new(),
                dropped: 0,
            }),
            ready: Notify::new(),
            capacity,
        }
    }

    fn with(&self, f: impl FnOnce(&mut LossyState, usize)) {
        f(
            &mut self.state.lock().unwrap_or_else(PoisonError::into_inner),
            self.capacity,
        );
        self.ready.notify_one();
    }

    fn push(&self, item: Item) {
        self.with(|state, capacity| {
            if state.queue.len() >= capacity {
                state.dropped = state.dropped.saturating_add(1);
            } else {
                state.report_lag();
                state.queue.push_back(Out::Item(item));
            }
        });
    }

    fn lag(&self, dropped: u64) {
        self.with(|state, _| state.dropped = state.dropped.saturating_add(dropped));
    }

    fn finish(&self, end: Out) {
        self.with(|state, _| {
            state.report_lag();
            state.queue.push_back(end);
        });
    }

    async fn pop(&self) -> Out {
        loop {
            {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(out) = state.queue.pop_front() {
                    return out;
                }
                if state.dropped > 0 {
                    return Out::Lag(std::mem::take(&mut state.dropped));
                }
            }
            self.ready.notified().await;
        }
    }
}

/// Nothing the view reads may come from before the session knew what
/// to answer color queries with, so a theme that cannot be written ends
/// the stream in place of `attached`.
async fn run_attach<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    conn: Connection<R, W>,
    theme: SessionToDaemonMsg,
    row: impl Future<Output = SessionRow>,
    input: mpsc::Receiver<QueuedInput>,
    items: mpsc::Sender<Out>,
) {
    let minor = u32::from(conn.effective_minor);
    let (reader, mut writer, driver, parked) = conn.into_parts();
    let first = match writer.send(&theme).await {
        Ok(()) => Out::Item(Item::Attached {
            minor,
            session: Box::new(row.await),
        }),
        Err(err) => Out::Error(connect_error(&err.into())),
    };
    if deliver(&items, first).await {
        tokio::join!(
            forward_attach(reader, driver, parked, &items),
            write_input(writer, input),
        );
    }
}

/// Plain backpressure: a consumer that stops reading stops these reads,
/// and the daemon's subscriber cap evicts the backlog.
async fn forward_attach<R: AsyncRead + Unpin>(
    mut reader: FrameReader<R>,
    mut driver: ClientDriver,
    parked: VecDeque<OwnedFrame>,
    items: &mpsc::Sender<Out>,
) {
    let mut parked = parked.into_iter();
    loop {
        let next = match parked.next() {
            Some(frame) => Ok(Some(frame)),
            None => reader.next_frame().await,
        };
        let out = match next {
            Ok(Some(frame)) => match attach_frame(&mut driver, &frame) {
                Some(out) => out,
                None => continue,
            },
            Ok(None) => Out::Error(lost("the daemon closed the connection")),
            Err(err) => Out::Error(lost(err)),
        };
        if !deliver(items, out).await {
            return;
        }
    }
}

/// `false` once the stream is over: a terminal went out, or nobody
/// reads any more.
async fn deliver(items: &mpsc::Sender<Out>, out: Out) -> bool {
    let terminal = !matches!(out, Out::Item(_) | Out::Lag(_));
    items.send(out).await.is_ok() && !terminal
}

fn attach_frame(driver: &mut ClientDriver, frame: &OwnedFrame) -> Option<Out> {
    let payload = match driver.classify(frame) {
        Ok(Incoming::Payload(payload)) => payload,
        Ok(Incoming::Control(ConnToClientMsg::Refused { reason, detail })) => {
            return Some(Out::Error(refused(reason, detail)));
        }
        Ok(Incoming::Control(_) | Incoming::CancelIgnored { .. }) => return None,
        Err(err) => return Some(Out::Error(internal(err))),
    };
    match payload.kind {
        MessageKind::Grid => match decode::<GridMsg>(driver, &payload)? {
            Ok(msg) => match GridJson::try_from(msg) {
                Ok(msg) => Some(Out::Item(Item::Grid { msg })),
                Err(DtoError::Outside(_)) => None,
                Err(err) => Some(Out::Error(internal(err))),
            },
            Err(out) => Some(out),
        },
        MessageKind::Image => match decode::<ImageMsg>(driver, &payload)? {
            Ok(msg) => Some(Out::Item(Item::Image {
                msg: ImageJson::from(msg),
            })),
            Err(out) => Some(out),
        },
        MessageKind::Push => match decode::<PushMsg>(driver, &payload)? {
            Ok(push) => Some(Out::Error(ErrorObject::from_push(&push))),
            Err(out) => Some(out),
        },
        _ => None,
    }
}

/// `None` for an item the driver drops after a cancel.
fn decode<M: felis_protocol::codec::WireCodec + felis_protocol::messages::Directed + Send>(
    driver: &mut ClientDriver,
    payload: &felis_transport::Payload,
) -> Option<Result<M, Out>> {
    match driver.decode::<M>(payload) {
        Ok(Delivery::Deliver(delivered)) => Some(Ok(delivered.msg)),
        Ok(Delivery::DroppedAfterCancel | Delivery::RefuseStream { .. }) => None,
        Err(err) => Some(Err(Out::Error(internal(err)))),
    }
}

/// [`AttachInput::send`] checked each frame against this connection's
/// wire limits and minor, so a write fails only with the connection,
/// whose loss the reader reports.
async fn write_input<W: AsyncWrite + Unpin>(
    mut writer: FrameWriter<W>,
    mut input: mpsc::Receiver<QueuedInput>,
) {
    while let Some((frame, bytes)) = input.recv().await {
        if writer.send_checked(&frame).await.is_err() {
            return;
        }
        drop(bytes);
    }
}

async fn forward_wait(mut conn: CarrierConnection, stream: StreamId, items: mpsc::Sender<Out>) {
    loop {
        let out = match conn.next_frame().await {
            Ok(Some(frame)) => match wait_frame(&mut conn.driver, &frame, stream) {
                Some(out) => out,
                None => continue,
            },
            Ok(None) => Out::Error(lost("the daemon closed the connection")),
            Err(err) => Out::Error(connect_error(&err)),
        };
        if !deliver(&items, out).await {
            return;
        }
    }
}

fn wait_frame(driver: &mut ClientDriver, frame: &OwnedFrame, stream: StreamId) -> Option<Out> {
    match driver.classify(frame) {
        Ok(Incoming::Control(ConnToClientMsg::End { stream_id, .. })) if stream_id == stream => {
            Some(Out::End { exit_code: None })
        }
        Ok(Incoming::Control(ConnToClientMsg::Error {
            subject: Subject::Stream(stream_id),
            reason,
            detail,
        })) if stream_id == stream => Some(Out::Error(ErrorObject::new(reason.into(), detail))),
        Ok(Incoming::Control(ConnToClientMsg::Refused { reason, detail })) => {
            Some(Out::Error(refused(reason, detail)))
        }
        Ok(Incoming::Payload(payload)) if payload.kind == MessageKind::Ops => {
            match decode::<OpsToClientMsg>(driver, &payload)? {
                Ok(OpsToClientMsg::WaitSent { outcome }) => {
                    Some(match SessionId::try_from(outcome) {
                        Ok(id) => Out::Item(Item::Sent { id }),
                        Err(error) => Out::Error(error),
                    })
                }
                Ok(OpsToClientMsg::Mark { exit_code }) => Some(Out::Item(Item::Mark { exit_code })),
                Ok(_) => None,
                Err(out) => Some(out),
            }
        }
        Ok(_) => None,
        Err(err) => Some(Out::Error(internal(err))),
    }
}

/// Drains the observer into `queue` without waiting on the consumer, so
/// a consumer that falls behind loses notifications, counted, rather
/// than the subscription. Only this task falling behind lags the
/// subscription, which ends the stream `lagged`.
async fn forward_notifications(mut items: StreamReceiver<NotifyToClientMsg>, queue: Arc<Lossy>) {
    loop {
        match items.next().await {
            StreamEvent::Item(NotifyToClientMsg::Lagged { missed }) => queue.lag(missed),
            StreamEvent::Item(NotifyToClientMsg::Subscribed { .. }) => {}
            StreamEvent::Item(event) => match NotificationJson::try_from(event) {
                Ok(notification) => queue.push(Item::Notification(notification)),
                Err(err) => return queue.finish(Out::Error(internal(err))),
            },
            StreamEvent::End(end) => return queue.finish(stream_end(end, None)),
        }
    }
}

fn stream_end(end: StreamEnd, exit_code: Option<u32>) -> Out {
    match end {
        StreamEnd::End { .. } => Out::End { exit_code },
        StreamEnd::Failed { reason, detail } => Out::Error(ErrorObject::new(reason.into(), detail)),
        StreamEnd::Lagged => Out::Lagged,
        StreamEnd::Closed(closed) => Out::Error(closed_error(&closed)),
    }
}

fn wire_steps(steps: Vec<SendStepJson>) -> Result<Vec<SendStep>, ErrorObject> {
    steps
        .into_iter()
        .map(SendStep::try_from)
        .collect::<Result<_, _>>()
        .map_err(|err| ErrorObject::new(ErrorKind::InvalidRequest, err.to_string()))
}

fn internal(err: impl std::fmt::Display) -> ErrorObject {
    ErrorObject::new(ErrorKind::Internal, err.to_string())
}

fn lost(err: impl std::fmt::Display) -> ErrorObject {
    ErrorObject::new(ErrorKind::DaemonLost, err.to_string())
}

fn refused(reason: RefusalReason, detail: String) -> ErrorObject {
    let kind = match reason {
        RefusalReason::AtCapacity => ErrorKind::AtCapacity,
        RefusalReason::Role | RefusalReason::UnknownMode => ErrorKind::Refused,
    };
    ErrorObject::new(kind, detail)
}

fn closed_error(closed: &Closed) -> ErrorObject {
    match closed {
        Closed::Refused { reason, detail } => refused(*reason, detail.clone()),
        Closed::Protocol(_) => internal(closed),
        Closed::Eof | Closed::Transport(_) | Closed::Dropped => lost(closed),
    }
}

/// Exhaustive, so a new failure does not inherit a kind by default.
fn connect_error(err: &ConnectError) -> ErrorObject {
    let kind = match err {
        ConnectError::AttachFailed { reason, .. } => {
            felis_protocol::messages::AttachRefusal::Attach(*reason).into()
        }
        ConnectError::CreateFailed { reason, .. } => (*reason).into(),
        ConnectError::StreamRefused { reason, .. } => (*reason).into(),
        ConnectError::Refused { reason, detail } => return refused(*reason, detail.clone()),
        ConnectError::ConnectionEnded(closed) => return closed_error(closed),
        ConnectError::MinorTooOld { .. }
        | ConnectError::Transport(TransportError::MinorTooOld { .. }) => ErrorKind::Unsupported,
        ConnectError::InvalidSessionPrefix { .. }
        | ConnectError::Transport(
            TransportError::Wire(_) | TransportError::Frame(FrameError::BodyTooLarge { .. }),
        ) => ErrorKind::InvalidRequest,
        ConnectError::Io(_)
        | ConnectError::Connect(_)
        | ConnectError::Transport(TransportError::Io(_) | TransportError::UnexpectedEof { .. })
        | ConnectError::Preface(felis_transport::PrefaceExchangeError::Io(_))
        | ConnectError::EofBeforeWelcome
        | ConnectError::EofMidAttach => ErrorKind::DaemonLost,
        ConnectError::Transport(TransportError::Frame(_) | TransportError::Correlation { .. })
        | ConnectError::Preface(_)
        | ConnectError::Codec(_)
        | ConnectError::UnexpectedKind { .. }
        | ConnectError::NotWelcome
        | ConnectError::MajorMismatch { .. }
        | ConnectError::UnknownPrefaceStatus { .. }
        | ConnectError::AcceptedUnofferedMajor { .. }
        | ConnectError::NotSessionAttached
        | ConnectError::InadmissibleRefusal { .. }
        | ConnectError::Abandoned
        | ConnectError::Driver(_) => ErrorKind::Internal,
    };
    ErrorObject::new(kind, err.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use felis_protocol::ConnectionMode;
    use felis_protocol::preface::PROTOCOL_MINOR;
    use tokio::io::{DuplexStream, duplex};

    use super::*;

    fn row() -> SessionRow {
        serde_json::from_value(serde_json::json!({
            "id": "0123456789abcdef0123456789abcdef",
            "short_id": "01234567",
            "rows": 24,
            "cols": 80,
            "tags": [],
            "attachments": [],
            "program_status": [],
            "sequence": 1,
        }))
        .unwrap()
    }

    /// An attach over in-memory pipes whose daemon side has queued one
    /// grid frame, with what the relay writes left on `to_daemon`; the
    /// daemon's own end comes back to keep it open.
    async fn attach_over(to_daemon: DuplexStream) -> (mpsc::Receiver<Out>, DuplexStream) {
        let (mut to_relay, from_daemon) = duplex(4096);
        FrameWriter::new(&mut to_relay, PROTOCOL_MINOR)
            .send(&GridMsg::CycleEnd)
            .await
            .unwrap();
        let mut conn = Connection::from_halves(
            FrameReader::new(from_daemon),
            FrameWriter::new(to_daemon, PROTOCOL_MINOR),
            ConnectionMode::Window,
        );
        conn.driver.attached();
        let theme = SessionToDaemonMsg::ConfigureTheme {
            fg: None,
            bg: Some((1, 2, 3)),
            cursor: None,
            palette: BTreeMap::new(),
        };
        let (_, input) = mpsc::channel(1);
        let (items_tx, items) = mpsc::channel(ATTACH_BUFFER);
        drop(tokio::spawn(run_attach(
            conn,
            theme,
            std::future::ready(row()),
            input,
            items_tx,
        )));
        (items, to_relay)
    }

    #[tokio::test(start_paused = true)]
    async fn an_attach_yields_nothing_before_its_theme_frame_is_written() {
        // One byte of room, so the theme write waits for the daemon side.
        let (to_daemon, from_relay) = duplex(1);
        let (mut items, _daemon) = attach_over(to_daemon).await;

        assert!(
            tokio::time::timeout(Duration::from_millis(200), items.recv())
                .await
                .is_err(),
            "an item went out while the theme frame was unwritten"
        );
        let theme = FrameReader::new(from_relay)
            .next_frame()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(theme.kind, MessageKind::Session.as_u16());
        assert!(matches!(
            items.recv().await,
            Some(Out::Item(Item::Attached { .. }))
        ));
        assert!(matches!(
            items.recv().await,
            Some(Out::Item(Item::Grid {
                msg: GridJson::CycleEnd
            }))
        ));
    }

    #[tokio::test]
    async fn a_theme_write_that_fails_ends_the_attach_daemon_lost_before_attached() {
        let (to_daemon, from_relay) = duplex(1);
        drop(from_relay);
        let (mut items, _daemon) = attach_over(to_daemon).await;

        let first = items.recv().await;
        let Some(Out::Error(err)) = first else {
            panic!("the first item was {first:?}, not an error terminal");
        };
        assert_eq!(err.kind, ErrorKind::DaemonLost);
        assert!(items.recv().await.is_none(), "the stream went on");
    }

    #[tokio::test]
    async fn an_input_past_a_wire_limit_is_refused_before_it_is_queued() {
        let (tx, mut queued) = mpsc::channel(4);
        let input = AttachInput {
            tx,
            budget: Arc::new(Semaphore::new(INPUT_BYTE_BUDGET)),
            paced: true,
            effective_minor: 0,
        };
        let oversized = InputJson::Paste {
            bytes: crate::json::Base64(vec![b'x'; felis_protocol::limits::MAX_PASTE_BYTES + 1]),
        };
        assert!(matches!(
            input.send(oversized).await,
            Err(InputError::TooLarge(_))
        ));
        assert!(queued.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn input_waits_once_its_queued_bytes_reach_the_budget_and_resumes_as_frames_are_written()
    {
        let (to_daemon, from_relay) = duplex(64 * 1024);
        let (tx, queued) = mpsc::channel(INPUT_BUFFER);
        let budget = Arc::new(Semaphore::new(INPUT_BYTE_BUDGET));
        let input = AttachInput {
            tx,
            budget: Arc::clone(&budget),
            paced: true,
            effective_minor: PROTOCOL_MINOR,
        };
        drop(tokio::spawn(write_input(
            FrameWriter::new(to_daemon, PROTOCOL_MINOR),
            queued,
        )));
        let paste = || InputJson::Paste {
            bytes: crate::json::Base64(vec![b'x'; felis_protocol::limits::MAX_PASTE_BYTES / 4]),
        };

        let mut admitted = 0;
        while tokio::time::timeout(Duration::from_millis(200), input.send(paste()))
            .await
            .is_ok()
        {
            admitted += 1;
            assert!(admitted < INPUT_BUFFER, "the byte budget never held a send");
        }
        let frame = HEADER_LEN + felis_protocol::limits::MAX_PASTE_BYTES / 4;
        assert!(admitted * frame <= INPUT_BYTE_BUDGET);
        assert!((admitted + 1) * frame > INPUT_BYTE_BUDGET);

        let mut daemon = FrameReader::new(from_relay);
        let blocked = input.send(paste());
        daemon.next_frame().await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .expect("a written frame did not return its bytes to the budget")
            .unwrap();
        for _ in 0..admitted {
            let written = daemon.next_frame().await.unwrap().unwrap();
            assert_eq!(written.kind, MessageKind::Input.as_u16());
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while budget.available_permits() < INPUT_BYTE_BUDGET {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("written frames kept their share of the budget");
    }
}

//! IPC server loop: bind socket, accept connections, run handshake,
//! and drive connections as session subscribers.
//!
//! Detach leaves sessions parked, and outbox events encode onto the wire
//! on demand (`docs/explanation/architecture/session-lifecycle.md`).

use std::{
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use felis_grid::{Grid, PtyEffect};
use felis_protocol::{
    ConnectionMode, MessageKind,
    codec::CodecError,
    convert::WireError,
    frame::FrameError,
    messages::{
        AttachFailure, AttachRefusal, AttachTarget, ConnToClientMsg, ConnToDaemonMsg, Correlation,
        CreateFailure, Directed, Direction, GridDims, GridMsg, ImageMsg, InfoOutcome, InputMsg,
        Notification, NotifyToClientMsg, NotifyToDaemonMsg, OpsToClientMsg, OpsToDaemonMsg,
        PushMsg, RefusalReason, RegionToDaemonMsg, RequestId, ResolvedId, SearchToDaemonMsg,
        SessionInfo, SessionToClientMsg, SessionToDaemonMsg, SpawnOutcome, StopMode, StopOutcome,
        StreamErrorReason, StreamId, Subject, SwitchScope, SwitchTarget, ThemeChannel,
    },
    preface::{self, CarrierBlock, ClientPreface, DaemonAccept, DaemonRefuse},
};
use felis_transport::{
    ConnectionDriver, DaemonDriver, Delivered, Delivery, DriverError, Endpoint, FrameReader,
    FrameWriter, Incoming, Listener, OwnedFrame, Payload, PrefaceExchangeError, TransportError,
    local::{AcceptError, BindError, ServerStream},
    preface::{read_client_bootstrap, write_daemon_preface},
    server_split,
};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore, mpsc, oneshot, watch};
use tracing::{debug, error, info, warn};

use crate::{
    SessionError, SessionId, SessionPool, SpawnedPty,
    pool::{Listing, ReserveRefusal},
};

pub(crate) mod key_encode;
pub(crate) mod region;
pub(crate) mod registry_sync;
pub mod session_task;
pub(crate) mod streaming;
use session_task::{
    OutEvent, PushOutcome, SessionCmd, SubscribeOk, SubscribeRefused, SubscribeReq, SubscriberId,
};
use streaming::{write_paste, write_pty};

/// Sized to hold a full diff cycle so a whole cycle coalesces into one
/// `write()` before the cycle-boundary flush; 64 KiB matches the
/// typical Unix-socket send buffer.
const WRITE_BUF_CAPACITY: usize = 64 * 1024;

/// Produces the program for a `Create` command with no explicit program.
///
/// Handed the resolved environment base ([`EnvBaseSource`]) so `spawn_with_args`
/// remains the central site applying cwd, env policy, and overrides.
pub type SessionFactory =
    Arc<dyn Fn(Option<&[crate::child_env::EnvEntry]>) -> felis_pty::Command + Send + Sync>;

#[must_use]
pub fn default_session_factory() -> SessionFactory {
    Arc::new(SpawnedPty::default_shell_command)
}

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("bind: {0}")]
    Bind(#[from] BindError),
    #[error("accept: {0}")]
    Accept(#[from] AcceptError),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// The endpoint is relative, so no stable agent path can be derived.
    /// Its own variant rather than `Io`: a startup misconfiguration the
    /// operator fixes by naming an absolute path.
    #[error("endpoint: {0}")]
    Endpoint(#[source] io::Error),
}

/// Per-connection (and session-task pipeline) error surface. A wire
/// error can never orphan a session: the session lives in its owner
/// task, and a failed connection merely unsubscribes.
#[derive(Debug, Error)]
pub enum ConnError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    #[error("pty io: {0}")]
    PtyIo(io::Error),
    #[error("session: {0}")]
    Session(#[from] SessionError),
    #[error("row encode: {0}")]
    RowEncode(#[from] felis_grid::wire::RowCodecError),
    /// A first `Conn` frame that was not `Hello`; a first frame on another
    /// family is the driver's phase check ([`Self::Driver`]).
    #[error("expected Hello, decoded a different ConnToDaemonMsg variant")]
    NotHello,
    /// A `Hello` naming a `ConnectionMode` this build does not define:
    /// a newer client, not a corrupt one. Answered by name before the
    /// close ([`RefusalReason::UnknownMode`]) so the peer reports daemon
    /// skew instead of a bare EOF.
    #[error("Hello named connection mode {value}, which this daemon does not define")]
    UnknownMode { value: i32 },
    /// A wrong magic gets no reply: answering an unknown protocol in ours
    /// would be noise to whatever dialed.
    #[error("preface: {0}")]
    Preface(#[from] PrefaceExchangeError),
    /// The daemon writes the frozen refusal naming the range it serves,
    /// then closes (`docs/reference/ipc.md` "Versioning").
    #[error("protocol major mismatch: client speaks {client}, daemon serves {min}-{max}")]
    MajorUnsupported { client: u16, min: u16, max: u16 },
    #[error("peer closed before Hello")]
    EofBeforeHello,
    #[error(
        "expected SessionToDaemonMsg::Attach/Create, an OpsToDaemonMsg one-shot, or NotifyToDaemonMsg::Subscribe after Welcome"
    )]
    ExpectedAttachOrCreate,
    /// Distinct from [`Self::ExpectedAttachOrCreate`] so a log line can
    /// tell "wrong frame shape" from "not what your connection is for".
    #[error("mode denied: a {mode:?} connection may not {attempted}")]
    ModeDenied {
        mode: ConnectionMode,
        attempted: &'static str,
    },
    /// A peer that opened a connection and then said nothing. Only the
    /// pre-attach phases are timed, and the first operation only in a
    /// mode that must name its subject next: past one operation a
    /// bridge idles between verbs and an observer idles for hours.
    #[error("peer sent nothing before the {phase} deadline")]
    HandshakeTimeout { phase: HandshakePhase },
    /// An input payload past a ceiling the session's budget could never
    /// grant. Refused whole rather than split: a reservation larger than
    /// the budget would park the pump forever
    /// (`docs/reference/ipc.md` "Backpressure").
    #[error("input of {bytes} bytes is over the {limit}-byte limit")]
    InputOverLimit { bytes: usize, limit: usize },
    /// Always fatal to this connection only: the session and every other
    /// subscriber keep running (`docs/explanation/architecture/ipc.md`
    /// "A corrupt frame ends the connection").
    #[error("driver: {0}")]
    Driver(#[from] DriverError),
}

/// Which pre-attach phase a [`ConnError::HandshakeTimeout`] cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakePhase {
    Preface,
    Hello,
    FirstOperation,
}

impl std::fmt::Display for HandshakePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Preface => "preface",
            Self::Hello => "Hello",
            Self::FirstOperation => "first operation",
        })
    }
}

/// How long a connection may spend in each pre-attach phase
/// (`docs/reference/ipc.md` "Handshake").
#[derive(Debug, Clone, Copy)]
pub struct HandshakeDeadlines {
    pub preface: Duration,
    pub hello: Duration,
    /// Applies to the modes that must name a subject in the frame after
    /// `Welcome`; an `Ops` connection is exempt
    /// ([`ConnectionMode::idles_before_first_operation`]).
    pub first_op: Duration,
}

/// Default handshake deadlines.
///
/// Preface spans a fixed 8-byte write; `Hello` covers a full round trip
/// including slow SSH carriers; first-op accommodates prefix resolution.
impl Default for HandshakeDeadlines {
    fn default() -> Self {
        Self {
            preface: Duration::from_secs(2),
            hello: Duration::from_secs(5),
            first_op: Duration::from_secs(30),
        }
    }
}

/// Daemon-wide connection admission ledger.
///
/// Holds one permit per served connection, released on task exit.
/// Refusals are bounded by a separate semaphore to prevent handshake floods.
#[derive(Debug)]
pub struct ConnectionAdmission {
    limit: usize,
    served: Arc<Semaphore>,
    refusing: Arc<Semaphore>,
}

impl ConnectionAdmission {
    /// Shared by every [`DaemonCaps`] clone, so the ledger is one even
    /// though the caps travel per connection.
    #[must_use]
    pub fn new(limit: usize) -> Arc<Self> {
        Self::with_refusal_slots(limit, crate::pool::MAX_REFUSALS_IN_FLIGHT)
    }

    /// Both ceilings, for an embedder (and the admission tests, which
    /// cannot afford 1024 real sockets) that picks its own.
    #[must_use]
    pub fn with_refusal_slots(limit: usize, refusals: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            served: Arc::new(Semaphore::new(limit)),
            refusing: Arc::new(Semaphore::new(refusals)),
        })
    }

    /// `None` when the daemon is full. Taken before the spawn: a permit
    /// acquired inside the task would let the flood create the tasks it
    /// was meant to bound.
    #[must_use]
    pub fn admit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.served.clone().try_acquire_owned().ok()
    }

    #[must_use]
    pub fn refusal_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.refusing.clone().try_acquire_owned().ok()
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Admitted connections only; a refusal in flight is not one.
    #[must_use]
    pub fn served(&self) -> usize {
        self.limit.saturating_sub(self.served.available_permits())
    }
}

/// What this daemon build serves with, built once at startup. `Clone`,
/// not `Copy`: the agent link is one shared single-writer object per
/// daemon.
#[derive(Debug, Clone)]
pub struct DaemonCaps {
    pub idle: IdlePolicy,
    /// Most sessions this daemon admits. A field rather than
    /// [`crate::pool::MAX_SESSIONS`] read at the check: an embedder (and
    /// the admission tests, which cannot afford 256 real PTYs) picks its
    /// own.
    pub max_sessions: usize,
    /// The stable `SSH_AUTH_SOCK` link; `None` before
    /// [`serve_unix_with_factory`] derives it from the endpoint, and on
    /// Windows.
    pub agent: Option<Arc<crate::agent::AgentLink>>,
    /// The endpoint as a child is told to address it (`FELIS_SOCKET`).
    /// Filled by [`serve_unix_with_factory`] from the bound address; a
    /// caller driving [`handle_stream`] directly stamps nothing.
    pub endpoint: Option<OsString>,
    /// The connection ledger. Only the accept loop takes permits from
    /// it; a caller driving [`handle_stream`] over its own carrier (the
    /// relay's stdio) is one connection the accepting daemon already
    /// admitted.
    pub admission: Arc<ConnectionAdmission>,
    /// How long a peer may stay silent in each pre-attach phase.
    pub handshake: HandshakeDeadlines,
    /// Fired by `Ops::Stop` to end the accept loop.
    pub shutdown: Arc<Shutdown>,
    /// The in-place upgrade's barrier and the listening socket it carries.
    pub upgrade: Arc<crate::upgrade::UpgradeState>,
}

/// The daemon's own exit signal: one sender for the process, watched by
/// the accept loop. A `watch` rather than a `Notify` so a fire that
/// lands while the loop is inside an accept is still read afterward.
#[derive(Debug)]
pub struct Shutdown(watch::Sender<bool>);

impl Shutdown {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self(watch::Sender::new(false)))
    }

    /// Fired *after* the reply that announced it was written, so the
    /// caller of `felis daemon stop` reads the outcome of a stop that
    /// then happens.
    pub fn fire(&self) {
        self.0.send_modify(|fired| *fired = true);
    }

    #[must_use]
    pub fn fired(&self) -> bool {
        *self.0.borrow()
    }

    fn watch(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

/// Hand-written: a zero `max_sessions` would refuse every create.
impl Default for DaemonCaps {
    fn default() -> Self {
        Self {
            idle: IdlePolicy::default(),
            max_sessions: crate::pool::MAX_SESSIONS,
            agent: None,
            endpoint: None,
            admission: ConnectionAdmission::new(crate::pool::MAX_CONNECTIONS),
            handshake: HandshakeDeadlines::default(),
            shutdown: Shutdown::new(),
            upgrade: crate::upgrade::UpgradeState::new(),
        }
    }
}

/// What the two handshakes settled for one connection.
#[derive(Debug, Clone, Copy)]
struct ConnParams {
    mode: ConnectionMode,
    pull_paced: bool,
    /// min(client minor, daemon minor): this daemon may use no addition a
    /// later minor introduced.
    effective_minor: u16,
}

/// Session lifecycle policy (`architecture/session-lifecycle.md`). A
/// task never reaps while its shell runs (REQ-008), so the only
/// destructive knob is the post-exit grace.
#[derive(Debug, Clone, Copy)]
pub struct IdlePolicy {
    /// Default 5 s per `session-lifecycle.md` "Post-exit reaping".
    pub post_exit_grace: Duration,
    /// How often a parked (0-subscriber) session task polls the child for
    /// exit.
    pub drain_interval: Duration,
}

impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            post_exit_grace: Duration::from_secs(5),
            drain_interval: Duration::from_millis(100),
        }
    }
}

pub async fn serve_unix(
    endpoint: impl Into<Endpoint>,
    caps: DaemonCaps,
    pool: Arc<Mutex<SessionPool>>,
) -> Result<(), ServeError> {
    serve_unix_with_factory(endpoint, caps, pool, default_session_factory()).await
}

/// The endpoint as `FELIS_SOCKET` spells it: the same string `--socket`
/// takes, so a child can hand it straight back to a front-door verb.
fn endpoint_env(endpoint: &Endpoint) -> OsString {
    #[cfg(unix)]
    {
        endpoint.path().as_os_str().to_owned()
    }
    #[cfg(windows)]
    {
        OsString::from(endpoint.pipe_name())
    }
}

/// How long the accept loop waits after an accept error before asking
/// the listener again (`docs/reference/spec.md` REQ-916).
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

pub async fn serve_unix_with_factory(
    endpoint: impl Into<Endpoint>,
    caps: DaemonCaps,
    pool: Arc<Mutex<SessionPool>>,
    factory: SessionFactory,
) -> Result<(), ServeError> {
    let endpoint = endpoint.into();
    let caps = with_endpoint(&endpoint, caps)?;
    let server = Listener::bind(&endpoint)?;
    // After the bind, which refuses an endpoint a live daemon still
    // serves: only then is a link at the derived path certainly a dead
    // daemon's.
    if let Some(link) = caps.agent.as_deref() {
        link.clear_stale();
    }
    info!(endpoint = %endpoint, "felis-daemon listening");
    // A manager that never hears READY tears this daemon down at its
    // start timeout, so a failed send is a startup failure rather than
    // a daemon serving a socket on borrowed time.
    #[cfg(target_os = "linux")]
    crate::notify::notify_ready(std::env::var_os(crate::NOTIFY_SOCKET_ENV).as_deref())
        .map_err(ServeError::Io)?;
    accept_loop(server, endpoint, caps, pool, factory).await
}

/// Serves the listening socket a predecessor carried across an in-place
/// upgrade. The predecessor already cleared stale links and told the
/// service manager it was ready, and the socket path is still its.
#[cfg(unix)]
pub async fn serve_resumed(
    server: Listener,
    endpoint: Endpoint,
    caps: DaemonCaps,
    pool: Arc<Mutex<SessionPool>>,
    factory: SessionFactory,
) -> Result<(), ServeError> {
    let caps = with_endpoint(&endpoint, caps)?;
    info!(endpoint = %endpoint, "felis-daemon resumed after an upgrade");
    accept_loop(server, endpoint, caps, pool, factory).await
}

fn with_endpoint(endpoint: &Endpoint, caps: DaemonCaps) -> Result<DaemonCaps, ServeError> {
    // A relative endpoint cannot derive a usable agent path; startup is
    // the only moment an operator can act on it.
    Ok(DaemonCaps {
        agent: crate::agent::AgentLink::for_endpoint(endpoint)
            .map_err(ServeError::Endpoint)?
            .map(Arc::new),
        endpoint: Some(endpoint_env(endpoint)),
        ..caps
    })
}

async fn accept_loop(
    server: Listener,
    endpoint: Endpoint,
    caps: DaemonCaps,
    pool: Arc<Mutex<SessionPool>>,
    factory: SessionFactory,
) -> Result<(), ServeError> {
    let mut exit = caps.shutdown.watch();
    #[cfg(unix)]
    if let Err(err) = caps
        .upgrade
        .set_listener(server.as_fd(), endpoint.path().to_path_buf())
    {
        warn!(
            ?err,
            "the listening socket cannot be kept for an in-place upgrade"
        );
    }
    loop {
        if caps.upgrade.gate.is_closed() {
            tokio::select! {
                biased;
                _ = exit.changed() => break,
                () = caps.upgrade.gate.wait_open() => {}
            }
        }
        // `biased` so a fired stop wins a ready accept: the daemon that
        // answered the stop must not admit one more connection first.
        let accepted = tokio::select! {
            biased;
            _ = exit.changed() => break,
            () = caps.upgrade.gate.wait_closed() => continue,
            accepted = server.accept() => accepted,
        };
        match accepted {
            Ok(stream) => {
                let Some(permit) = caps.admission.admit() else {
                    refuse_over_capacity(stream, &caps);
                    continue;
                };
                let pool = pool.clone();
                let factory = factory.clone();
                let caps = caps.clone();
                tokio::spawn(async move {
                    // Moved in, never read: the permit is released by the
                    // drop that ends this task, however it ends.
                    let _admitted = permit;
                    if let Err(err) = handle_connection(stream, caps, pool, factory).await {
                        warn!(?err, "connection ended with error");
                    }
                });
            }
            Err(AcceptError::Peer(err)) => {
                // Not backed off like the I/O arm below: this accept
                // consumed its backlog entry, so the loop is already
                // past the offending peer, and a delay here would let
                // a foreign-UID flood meter how fast legitimate dials
                // are admitted.
                warn!(?err, "rejected connection");
            }
            Err(AcceptError::Io(err)) => {
                // Descriptor exhaustion (`EMFILE`/`ENFILE`) leaves pending dials
                // in the backlog, spinning accept at 100% CPU if not backed off.
                // Backing off prevents spinning when fd limits sit below `MAX_CONNECTIONS`.
                warn!(?err, "accept io error; backing off");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
            }
        }
    }
    #[cfg(unix)]
    caps.upgrade.clear_listener();
    info!(endpoint = %endpoint, "felis-daemon stopping");
    // Nothing unlinks the path, here or in `Drop` (REQ-009d): an
    // unlink would race a replacement daemon that rebound the same
    // name and delete its live socket.
    drop(server);
    Ok(())
}

/// Per-connection driver shared by every carrier: a `UnixStream`'s
/// halves or the relay's stdio pipes alike.
pub async fn handle_stream<R, W>(
    read_half: R,
    write_half: W,
    caps: DaemonCaps,
    pool: Arc<Mutex<SessionPool>>,
    factory: SessionFactory,
) -> Result<(), ConnError>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    // The preface runs on the raw halves: a `FrameReader` reads ahead and
    // would pull the preface into a buffer nothing else can reach.
    let (mut read_half, mut write_half) = (read_half, write_half);
    // Built before the preface so a frame arriving ahead of one is
    // refused by phase.
    let mut driver = ConnectionDriver::daemon();
    let (effective_minor, carrier) = with_deadline(
        caps.handshake.preface,
        HandshakePhase::Preface,
        exchange_bootstrap(&mut read_half, &mut write_half),
    )
    .await?;
    driver.preface_done();
    // The lease lives exactly as long as this connection: the forwarded
    // agent socket dies with the SSH link, and a registration held past
    // it would leave the stable link naming a dead socket.
    let _agent = carrier
        .as_ref()
        .and_then(forwarded_agent_socket)
        .zip(caps.agent.as_ref())
        .map(|(target, link)| link.register(target));
    let relay_env = carrier.map(|block| block.env);

    let mut reader = FrameReader::new(read_half);
    // One syscall per multi-frame cycle; see `pump_subscriber`'s flush
    // boundary.
    let mut writer = FrameWriter::new(
        tokio::io::BufWriter::with_capacity(WRITE_BUF_CAPACITY, write_half),
        effective_minor,
    );

    let params = handshake(
        &mut reader,
        &mut writer,
        &mut driver,
        effective_minor,
        caps.handshake.hello,
    )
    .await?;

    let Some((attached, mut out_rx, ack)) = wait_for_attach(
        &mut reader,
        &mut writer,
        &mut driver,
        params,
        &caps,
        relay_env.as_deref(),
        &pool,
        &factory,
    )
    .await?
    else {
        return Ok(());
    };
    info!(id = ?attached.id, minor = params.effective_minor, "session attached");

    // From the subscribe reply, never a second pool lookup: a concurrent
    // destroy could have removed the handle, leaving the client waiting
    // for an ack that never comes, and the row the spawn minted predates
    // the subscription both acks report.
    let (ack, registered) = match ack {
        Ack::Attached => (
            SessionToClientMsg::Attached {
                info: attached.ok.info.clone(),
            },
            None,
        ),
        Ack::Created { registered } => (
            SessionToClientMsg::Created {
                info: attached.ok.info.clone(),
            },
            Some(registered),
        ),
    };
    // Published before the write, so the id is nameable by the time any
    // peer can read it. Publishing after it would refuse a re-attach that
    // outruns the publish; a lost ack leaves the session listed and
    // detached instead, as a lost `Spawn` reply does.
    if let Some(registered) = registered {
        let _kept = registered.publish().await;
    }
    let result = match writer.send(&ack).await {
        Ok(()) => {
            pump_subscriber(
                &mut reader,
                &mut writer,
                &mut driver,
                &attached,
                &mut out_rx,
                CreateCtx {
                    pool: &pool,
                    caps: &caps,
                    factory: &factory,
                    relay_env: relay_env.as_deref(),
                },
            )
            .await
        }
        Err(err) => Err(err.into()),
    };
    debug!(id = ?attached.id, "session detaching");
    // Best-effort: on eviction or session end the task already dropped
    // this subscriber.
    drop(
        attached
            .cmd
            .send(SessionCmd::Unsubscribe {
                sub: attached.ok.sub,
            })
            .await,
    );
    result
}

pub async fn handle_connection(
    stream: ServerStream,
    caps: DaemonCaps,
    pool: Arc<Mutex<SessionPool>>,
    factory: SessionFactory,
) -> Result<(), ConnError> {
    let (read_half, write_half) = server_split(stream);
    handle_stream(read_half, write_half, caps, pool, factory).await
}

/// Answer a peer the daemon has no permit for, then close. Spawned
/// rather than awaited so one silent over-cap peer cannot stall the
/// accept loop for the length of its handshake deadlines.
fn refuse_over_capacity(stream: ServerStream, caps: &DaemonCaps) {
    let Some(slot) = caps.admission.refusal_slot() else {
        // Dropping the socket unanswered is the last resort, and the
        // one case a client cannot tell from a crashed daemon; it costs
        // a retry, where an unbounded refusal path would cost the
        // bound this whole ledger exists for.
        warn!(
            "at connection capacity with every refusal slot taken; dropping a connection unanswered"
        );
        return;
    };
    let deadlines = caps.handshake;
    // The count is the limit, not a fresh `served()` sample: the failed
    // admission above already proved every permit was held, while a
    // second read can catch a peer leaving and refuse a dial with "at
    // 1023 of 1024", a contradiction the reader would have to explain.
    let limit = caps.admission.limit();
    tokio::spawn(async move {
        let _refusing = slot;
        if let Err(err) = write_capacity_refusal(stream, deadlines, limit).await {
            debug!(?err, "over-capacity peer left before it was refused");
        }
    });
}

/// The refusal is a frame, not a bare close: to `felis` and the bridge
/// a closed socket is indistinguishable from a crashed daemon, and
/// they must report `at_capacity` (retry) rather than `daemon` (exit
/// 2). The preface still runs: it is a fixed 8-byte exchange, and the
/// peer must agree on a major before it can decode a refusal at all.
async fn write_capacity_refusal(
    stream: ServerStream,
    deadlines: HandshakeDeadlines,
    limit: usize,
) -> Result<(), ConnError> {
    let (mut read_half, mut write_half) = server_split(stream);
    let (effective_minor, _carrier) = with_deadline(
        deadlines.preface,
        HandshakePhase::Preface,
        exchange_bootstrap(&mut read_half, &mut write_half),
    )
    .await?;
    let mut reader = FrameReader::new(read_half);
    // The `Hello` is read and discarded: a client writes it before it
    // reads, so answering the socket mid-write is what leaves the
    // refusal unread on some carriers.
    let _hello = with_deadline(deadlines.hello, HandshakePhase::Hello, reader.next_frame()).await?;
    let mut writer = FrameWriter::new(write_half, effective_minor);
    // Deadline-bounded like the reads before it: a peer that completes
    // the handshake and then stops reading would otherwise hold one of
    // the few refusal slots forever, and enough such peers silence the
    // refusal path: the one failure a client cannot tell from a
    // crashed daemon.
    with_deadline(
        deadlines.hello,
        HandshakePhase::Hello,
        write_conn(
            &mut writer,
            &ConnToClientMsg::Refused {
                reason: RefusalReason::AtCapacity,
                detail: format!("the daemon is at {limit} of {limit} connections"),
            },
        ),
    )
    .await
}

/// Bound one pre-attach phase. A peer that says nothing is a hung
/// carrier or a probe, not an operator error, so the cut is logged at
/// `debug`.
async fn with_deadline<T, E, F>(
    limit: Duration,
    phase: HandshakePhase,
    step: F,
) -> Result<T, ConnError>
where
    F: Future<Output = Result<T, E>>,
    E: Into<ConnError>,
{
    match tokio::time::timeout(limit, step).await {
        Ok(done) => done.map_err(Into::into),
        Err(_elapsed) => {
            debug!(%phase, "cutting a silent peer at its handshake deadline");
            Err(ConnError::HandshakeTimeout { phase })
        }
    }
}

/// Bytes one input message must reserve against the session's budget.
///
/// Returns `None` for messages that reach no PTY. Paste reservations include
/// bracketing overhead unconditionally to prevent mode races on write.
fn input_reservation_bytes(msg: &InputMsg) -> Result<Option<u32>, ConnError> {
    use felis_protocol::limits::{
        MAX_KEY_CHARACTER_BYTES, MAX_KEY_REPORT_BYTES, MAX_KEY_TEXT_BYTES, MAX_MOUSE_REPORT_BYTES,
        MAX_PASTE_BYTES, PASTE_BRACKET_OVERHEAD, PTY_INPUT_BUDGET,
    };

    let bytes = match msg {
        InputMsg::KeyBytes(bytes) => bytes.len(),
        // A mouse event is client input like a keystroke, not a reply
        // the actor generated: dropping it silently under a full gauge
        // would deliver a press whose release never lands, leaving a
        // TUI in a drag no further input clears.
        InputMsg::Mouse(_) => MAX_MOUSE_REPORT_BYTES,
        // A key report's width depends on modes the child can change
        // between admission and the write, so the mode-independent worst
        // case is what the permit has to cover.
        InputMsg::Key(event) => {
            let character = match &event.key {
                felis_protocol::messages::Key::Character(s) => s.len(),
                _ => 0,
            };
            if character > MAX_KEY_CHARACTER_BYTES {
                return Err(ConnError::InputOverLimit {
                    bytes: character,
                    limit: MAX_KEY_CHARACTER_BYTES,
                });
            }
            let text = event.text.as_ref().map_or(0, String::len);
            if text > MAX_KEY_TEXT_BYTES {
                return Err(ConnError::InputOverLimit {
                    bytes: text,
                    limit: MAX_KEY_TEXT_BYTES,
                });
            }
            MAX_KEY_REPORT_BYTES
        }
        InputMsg::Paste(bytes) => {
            if bytes.len() > MAX_PASTE_BYTES {
                return Err(ConnError::InputOverLimit {
                    bytes: bytes.len(),
                    limit: MAX_PASTE_BYTES,
                });
            }
            bytes.len() + PASTE_BRACKET_OVERHEAD
        }
        _ => return Ok(None),
    };
    if bytes > PTY_INPUT_BUDGET {
        return Err(ConnError::InputOverLimit {
            bytes,
            limit: PTY_INPUT_BUDGET,
        });
    }
    Ok(Some(u32::try_from(bytes).unwrap_or(u32::MAX)))
}

struct AttachedSub {
    id: SessionId,
    ok: SubscribeOk,
    cmd: mpsc::Sender<SessionCmd>,
    /// The session's input budget; this connection's share of it is
    /// acquired per input message and travels with the bytes.
    input_budget: Arc<Semaphore>,
    buffered: Arc<std::sync::atomic::AtomicUsize>,
    /// Weak handle for inbound pump replies routed through the outbox.
    ///
    /// Must remain weak so `pump_outbound` terminates once the session task
    /// drops the subscriber channel.
    out: mpsc::WeakUnboundedSender<OutEvent>,
}

impl AttachedSub {
    /// Queue one event this pump answered itself; `false` means the
    /// subscription is over. It goes through the same byte gauge as the
    /// session task's pushes because [`write_out_event`] settles that
    /// gauge for every event it writes.
    fn queue(&self, ev: OutEvent) -> bool {
        let Some(out) = self.out.upgrade() else {
            return false;
        };
        let len = ev.approx_wire_len();
        if self
            .buffered
            .fetch_add(len, std::sync::atomic::Ordering::Relaxed)
            + len
            > session_task::SUBSCRIBER_BUFFER_CAP
        {
            warn!(sub = ?self.ok.sub, "reply would overflow the outbox; ending the connection");
            return false;
        }
        out.send(ev).is_ok()
    }
}

/// Which ack the caller owes once [`wait_for_attach`] hands back a
/// subscription.
enum Ack {
    Attached,
    Created {
        /// Still armed: nothing can name the row until the caller
        /// publishes it just before the ack.
        registered: Registered,
    },
}

struct Subscription {
    ok: SubscribeOk,
    out_rx: mpsc::UnboundedReceiver<OutEvent>,
    buffered: Arc<std::sync::atomic::AtomicUsize>,
    out: mpsc::WeakUnboundedSender<OutEvent>,
}

/// Why a subscribe did not land.
#[derive(Debug, Clone, Copy)]
enum SubscribeFailure {
    /// The task ended between the lookup and the send, or dropped the
    /// reply.
    Ending,
    /// A `live_only` request met a shell that had already exited.
    Exited,
}

impl SubscribeFailure {
    fn refusal(self, id: SessionId) -> (AttachFailure, String) {
        match self {
            Self::Ending => (
                AttachFailure::SessionEnding,
                format!("session {:#x} is shutting down", id.0),
            ),
            Self::Exited => (
                AttachFailure::SessionExited,
                format!("session {:#x} has already exited", id.0),
            ),
        }
    }
}

/// Subscribe this connection to a session task.
async fn subscribe_to(
    cmd: &mpsc::Sender<SessionCmd>,
    mode: ConnectionMode,
    pull_paced: bool,
    live_only: bool,
) -> Result<Subscription, SubscribeFailure> {
    let (tx, out_rx) = mpsc::unbounded_channel();
    let out = tx.downgrade();
    let buffered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (reply_tx, reply_rx) = oneshot::channel();
    let req = SubscribeReq {
        mode,
        pull_paced,
        live_only,
        tx,
        buffered: Arc::clone(&buffered),
        reply: reply_tx,
    };
    // Mid-teardown (reaped between the lookup and here): same user
    // outcome as an unknown id.
    if cmd.send(SessionCmd::Subscribe(req)).await.is_err() {
        return Err(SubscribeFailure::Ending);
    }
    match reply_rx.await {
        Ok(Ok(ok)) => Ok(Subscription {
            ok,
            out_rx,
            buffered,
            out,
        }),
        Ok(Err(SubscribeRefused::Exited)) => Err(SubscribeFailure::Exited),
        Err(_) => Err(SubscribeFailure::Ending),
    }
}

/// What a spawn needs from its connection.
#[derive(Clone, Copy)]
struct CreateCtx<'a> {
    pool: &'a Arc<Mutex<SessionPool>>,
    caps: &'a DaemonCaps,
    factory: &'a SessionFactory,
    relay_env: Option<&'a [crate::child_env::EnvEntry]>,
}

/// A session in the pool whose creator has not yet been told its id.
/// Nothing else can reach it, so every failure path must
/// [`Self::roll_back`]; `Drop` is only the panic fallback, since it
/// cannot await the child's reap.
struct Registered {
    pool: Arc<Mutex<SessionPool>>,
    id: SessionId,
    cmd: mpsc::Sender<SessionCmd>,
    input_budget: Arc<Semaphore>,
    done: watch::Receiver<bool>,
    live: bool,
}

/// How long rollback waits for the owner task to reap the child.
///
/// Derived from child teardown grace (`session_task::CHILD_TEARDOWN_BUDGET`)
/// plus margin for `run_session` cleanup.
const ROLLBACK_TIMEOUT: Duration = session_task::CHILD_TEARDOWN_BUDGET
    .checked_add(Duration::from_secs(2))
    .expect("the teardown budget is seconds, not an overflow away");

impl Registered {
    /// Make the pool row nameable and hand the session over: past this
    /// the row is shared, so there is nothing left to roll back.
    async fn publish(self) -> mpsc::Sender<SessionCmd> {
        self.pool.lock().await.publish(self.id);
        self.keep()
    }

    /// Hand the session over to the caller that delivered its id.
    fn keep(mut self) -> mpsc::Sender<SessionCmd> {
        self.live = false;
        self.cmd.clone()
    }

    /// Unwind the registration and *wait* for the child to be reaped:
    /// returning earlier would let the connection close while a PTY
    /// child the caller never learned about is still running.
    async fn roll_back(&mut self) {
        let _entry = self.pool.lock().await.remove(self.id);
        // A failed send means the task already ended.
        drop(self.cmd.send(SessionCmd::Shutdown).await);
        let wait = async {
            while !*self.done.borrow_and_update() {
                if self.done.changed().await.is_err() {
                    return;
                }
            }
        };
        if tokio::time::timeout(ROLLBACK_TIMEOUT, wait).await.is_err() {
            warn!(id = ?self.id, "rolled-back session did not finish reaping in time");
        }
        // Disarmed last, not first: a cancellation partway through the
        // awaits above must still leave `Drop` holding the session, or
        // the pool entry and its child outlive every reference to them.
        self.live = false;
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        // A panic unwound past every `roll_back`; `Drop` cannot await,
        // so the supervisor is left to finish the reap.
        error!(id = ?self.id, "a half-built session was dropped without a rollback");
        if self.cmd.try_send(SessionCmd::Shutdown).is_ok() {
            // The owner task removes its own pool entry on the way out.
            return;
        }
        // A full command channel would swallow the shutdown and leave a
        // listable session nobody can name, so the rollback moves to a
        // detached task that can block on the send.
        let pool = Arc::clone(&self.pool);
        let cmd = self.cmd.clone();
        let id = self.id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _entry = pool.lock().await.remove(id);
                drop(cmd.send(SessionCmd::Shutdown).await);
            });
        } else {
            error!(id = ?id, "no runtime left to roll back on; the session stays in the pool");
        }
    }
}

/// Admit, spawn and register one session. The refusal is returned
/// rather than written: each entry point phrases it in its own family.
async fn create_session(
    ctx: CreateCtx<'_>,
    args: felis_protocol::messages::SpawnArgs,
) -> Result<(Registered, SessionInfo), (CreateFailure, String)> {
    let CreateCtx {
        pool,
        caps,
        factory,
        relay_env,
    } = ctx;
    // Geometry first, before an id is minted or a PTY allocated: the
    // refusal must not be paid for with an allocation (REQ-605a).
    // Absence is the create asking for the daemon default, and this is
    // the one place that default is resolved: nothing downstream sees a
    // geometry that is not a real size.
    let dims = args
        .dims
        .map(felis_protocol::messages::RequestedDims::admit)
        .transpose()
        .map_err(|rejection| (CreateFailure::GeometryOutOfRange, rejection.to_string()))?
        .unwrap_or(GridDims {
            rows: crate::DEFAULT_ROWS,
            cols: crate::DEFAULT_COLS,
            pixel_w: 0,
            pixel_h: 0,
        });
    // The slot is taken, not the count read: registration happens after
    // the fork/exec, and a bare count would let every create in a burst
    // pass (REQ-915).
    let admitted = pool.lock().await.try_reserve(caps.max_sessions);
    let slot = admitted.map_err(|refusal| match refusal {
        ReserveRefusal::AtCapacity { admitted } => (
            CreateFailure::SessionLimitReached,
            format!(
                "the daemon is at {admitted} of {} sessions; \
                 reap one with `felis sessions kill <id>`",
                caps.max_sessions
            ),
        ),
        ReserveRefusal::Draining => (
            CreateFailure::DaemonDraining,
            "the daemon is draining toward exit and admits no new session; \
             start one on another daemon"
                .to_owned(),
        ),
        ReserveRefusal::Upgrading => (
            CreateFailure::SpawnFailed,
            "the daemon is switching to a new binary; create the session again in a moment"
                .to_owned(),
        ),
    })?;
    // Before the spawn: the child env carries `FELIS_SESSION_ID`, and
    // env is fixed at exec.
    let sid = SessionId::new();
    // A spawn failure reaches the wire so `felis -- /nonexistent` reports
    // the exec error, not a bare EOF.
    let session = spawn_with_args(
        &args,
        factory,
        sid.0,
        env_base_source(&args, relay_env),
        caps.agent.as_deref(),
        caps.endpoint.as_deref(),
    )
    .map_err(|err| (CreateFailure::SpawnFailed, err.to_string()))?;
    // `dims` is already resolved and admitted above; `spawn_session`
    // applies it, it does not default anything.
    let life = session_task::spawn_session(
        pool,
        session,
        caps.idle,
        sid,
        dims,
        args.tags,
        Some(slot),
        Listing::Held,
    )
    .await;
    Ok((
        Registered {
            pool: Arc::clone(pool),
            id: life.id,
            cmd: life.cmd,
            input_budget: life.input_budget,
            done: life.done,
            live: true,
        },
        // From the spawn, never a second pool lookup a concurrent destroy
        // or fast-exiting child could invalidate.
        life.info,
    ))
}

/// Wait for an attach decision. `Ok(None)` when the peer disconnects.
///
/// Dispatches incoming pre-attach frames by kind. Creates spawn and attach,
/// Ops verbs reply correlated, and Notify turns the connection into an observer.
/// Refusals write typed failure responses before closing.
#[allow(clippy::too_many_lines)]
async fn wait_for_attach<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    driver: &mut DaemonDriver,
    params: ConnParams,
    caps: &DaemonCaps,
    relay_env: Option<&[crate::child_env::EnvEntry]>,
    pool: &Arc<Mutex<SessionPool>>,
    factory: &SessionFactory,
) -> Result<Option<(AttachedSub, mpsc::UnboundedReceiver<OutEvent>, Ack)>, ConnError> {
    let ConnParams {
        mode, pull_paced, ..
    } = params;
    // Every pre-attach frame is deadline-bounded for modes that do not idle
    // before operations. This prevents peers that query metadata from holding
    // connection permits indefinitely.
    let timed = !mode.idles_before_first_operation();
    loop {
        let next = if timed {
            with_deadline(
                caps.handshake.first_op,
                HandshakePhase::FirstOperation,
                reader.next_frame(),
            )
            .await?
        } else {
            reader.next_frame().await?
        };
        let Some(frame) = next else {
            return Ok(None);
        };
        // Held through the frame: whatever it attaches, creates or stops
        // lands before an upgrade's barrier closes, or after it reopens.
        let dispatching = caps.upgrade.gate.dispatch().await;
        let classified = match driver.classify(&frame) {
            Ok(classified) => classified,
            // The driver's kind-level mode gate fires before any arm-level one
            // below, so the typed `Conn::Refused` is written here too.
            Err(DriverError::ModeDenied { mode, kind }) => {
                return Err(refuse_mode(writer, mode, attempted_phrase(kind)).await);
            }
            Err(err) => return Err(err.into()),
        };
        let payload = match classified {
            Incoming::Payload(payload) => payload,
            // A second Hello is the peer losing its place, not a frame to skip.
            Incoming::Control(msg) => {
                return Err(ConnError::Driver(DaemonDriver::unexpected(
                    "an attach, create, ops or subscribe frame",
                    msg.variant(),
                )));
            }
            Incoming::CancelIgnored { stream_id } => {
                return Err(ConnError::Driver(DaemonDriver::correlation_violation(
                    MessageKind::Conn,
                    "a cancel only after a stream was opened",
                    format!("stream {stream_id}, before any attach"),
                )));
            }
        };
        match payload.kind {
            MessageKind::Session => match deliver(driver.decode::<SessionToDaemonMsg>(&payload)?)
                .ok_or(ConnError::ExpectedAttachOrCreate)?
                .msg
            {
                SessionToDaemonMsg::Create { args } => {
                    // The spawn's roster row predates the subscription,
                    // and `Created` promises the row of an attached
                    // session.
                    let (mut registered, _) = match create_session(
                        CreateCtx {
                            pool,
                            caps,
                            factory,
                            relay_env,
                        },
                        args,
                    )
                    .await
                    {
                        Ok(created) => created,
                        Err((reason, detail)) => {
                            write_attach_failed(writer, AttachRefusal::Create(reason), detail)
                                .await?;
                            return Ok(None);
                        }
                    };
                    // Not live-only: `felis -- true` exits before the subscribe
                    // lands, and a live-only create would refuse the window its
                    // own command's output.
                    let subscribed = subscribe_to(&registered.cmd, mode, pull_paced, false).await;
                    let id = registered.id;
                    let Subscription {
                        ok,
                        out_rx,
                        buffered,
                        out,
                    } = match subscribed {
                        Ok(sub) => sub,
                        Err(failure) => {
                            // Nothing else knows this session's id yet, so the pool
                            // entry and its child would be unreachable forever.
                            registered.roll_back().await;
                            // The spawn half already succeeded, so this
                            // is the create's attach half refusing.
                            let (reason, detail) = failure.refusal(id);
                            write_attach_failed(writer, AttachRefusal::Attach(reason), detail)
                                .await?;
                            return Ok(None);
                        }
                    };
                    let cmd = registered.cmd.clone();
                    let input_budget = Arc::clone(&registered.input_budget);
                    driver.attached();
                    return Ok(Some((
                        AttachedSub {
                            id,
                            ok,
                            cmd,
                            input_budget,
                            buffered,
                            out,
                        },
                        out_rx,
                        Ack::Created { registered },
                    )));
                }
                SessionToDaemonMsg::Attach { target, live_only } => {
                    let taken = take_attach_handle(pool, &target).await;
                    let (id, handle) = match taken {
                        Ok(taken) => taken,
                        Err((reason, detail)) => {
                            write_attach_failed(writer, AttachRefusal::Attach(reason), detail)
                                .await?;
                            return Ok(None);
                        }
                    };
                    let subscribed = subscribe_to(&handle.cmd, mode, pull_paced, live_only).await;
                    let Subscription {
                        ok,
                        out_rx,
                        buffered,
                        out,
                    } = match subscribed {
                        Ok(sub) => sub,
                        Err(failure) => {
                            let (reason, detail) = failure.refusal(id);
                            write_attach_failed(writer, AttachRefusal::Attach(reason), detail)
                                .await?;
                            return Ok(None);
                        }
                    };
                    driver.attached();
                    return Ok(Some((
                        AttachedSub {
                            id,
                            ok,
                            cmd: handle.cmd,
                            input_budget: handle.input_budget,
                            buffered,
                            out,
                        },
                        out_rx,
                        Ack::Attached,
                    )));
                }
                // The attached-phase arms; the driver's phase column
                // already refused them.
                SessionToDaemonMsg::Detach
                | SessionToDaemonMsg::ConfigureTheme { .. }
                | SessionToDaemonMsg::InputFence => {
                    return Err(ConnError::ExpectedAttachOrCreate);
                }
            },
            MessageKind::Ops => {
                let decoded = match driver.decode::<OpsToDaemonMsg>(&payload) {
                    Ok(decoded) => decoded,
                    // Pre-attach there is still a writer for the typed
                    // `Conn::Refused`, so a denied verb ends the connection
                    // here rather than riding the envelope.
                    Err(DriverError::ArmDenied { mode, .. }) => {
                        return Err(refuse_mode(writer, mode, "operate on other sessions").await);
                    }
                    Err(err) => return Err(err.into()),
                };
                let delivered = deliver(decoded).ok_or(ConnError::ExpectedAttachOrCreate)?;
                // Every verb replies and the connection lives on; the
                // request id the driver validated tells the replies apart.
                let correlation = Correlation::request(delivered.request());
                if let OpsToDaemonMsg::Upgrade { successor } = &delivered.msg {
                    drop(dispatching);
                    upgrade_connection(writer, correlation, successor, pool, caps).await?;
                    continue;
                }
                let ctx = CreateCtx {
                    pool,
                    caps,
                    factory,
                    relay_env,
                };
                let reply = ops_reply(ctx, delivered.msg).await;
                let stopping = matches!(
                    reply,
                    OpsToClientMsg::StopReply {
                        outcome: StopOutcome::Stopping
                    }
                );
                let sent = writer.send_correlated(&reply, correlation).await;
                if stopping {
                    // Fired even when the reply never reached the
                    // requester: the pool is already draining or
                    // destroyed, so a daemon that skipped the shutdown
                    // here would serve nothing and never exit.
                    caps.shutdown.fire();
                    sent?;
                    return Ok(None);
                }
                sent?;
            }
            MessageKind::Notify => {
                match driver.decode::<NotifyToDaemonMsg>(&payload)? {
                    // The bound refuses the stream, never the connection.
                    Delivery::RefuseStream { reply } => {
                        writer.send(&reply).await?;
                    }
                    // No stream of this connection's was ever canceled.
                    Delivery::DroppedAfterCancel => return Ok(None),
                    Delivery::Deliver(delivered) => {
                        // The driver validated the opener's id against
                        // the arm's class, so the ack and the terminal
                        // read it off the delivery instead of the body.
                        let stream = delivered.stream();
                        let NotifyToDaemonMsg::Subscribe { session_prefix } = delivered.msg;
                        // The role changes before the ack is written: a
                        // session frame arriving between the two is
                        // refused by phase.
                        driver.observing();
                        // docs/reference/protocols/notifications.md. Subscribe before
                        // resolving, under one lock: an event published while the filter
                        // resolves must already be in this receiver's queue, or `--once`
                        // waits for a notification that was sent.
                        let (mut rx, filter) = {
                            let guard = pool.lock().await;
                            let rx = guard.subscribe_notifications();
                            let filter = session_prefix.as_deref().map(|prefix| {
                                felis_protocol::session_prefix::resolve_session_prefix(
                                    prefix,
                                    guard.ids().map(|id| id.0),
                                )
                            });
                            (rx, filter)
                        };
                        writer
                            .send_correlated(
                                &NotifyToClientMsg::Subscribed { filter },
                                Correlation::stream(stream),
                            )
                            .await?;
                        let only = match filter {
                            None => None,
                            Some(ResolvedId::Ok { id }) => Some(id),
                            // The ack said so; end the stream rather than leave a bare EOF.
                            Some(ResolvedId::NoMatch | ResolvedId::Ambiguous { .. }) => {
                                end_observer(
                                    writer,
                                    driver,
                                    stream,
                                    0,
                                    Some((
                                        StreamErrorReason::InvalidRequest,
                                        "--session matched no live session".to_owned(),
                                    )),
                                )
                                .await?;
                                return Ok(None);
                            }
                        };
                        // The observer runs until the subscriber leaves; an
                        // upgrade ends it at the exec instead of waiting.
                        drop(dispatching);
                        let sent = run_notification_observer(
                            reader, writer, driver, stream, only, &mut rx,
                        )
                        .await?;
                        end_observer(writer, driver, stream, sent, None).await?;
                        return Ok(None);
                    }
                }
            }
            kind => {
                return Err(ConnError::Driver(DaemonDriver::unexpected(
                    "an attach, create, ops or subscribe frame",
                    kind.as_str(),
                )));
            }
        }
    }
}

/// `None` collapses every non-answer: no such session, the task ended,
/// or it dropped the reply. An `Ops` one-shot answers all three
/// identically.
async fn ask_session<T>(
    pool: &Arc<Mutex<SessionPool>>,
    id: SessionId,
    make_cmd: impl FnOnce(oneshot::Sender<T>) -> SessionCmd,
) -> Option<T> {
    let handle = pool.lock().await.handle_cloned(id)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    handle.cmd.send(make_cmd(reply_tx)).await.ok()?;
    reply_rx.await.ok()
}

/// A session whose task is already gone reports nothing queued, not a
/// denial: nothing named a window that failed to resolve.
async fn push_to_session(
    pool: &Arc<Mutex<SessionPool>>,
    id: SessionId,
    msg: PushMsg,
    scope: SwitchScope,
) -> PushOutcome {
    ask_session(pool, id, |reply| SessionCmd::PushToTarget {
        msg,
        scope,
        reply,
    })
    .await
    .unwrap_or(PushOutcome::Accepted(0))
}

/// A denial reports `queued: 0` beside it; a caller reading only the
/// count still reads the truth.
const fn switched(
    from: ResolvedId,
    to: Option<ResolvedId>,
    outcome: PushOutcome,
) -> OpsToClientMsg {
    let (queued, denied) = match outcome {
        PushOutcome::Accepted(queued) => (queued, None),
        PushOutcome::Denied(reason) => (0, Some(reason)),
    };
    OpsToClientMsg::Switched {
        from,
        to,
        queued,
        denied,
    }
}

/// Answer one `OpsToDaemonMsg` verb. Pure in the connection (neither writes
/// nor decides whether it lives), so the pre-attach loop and the
/// attached pump, which must queue its reply behind the frames already
/// on the outbox, share one implementation.
async fn ops_reply(ctx: CreateCtx<'_>, msg: OpsToDaemonMsg) -> OpsToClientMsg {
    let (pool, caps) = (ctx.pool, ctx.caps);
    match msg {
        OpsToDaemonMsg::List => {
            let sessions = build_session_list(pool).await;
            OpsToClientMsg::Listed { sessions }
        }
        OpsToDaemonMsg::Destroy { id_prefix } => {
            let resolved = resolve_prefix(pool, &id_prefix).await;
            if let ResolvedId::Ok { id } = resolved {
                let handle = pool.lock().await.remove(SessionId(id));
                if let Some(handle) = handle {
                    // A failed send means the task already ended.
                    drop(handle.cmd.send(SessionCmd::Shutdown).await);
                }
            }
            OpsToClientMsg::Destroyed { resolved }
        }
        OpsToDaemonMsg::ForceDetach { id_prefix } => {
            // control-surfaces.md: the reply arrives after the eviction
            // completed, so `was_attached` means "drivable now".
            let resolved = resolve_prefix(pool, &id_prefix).await;
            let was_attached = if let ResolvedId::Ok { id } = resolved {
                ask_session(pool, SessionId(id), |reply| SessionCmd::EvictAll { reply })
                    .await
                    .is_some_and(|n| n > 0)
            } else {
                false
            };
            OpsToClientMsg::Detached {
                resolved,
                was_attached,
            }
        }
        OpsToDaemonMsg::Switch {
            from_prefix,
            target,
            scope,
        } => {
            // control-surfaces.md: the daemon only resolves and routes; each
            // client performs its own re-attach or re-dial (Principle 3). The
            // scope resolves inside the session actor
            // (`SessionCmd::PushToTarget`).
            let from = resolve_prefix(pool, &from_prefix).await;
            match target {
                SwitchTarget::Session(to_prefix) => {
                    let to = resolve_prefix(pool, &to_prefix).await;
                    // The count means "took the push", never "landed on the new
                    // session".
                    let outcome = match (from, to) {
                        // A session switching to itself needs no push; resolving the scope
                        // could only invent a denial for it.
                        (ResolvedId::Ok { id: from_id }, ResolvedId::Ok { id })
                            if from_id == id =>
                        {
                            PushOutcome::Accepted(0)
                        }
                        (ResolvedId::Ok { id: from_id }, ResolvedId::Ok { id }) => {
                            push_to_session(
                                pool,
                                SessionId(from_id),
                                PushMsg::Reattach { id },
                                scope,
                            )
                            .await
                        }
                        _ => PushOutcome::Accepted(0),
                    };
                    switched(from, Some(to), outcome)
                }
                SwitchTarget::Carrier(target) => {
                    // The target lives on another daemon: nothing local to resolve
                    // (`to: None`).
                    let outcome = if let ResolvedId::Ok { id } = from {
                        push_to_session(
                            pool,
                            SessionId(id),
                            PushMsg::RetargetHost { target },
                            scope,
                        )
                        .await
                    } else {
                        PushOutcome::Accepted(0)
                    };
                    switched(from, None, outcome)
                }
            }
        }
        OpsToDaemonMsg::Tag {
            id_prefix,
            add,
            remove,
        } => {
            // control-surfaces.md: felis never interprets a tag. Mutated through
            // the handle's shared meta, so a parked session relabels without
            // waking its task.
            let resolved = resolve_prefix(pool, &id_prefix).await;
            let (tags, denied) = if let ResolvedId::Ok { id } = resolved {
                let handle = pool.lock().await.handle_cloned(SessionId(id));
                handle.map_or_else(
                    || (Vec::new(), None),
                    |handle| {
                        let mut meta = handle
                            .meta
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let denied = crate::pool::apply_tag_delta(&mut meta.tags, &add, &remove)
                            .err()
                            .map(|e| e.to_string());
                        (meta.tags.iter().cloned().collect(), denied)
                    },
                )
            } else {
                (Vec::new(), None)
            };
            OpsToClientMsg::TagsUpdated {
                resolved,
                tags,
                denied,
            }
        }
        OpsToDaemonMsg::Info { id_prefix } => {
            // One roster snapshot for the resolution, the row and the
            // short id: the three would otherwise be able to disagree
            // about which sessions exist.
            let sessions = build_session_list(pool).await;
            let outcome = match felis_protocol::session_prefix::resolve_session_prefix(
                &id_prefix,
                sessions.iter().map(|info| info.id),
            ) {
                ResolvedId::Ok { id } => {
                    let short_id = felis_protocol::session_prefix::short_session_prefix(
                        id,
                        sessions.iter().map(|info| info.id),
                    );
                    let session = sessions.into_iter().find(|info| info.id == id);
                    match session {
                        Some(session) => InfoOutcome::Found {
                            session: Box::new(session),
                            short_id,
                        },
                        None => InfoOutcome::NoMatch,
                    }
                }
                ResolvedId::NoMatch => InfoOutcome::NoMatch,
                ResolvedId::Ambiguous { matches } => InfoOutcome::Ambiguous { matches },
            };
            OpsToClientMsg::InfoReply { outcome }
        }
        OpsToDaemonMsg::Status => daemon_status(pool, caps).await,
        OpsToDaemonMsg::Stop { mode } => OpsToClientMsg::StopReply {
            outcome: stop_daemon(pool, caps, mode).await,
        },
        // Answered by `upgrade_connection`: the exec must follow the reply
        // on the requesting connection.
        OpsToDaemonMsg::Upgrade { .. } => OpsToClientMsg::UpgradeReply {
            outcome: crate::upgrade::refused(&crate::upgrade::Refusal::Busy),
        },
        OpsToDaemonMsg::Spawn { args } => {
            let outcome = match create_session(ctx, args).await {
                // Nothing here subscribes, so the row is published at
                // once: a lost reply leaves the session listed rather
                // than rolled back.
                Ok((registered, info)) => {
                    let _kept = registered.publish().await;
                    SpawnOutcome::Ok {
                        info: Box::new(info),
                    }
                }
                Err((reason, detail)) => SpawnOutcome::Refused { reason, detail },
            };
            OpsToClientMsg::Spawned { outcome }
        }
    }
}

/// Answer an `Ops::Stop` (`docs/reference/cli.md` "Daemon stop"). The
/// emptiness decision and the drain flag are one critical section under
/// the pool lock: a create that reserves after `Stopping` was decided
/// cannot exist, because the same flag refuses the reservation. The
/// shutdown fires in the caller, once this reply is on the wire.
async fn stop_daemon(
    pool: &Arc<Mutex<SessionPool>>,
    caps: &DaemonCaps,
    mode: StopMode,
) -> StopOutcome {
    match mode {
        StopMode::IfEmpty => {
            let mut guard = pool.lock().await;
            let sessions = admitted_count(guard.admitted());
            if guard.draining() {
                return StopOutcome::Draining { sessions };
            }
            if sessions > 0 {
                return StopOutcome::Refused { sessions };
            }
            guard.start_draining();
            drop(guard);
            // An empty pool can still owe a reap: a session that removed
            // itself a moment ago is gone from the count while its child
            // is being hung up.
            drain_to_empty(pool, false).await;
            StopOutcome::Stopping
        }
        StopMode::WhenEmpty => {
            let sessions = {
                let mut guard = pool.lock().await;
                guard.start_draining();
                admitted_count(guard.admitted())
            };
            if sessions == 0 {
                drain_to_empty(pool, false).await;
                return StopOutcome::Stopping;
            }
            let pool = Arc::clone(pool);
            let shutdown = Arc::clone(&caps.shutdown);
            tokio::spawn(async move {
                drain_to_empty(&pool, false).await;
                shutdown.fire();
            });
            StopOutcome::Draining { sessions }
        }
        StopMode::Force => {
            // Drain first: without it a create already past its own
            // admission check could register a session after the sweep
            // and outlive the daemon that was told to take it down.
            pool.lock().await.start_draining();
            drain_to_empty(pool, true).await;
            StopOutcome::Stopping
        }
    }
}

/// Answers an `Ops::Upgrade` (`docs/reference/cli.md` "Daemon upgrade"):
/// `Upgrading` goes out before the exec, which closes this connection
/// with every other.
async fn upgrade_connection<W>(
    writer: &mut FrameWriter<W>,
    correlation: Correlation,
    successor: &str,
    pool: &Arc<Mutex<SessionPool>>,
    caps: &DaemonCaps,
) -> Result<(), ConnError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    #[cfg(unix)]
    {
        let prepared = crate::upgrade::prepare(PathBuf::from(successor), pool, &caps.upgrade).await;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(refusal) => {
                warn!(%refusal, "upgrade refused");
                let reply = OpsToClientMsg::UpgradeReply {
                    outcome: crate::upgrade::refused(&refusal),
                };
                writer.send_correlated(&reply, correlation).await?;
                return Ok(());
            }
        };
        let reply = OpsToClientMsg::UpgradeReply {
            outcome: felis_protocol::messages::UpgradeOutcome::Upgrading,
        };
        if let Err(err) = writer.send_correlated(&reply, correlation).await {
            prepared.abandon().await;
            return Err(err.into());
        }
        let refusal = prepared.exec().await;
        error!(%refusal, "upgrade: the exec failed after it was announced; serving on");
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (successor, pool, caps);
        let reply = OpsToClientMsg::UpgradeReply {
            outcome: crate::upgrade::refused(&crate::upgrade::Refusal::Unsupported(
                "this platform drains and restarts instead".to_owned(),
            )),
        };
        writer.send_correlated(&reply, correlation).await?;
        Ok(())
    }
}

/// Saturates rather than wraps: the cap is far below `u32::MAX`, and a
/// count that wrapped would report an empty daemon.
fn admitted_count(admitted: usize) -> u32 {
    u32::try_from(admitted).unwrap_or(u32::MAX)
}

/// Wait until the pool admits nothing (no registered session, no
/// reservation, no child still being reaped) and every session task it
/// held has returned, destroying what it holds on each pass when
/// `destroy` is set. A create that registers between two passes is
/// caught by the next one, so a forced stop leaves no child behind.
async fn drain_to_empty(pool: &Arc<Mutex<SessionPool>>, destroy: bool) {
    let settled = pool.lock().await.settled();
    loop {
        // Armed before the count is read: a slot released between the
        // read and the await must not be a wake this loop sleeps
        // through.
        let woken = settled.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        let handles = {
            let mut guard = pool.lock().await;
            if guard.quiescent() {
                return;
            }
            if destroy {
                guard.drain_sessions()
            } else {
                guard.live_handles()
            }
        };
        if handles.is_empty() {
            woken.await;
            continue;
        }
        for handle in handles {
            // A failed send means the task already ended.
            if destroy {
                drop(handle.cmd.send(SessionCmd::Shutdown).await);
            }
            // The receiver drops when the session task returns, which
            // is what reaps the child: returning at the pool count
            // alone would let the daemon exit while a child it owns is
            // still being hung up.
            handle.cmd.closed().await;
        }
    }
}

/// Build `Ops::StatusReply` (`docs/reference/cli.md` "Daemon status").
///
/// Samples live state from session actors during reply construction,
/// computing totals and maxima in one pass without hot-path counters.
async fn daemon_status(pool: &Arc<Mutex<SessionPool>>, caps: &DaemonCaps) -> OpsToClientMsg {
    use felis_protocol::messages::{
        Limit, ReportScope, ResourceKind, ResourceReport, ResourceUnit, SubjectKind,
    };

    // Read admitted count (registered plus reserved slots, REQ-915) under the
    // same lock as session ids so status matches refusal checks. Held rows
    // count towards admission even though omitted from namable ids.
    let (ids, sessions, draining): (Vec<SessionId>, usize, bool) = {
        let guard = pool.lock().await;
        (guard.ids().collect(), guard.admitted(), guard.draining())
    };
    let mut image_bytes = Aggregate::default();
    // The cap read off the stores, not the constant: the report must name
    // the ceiling an insert is refused against. The loosest in force, so
    // a larger store cannot hide behind a smaller neighbor.
    let mut image_cap = crate::pool::DEFAULT_IMAGE_BYTE_CAP as u64;
    let mut decodes: u64 = 0;
    let mut decode_bytes = Aggregate::default();
    let mut outbox = Aggregate::default();
    let mut pty_input = Aggregate::default();
    for id in ids {
        let Some(stats) = ask_session(pool, id, |reply| SessionCmd::Stats { reply }).await else {
            continue;
        };
        image_bytes.observe(stats.image_bytes as u64);
        image_cap = image_cap.max(stats.image_bytes_cap as u64);
        if let Some(bytes) = stats.decode_bytes_in_flight {
            decodes += 1;
            decode_bytes.observe(bytes as u64);
        }
        // A session holds many subscribers, so its own max is the
        // subject sample and its own sum joins the daemon-wide total.
        outbox.total = outbox
            .total
            .saturating_add(stats.total_subscriber_backlog as u64);
        outbox.max = outbox.max.max(stats.max_subscriber_backlog as u64);
        pty_input.observe(stats.pty_input_bytes as u64);
    }

    let daemon_row = |resource, unit, total_used, global_limit| ResourceReport {
        resource,
        unit,
        total_used,
        scope: ReportScope::Daemon { global_limit },
    };
    let subject_row = |resource, unit, subject, agg: Aggregate, per_subject_limit| ResourceReport {
        resource,
        unit,
        total_used: agg.total,
        scope: ReportScope::Subject {
            subject,
            max_subject_used: agg.max,
            per_subject_limit,
            // No resource carries a daemon-wide byte budget beside its
            // per-subject cap yet; the aggregate is the product of the
            // admission caps and the per-subject ceiling (REQ-915).
            global_limit: Limit::Unlimited,
        },
    };
    let connections = daemon_row(
        ResourceKind::Connections,
        ResourceUnit::Count,
        caps.admission.served() as u64,
        Limit::Bounded(caps.admission.limit() as u64),
    );
    let pty_input_row = subject_row(
        ResourceKind::PtyInputBytes,
        ResourceUnit::Bytes,
        SubjectKind::Session,
        pty_input,
        Limit::Bounded(felis_protocol::limits::PTY_INPUT_BUDGET as u64),
    );
    OpsToClientMsg::StatusReply {
        worker_threads: tokio::runtime::Handle::current().metrics().num_workers() as u32,
        draining,
        resources: [
            connections,
            daemon_row(
                ResourceKind::Sessions,
                ResourceUnit::Count,
                sessions as u64,
                Limit::Bounded(caps.max_sessions as u64),
            ),
            subject_row(
                ResourceKind::ImageStoreBytes,
                ResourceUnit::Bytes,
                SubjectKind::Session,
                image_bytes,
                Limit::Bounded(image_cap),
            ),
            // A session reassembles at most one transmission at a time,
            // so the session cap bounds this count too.
            daemon_row(
                ResourceKind::InFlightDecodes,
                ResourceUnit::Count,
                decodes,
                Limit::Bounded(caps.max_sessions as u64),
            ),
            subject_row(
                ResourceKind::InFlightDecodeBytes,
                ResourceUnit::Bytes,
                SubjectKind::Session,
                decode_bytes,
                Limit::Bounded(felis_vt::kitty_graphics::REASSEMBLY_BUFFER_LIMIT as u64),
            ),
            subject_row(
                ResourceKind::SubscriberQueueBytes,
                ResourceUnit::Bytes,
                SubjectKind::Subscriber,
                outbox,
                Limit::Bounded(session_task::SUBSCRIBER_BUFFER_CAP as u64),
            ),
            pty_input_row,
        ]
        .into(),
    }
}

/// One resource's two samples over the same pass: what every subject
/// holds together, and what the deepest single subject holds.
#[derive(Debug, Default, Clone, Copy)]
struct Aggregate {
    total: u64,
    max: u64,
}

impl Aggregate {
    fn observe(&mut self, subject: u64) {
        self.total = self.total.saturating_add(subject);
        self.max = self.max.max(subject);
    }
}

/// Every refusal path must emit `AttachFailed`, not a bare EOF.
async fn write_attach_failed<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    reason: AttachRefusal,
    detail: String,
) -> Result<(), ConnError> {
    warn!(?reason, %detail, "attach refused");
    writer
        .send(&SessionToClientMsg::AttachFailed { reason, detail })
        .await?;
    Ok(())
}

/// The driver denies at the kind level, coarser than the sentence a
/// user should read.
const fn attempted_phrase(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Notify => "subscribe to notifications",
        MessageKind::Ops => "operate on other sessions",
        MessageKind::Conn
        | MessageKind::Session
        | MessageKind::Input
        | MessageKind::Grid
        | MessageKind::Image
        | MessageKind::Push
        | MessageKind::Region
        | MessageKind::Search => "attach to a session",
    }
}

/// A write failure is reported in place of the mode error: the
/// connection is already unusable, and the IO cause is the more useful
/// in a log.
async fn refuse_mode<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    mode: ConnectionMode,
    attempted: &'static str,
) -> ConnError {
    warn!(?mode, attempted, "refusing a frame outside the peer's mode");
    let refusal = ConnToClientMsg::Refused {
        reason: RefusalReason::Role,
        detail: format!("a {mode:?} connection may not {attempted}"),
    };
    match writer.send(&refusal).await {
        Ok(()) => ConnError::ModeDenied { mode, attempted },
        Err(err) => err.into(),
    }
}

enum Route {
    Cmd(SessionCmd),
    /// Through the outbox, the connection's one writer, or the reply
    /// overtakes the grid frames that preceded it.
    Reply(OutEvent),
    Ops {
        msg: Box<OpsToDaemonMsg>,
        correlation: Correlation,
    },
    Detach,
    /// The driver already settled it (a cancel racing a terminal, or an
    /// item for a canceled stream).
    Settled,
}

/// Names the arm only: `SessionCmd` and `OutEvent` carry channels and
/// whole grid payloads.
impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cmd(_) => "Route::Cmd",
            Self::Reply(_) => "Route::Reply",
            Self::Ops { .. } => "Route::Ops",
            Self::Detach => "Route::Detach",
            Self::Settled => "Route::Settled",
        })
    }
}

/// Turn one inbound frame into its route. A wrong-direction message,
/// an undecodable body, or a correlation violation ends this
/// connection with a typed error; the session and every other
/// connection are untouched.
fn route_frame(
    driver: &mut DaemonDriver,
    frame: &OwnedFrame,
    sub: SubscriberId,
) -> Result<Route, ConnError> {
    let incoming = driver.classify(frame)?;
    let payload = match incoming {
        Incoming::CancelIgnored { stream_id } => {
            debug!(stream = %stream_id, "cancel for an already-terminated stream");
            return Ok(Route::Settled);
        }
        Incoming::Control(ConnToDaemonMsg::Cancel { stream_id }) => {
            return Ok(Route::Cmd(SessionCmd::CancelStream {
                sub,
                stream: stream_id,
            }));
        }
        // The phase column refuses a `Hello` once the handshake is over.
        Incoming::Control(hello @ ConnToDaemonMsg::Hello { .. }) => {
            unreachable!("the driver admitted a Hello on an attached connection: {hello:?}")
        }
        Incoming::Payload(payload) => payload,
    };
    match payload.kind {
        MessageKind::Input => {
            let delivered = deliver(driver.decode::<InputMsg>(&payload)?);
            Ok(delivered.map_or(Route::Settled, |delivered| {
                Route::Cmd(SessionCmd::Input {
                    sub,
                    msg: delivered.msg,
                    reservation: None,
                })
            }))
        }
        MessageKind::Search => match driver.decode::<SearchToDaemonMsg>(&payload)? {
            Delivery::Deliver(delivered) => Ok(Route::Cmd(SessionCmd::Search {
                sub,
                stream: delivered.stream(),
                msg: delivered.msg,
            })),
            Delivery::DroppedAfterCancel => Ok(Route::Settled),
            Delivery::RefuseStream { reply } => Ok(Route::Reply(OutEvent::Control(reply))),
        },
        MessageKind::Region => match driver.decode::<RegionToDaemonMsg>(&payload)? {
            Delivery::Deliver(delivered) => match delivered.msg {
                RegionToDaemonMsg::Request { source, ansi } => Ok(Route::Cmd(SessionCmd::Region {
                    sub,
                    source,
                    ansi,
                    request: delivered.request(),
                })),
                RegionToDaemonMsg::Rows {
                    source,
                    ansi,
                    max_rows,
                } => Ok(Route::Cmd(SessionCmd::RegionRows {
                    sub,
                    source,
                    ansi,
                    max_rows,
                    stream: delivered.stream(),
                })),
            },
            Delivery::DroppedAfterCancel => Ok(Route::Settled),
            Delivery::RefuseStream { reply } => Ok(Route::Reply(OutEvent::Control(reply))),
        },
        MessageKind::Session => {
            let Some(delivered) = deliver(driver.decode::<SessionToDaemonMsg>(&payload)?) else {
                return Ok(Route::Settled);
            };
            match delivered.msg {
                SessionToDaemonMsg::Detach => Ok(Route::Detach),
                SessionToDaemonMsg::ConfigureTheme { fg, bg, cursor } => {
                    Ok(Route::Cmd(SessionCmd::ConfigureTheme {
                        sub,
                        fg,
                        bg,
                        cursor,
                    }))
                }
                SessionToDaemonMsg::InputFence => Ok(Route::Cmd(SessionCmd::InputFence {
                    sub,
                    request: delivered.request(),
                })),
                // The setup-phase openers; the driver's phase column
                // already refused them.
                ref other @ (SessionToDaemonMsg::Attach { .. }
                | SessionToDaemonMsg::Create { .. }) => {
                    Err(ConnError::Driver(DaemonDriver::unexpected(
                        "an attached connection's session traffic",
                        other.variant(),
                    )))
                }
            }
        }
        // Admitted post-attach so a window can re-list without a second
        // connection.
        MessageKind::Ops => {
            let decoded = match driver.decode::<OpsToDaemonMsg>(&payload) {
                Ok(decoded) => decoded,
                // Post-attach there is no writer here for a `Conn::Refused`,
                // so an over-reaching verb is refused through the envelope
                // and the connection survives.
                Err(DriverError::ArmDenied { mode, .. }) => {
                    // The driver validated the id before it judged the
                    // arm, so the refusal can still ride the envelope.
                    let request = request_of(&payload)?;
                    return Ok(Route::Reply(OutEvent::Control(
                        DaemonDriver::refuse_request(
                            request,
                            StreamErrorReason::InvalidRequest,
                            format!("a {mode:?} connection may not operate on other sessions"),
                        ),
                    )));
                }
                Err(err) => return Err(err.into()),
            };
            Ok(
                deliver(decoded).map_or(Route::Settled, |delivered| Route::Ops {
                    correlation: Correlation::request(delivered.request()),
                    msg: Box::new(delivered.msg),
                }),
            )
        }
        kind => Err(ConnError::Driver(DaemonDriver::unexpected(
            "a frame an attached connection may send",
            kind.as_str(),
        ))),
    }
}

/// Unwrap a delivery whose stream accounting cannot refuse (the
/// un-correlated families): `None` means the driver dropped an item for
/// a stream this side canceled.
fn deliver<M>(delivery: Delivery<M>) -> Option<Delivered<M>> {
    match delivery {
        Delivery::Deliver(delivered) => Some(delivered),
        Delivery::DroppedAfterCancel | Delivery::RefuseStream { .. } => None,
    }
}

/// The one place that needs an id the driver validated but holds no
/// [`Delivered`] carrying it: refusing an `Ops` arm whose *decode* was
/// denied, which returns before a delivery exists.
fn request_of(payload: &Payload) -> Result<RequestId, ConnError> {
    payload
        .correlation
        .and_then(Correlation::request_id)
        .ok_or_else(|| {
            ConnError::Driver(DaemonDriver::correlation_violation(
                payload.kind,
                "a request carrying a request_id",
                "no request id",
            ))
        })
}

/// Steady-state subscriber pump running inbound and outbound concurrently.
///
/// Running independent loops ensures backpressure or socket writes in one
/// direction cannot block input processing in the other.
async fn pump_subscriber<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    driver: &mut DaemonDriver,
    attached: &AttachedSub,
    out_rx: &mut mpsc::UnboundedReceiver<OutEvent>,
    ctx: CreateCtx<'_>,
) -> Result<(), ConnError> {
    // The outbound loop writes the terminals the session task minted; the
    // inbound loop owns the driver that retires their ids. Without the
    // report the stream table would count streams opened, not open.
    let (retired_tx, mut retired_rx) = mpsc::unbounded_channel();
    tokio::select! {
        // Inbound first (`biased;`): under pull pacing the client's
        // per-vsync pulls pace emission, so a ready outbound loop must
        // not starve them.
        biased;
        res = pump_inbound(reader, driver, attached, ctx, &mut retired_rx) => res,
        res = pump_outbound(writer, attached, out_rx, &retired_tx) => res,
    }
}

/// Route inbound frames to the session task until detach, hangup, or error.
///
/// Owns the connection driver and retires streams from the outbound pump
/// before processing the next frame.
async fn pump_inbound<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
    driver: &mut DaemonDriver,
    attached: &AttachedSub,
    ctx: CreateCtx<'_>,
    retired: &mut mpsc::UnboundedReceiver<StreamId>,
) -> Result<(), ConnError> {
    while let Some(frame) = reader.next_frame().await? {
        ctx.caps.upgrade.gate.wait_open().await;
        while let Ok(stream) = retired.try_recv() {
            let _freed = driver.retire_stream(stream);
        }
        match route_frame(driver, &frame, attached.ok.sub)? {
            Route::Detach => return Ok(()),
            Route::Cmd(mut cmd) => {
                // Acquired before bytes enter the session command channel,
                // released after the PTY writer sends them to the OS.
                // Backpressure pauses socket reads so memory stays bounded.
                if let SessionCmd::Input {
                    msg, reservation, ..
                } = &mut cmd
                    && let Some(bytes) = input_reservation_bytes(msg)?
                {
                    // While waiting for budget, race peer hangup and session
                    // termination so wedged children do not strand the connection.
                    let permit = tokio::select! {
                        biased;
                        granted = Arc::clone(&attached.input_budget).acquire_many_owned(bytes) => {
                            // Nothing closes the budget; ending the pump
                            // is the answer that stays right if that
                            // changes.
                            let Ok(permit) = granted else { return Ok(()) };
                            permit
                        }
                        hangup = reader.wait_for_hangup() => {
                            hangup?;
                            return Ok(());
                        }
                        () = attached.cmd.closed() => return Ok(()),
                    };
                    *reservation = Some(Box::new(permit));
                }
                let dispatching = ctx.caps.upgrade.gate.dispatch().await;
                // The session task ended (destroy / reap): input has nowhere to go.
                if attached.cmd.send(cmd).await.is_err() {
                    return Ok(());
                }
                drop(dispatching);
            }
            Route::Ops { msg, correlation } => {
                let dispatching = ctx.caps.upgrade.gate.dispatch().await;
                let ev = OutEvent::Ops {
                    msg: Box::new(ops_reply(ctx, *msg).await),
                    correlation,
                };
                drop(dispatching);
                if !attached.queue(ev) {
                    return Ok(());
                }
            }
            Route::Reply(ev) => {
                if !attached.queue(ev) {
                    return Ok(());
                }
            }
            Route::Settled => {}
        }
    }
    Ok(())
}

/// Drain the outbox onto the wire. The only writer on this connection,
/// so the outbound stream stays FIFO: a `Region` / `Search` reply
/// orders after the `RehydrateEnd` a verb drains to, an
/// `ImageComplete` after its chunks.
async fn pump_outbound<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    attached: &AttachedSub,
    out_rx: &mut mpsc::UnboundedReceiver<OutEvent>,
    retired: &mpsc::UnboundedSender<StreamId>,
) -> Result<(), ConnError> {
    // Outbox closed: evicted, or the session ended.
    while let Some(ev) = out_rx.recv().await {
        write_out_event(writer, ev, attached, retired).await?;
        // One syscall per burst: the ipc.md write-coalescing contract.
        while let Ok(ev) = out_rx.try_recv() {
            write_out_event(writer, ev, attached, retired).await?;
        }
        writer.flush().await?;
    }
    Ok(())
}

/// Encode one outbox event and settle its share of the buffered-bytes
/// gauge after the write, not before: bytes in flight on a stalled
/// connection are backlog the session task must see and evict on. A
/// stream terminal is reported on `retired` for [`pump_inbound`]'s
/// driver.
async fn write_out_event<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    ev: OutEvent,
    attached: &AttachedSub,
    retired: &mpsc::UnboundedSender<StreamId>,
) -> Result<(), ConnError> {
    let len = ev.approx_wire_len();
    if let Some(stream) = terminated_stream(&ev) {
        let _reported = retired.send(stream);
    }
    let res = write_event_frame(writer, &ev).await;
    attached
        .buffered
        .fetch_sub(len, std::sync::atomic::Ordering::Relaxed);
    res
}

/// The stream an outbound event closes, if it is a terminal.
const fn terminated_stream(ev: &OutEvent) -> Option<StreamId> {
    match ev {
        OutEvent::Control(
            ConnToClientMsg::End { stream_id, .. }
            | ConnToClientMsg::Error {
                subject: Subject::Stream(stream_id),
                ..
            },
        ) => Some(*stream_id),
        _ => None,
    }
}

/// Unflushed; the pump batches a burst into one flush.
async fn write_event_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    ev: &OutEvent,
) -> Result<(), ConnError> {
    let frame = streaming::body_for(ev)?;
    // A body past the framing ceiling is this daemon's bug, not the
    // subscriber's: nothing it could send makes an event too large. Say
    // so before the connection goes, or the eviction reads as a client
    // fault in the log.
    if let Err(err) = writer.send_checked_unflushed(&frame).await {
        if matches!(err, TransportError::Frame(FrameError::BodyTooLarge { .. })) {
            tracing::error!(
                kind = frame.kind(),
                body_len = frame.body().len(),
                "outbound event exceeds the frame ceiling; evicting the subscriber"
            );
        }
        return Err(err.into());
    }
    Ok(())
}

/// Drive a notification observer (docs/reference/protocols/notifications.md).
///
/// Runs until broadcast closes, client disconnects, or stream cancel arrives.
/// Returns emitted item count for terminal `End` emission.
async fn run_notification_observer<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    driver: &mut DaemonDriver,
    stream: StreamId,
    only: Option<u128>,
    rx: &mut tokio::sync::broadcast::Receiver<NotifyToClientMsg>,
) -> Result<u32, ConnError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::sync::broadcast::error::RecvError;
    let correlation = Correlation::stream(stream);
    let mut sent = 0u32;
    loop {
        tokio::select! {
            frame = reader.next_frame() => {
                let Some(frame) = frame? else { return Ok(sent) };
                match driver.classify(&frame)? {
                    Incoming::Control(ConnToDaemonMsg::Cancel { stream_id }) if stream_id == stream => {
                        return Ok(sent);
                    }
                    // The terminal is already this call's return path.
                    Incoming::CancelIgnored { .. } => {}
                    other => {
                        return Err(ConnError::Driver(DaemonDriver::unexpected(
                            "a cancel for the notification stream",
                            format!("{other:?}"),
                        )));
                    }
                }
            }
            event = rx.recv() => {
                match event {
                    Ok(NotifyToClientMsg::Event { session_id, .. })
                        if only.is_some_and(|want| want != session_id) => {}
                    Ok(ev) => {
                        writer.send_correlated(&ev, correlation).await?;
                        sent = sent.saturating_add(1);
                    }
                    // Loss is the contract, but never silent: the typed marker lets a
                    // consumer waiting on a specific event re-check.
                    Err(RecvError::Lagged(missed)) => {
                        writer
                            .send_correlated(&NotifyToClientMsg::Lagged { missed }, correlation)
                            .await?;
                    }
                    // All hub senders dropped (daemon shutdown); the caller's terminal
                    // says so.
                    Err(RecvError::Closed) => return Ok(sent),
                }
            }
        }
    }
}

/// `failure` turns the terminal into a typed `Error`, which is how
/// `notifications --once` tells "nothing was published" from "the
/// filter never could match".
async fn end_observer<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    driver: &mut DaemonDriver,
    stream: StreamId,
    sent: u32,
    failure: Option<(StreamErrorReason, String)>,
) -> Result<(), ConnError> {
    let terminal = match failure {
        Some((reason, detail)) => driver.fail_stream(stream, reason, detail),
        None => driver.finish_stream(stream, sent),
    };
    if let Some(terminal) = terminal {
        writer.send(&terminal).await?;
    }
    Ok(())
}

/// Spawn a PTY session from `SpawnArgs`; an empty `args.command` takes
/// its program from the factory, every other field applies either way.
/// Every session carries the inherit + denylist + overrides env posture
/// (security-model.md "Process and environment boundary").
pub(crate) fn spawn_with_args(
    args: &felis_protocol::messages::SpawnArgs,
    factory: &SessionFactory,
    session_id: u128,
    base: EnvBaseSource<'_>,
    agent: Option<&crate::agent::AgentLink>,
    endpoint: Option<&OsStr>,
) -> Result<SpawnedPty, SessionError> {
    let default_program = args.command.is_empty();
    if default_program && !args.args.is_empty() {
        // Args with no program would hand the default shell an argv meant
        // for something else.
        return Err(SessionError::Spawn(
            "SpawnArgs: args without a command".to_string(),
        ));
    }
    // Both sources are validated before anything is spawned: on Windows
    // the environment block is built directly from this map, so an entry
    // that breaks the block's encoding must never reach it.
    let windows = crate::child_env::TARGET_IS_WINDOWS;
    if let Err(err) = crate::child_env::check_explicit(&args.env, windows) {
        return Err(SessionError::Spawn(err.to_string()));
    }
    let env = match resolve_env_base(base, agent, endpoint, windows) {
        Ok(env) => env,
        Err(err) => return Err(SessionError::Spawn(err.to_string())),
    };
    // Same caps as `OpsToDaemonMsg::Tag`: a creation-time label set must not
    // smuggle in what the verb would refuse.
    if let Err(err) =
        crate::pool::apply_tag_delta(&mut std::collections::BTreeSet::new(), &args.tags, &[])
    {
        return Err(SessionError::Spawn(format!("SpawnArgs: {err}")));
    }
    let Some(dir) = spawn_cwd(args) else {
        return SpawnedPty::spawn(command_from_args(args, factory, session_id, None, &env));
    };
    if !dir.is_absolute() {
        // A relative cwd would resolve against the daemon's directory, which
        // froze at auto-spawn (`/` for a macOS GUI autospawn).
        return Err(SessionError::Spawn(format!(
            "SpawnArgs: cwd must be absolute, got {:?}",
            args.cwd
        )));
    }
    match SpawnedPty::spawn(command_from_args(
        args,
        factory,
        session_id,
        Some(dir),
        &env,
    )) {
        Err(err) if default_program => {
            // Fall back to daemon cwd for bare shell launches if requested cwd
            // does not exist; named commands still fail on bad directories
            // (docs/explanation/architecture/session-lifecycle.md).
            warn!(
                cwd = %args.cwd,
                %err,
                "requested cwd unusable; starting the default shell in the daemon's cwd"
            );
            SpawnedPty::spawn(command_from_args(args, factory, session_id, None, &env))
        }
        result => result,
    }
}

/// `None` when the field is empty: the wire contract's "inherit the
/// daemon's cwd" (docs/reference/ipc.md).
fn spawn_cwd(args: &felis_protocol::messages::SpawnArgs) -> Option<&Path> {
    (!args.cwd.is_empty()).then(|| Path::new(&args.cwd))
}

/// `cwd` is passed rather than read off `args` so the bare-launch
/// fallback can rebuild the same command without one
/// (`felis_pty::Command` is not `Clone`).
fn command_from_args(
    args: &felis_protocol::messages::SpawnArgs,
    factory: &SessionFactory,
    session_id: u128,
    cwd: Option<&Path>,
    env: &ResolvedEnv,
) -> felis_pty::Command {
    let mut cmd = if args.command.is_empty() {
        // The factory gets the resolved base: the default program is the
        // base's own `$SHELL`.
        factory(env.base())
    } else {
        let mut cmd = felis_pty::Command::new(&args.command);
        if !args.args.is_empty() {
            cmd.args(&args.args);
        }
        cmd
    };
    apply_spawn_posture(
        &mut cmd,
        session_id,
        cwd,
        env,
        &args.env,
        crate::terminfo::shipped_dir(),
    );
    cmd
}

/// Apply session spawn posture: cwd, identity stamps, and caller env overrides.
///
/// Caller pairs may override unreserved identity stamps, but cannot set
/// `FELIS_SESSION_ID` or denylisted variables checked upstream.
fn apply_spawn_posture(
    cmd: &mut felis_pty::Command,
    session_id: u128,
    cwd: Option<&Path>,
    resolved: &ResolvedEnv,
    env: &[(String, String)],
    shipped_terminfo: Option<&Path>,
) {
    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }
    // First, and by replacement: `Command::new` snapshotted the daemon's
    // environment, and leaving it under a base would keep exactly the
    // stale entries the base was sent to displace.
    if let Some(base) = resolved.base() {
        cmd.env_base(base.iter().map(|(k, v)| (k.as_slice(), v.as_slice())));
    }
    crate::apply_env_policy(
        cmd,
        session_id,
        &resolved.hatch,
        resolved.endpoint.as_deref(),
        resolved.lang.as_deref(),
    );
    for (k, v) in env {
        cmd.env(k, v);
    }
    // After the caller's pairs, so a `TERMINFO_DIRS` among them still
    // resolves the `TERM` stamped above.
    if let Some(dir) = shipped_terminfo {
        crate::terminfo::list_in(cmd, dir);
    }
}

/// The roster for `OpsToDaemonMsg::List`; `Welcome` carries none.
async fn build_session_list(pool: &Arc<Mutex<SessionPool>>) -> Vec<SessionInfo> {
    let guard = pool.lock().await;
    let now = Instant::now();
    guard
        .roster_by_recency()
        .into_iter()
        .map(|(id, meta, foreground_pgrp)| session_info_from_meta(id, meta, foreground_pgrp, now))
        .collect()
}

/// Shared by the `OpsToClientMsg::Listed` walk and the `SessionToClientMsg::Attached`
/// reply, so the two cannot drift on how a field is derived.
fn session_info_from_meta(
    id: SessionId,
    meta: crate::pool::SessionMeta,
    foreground_pgrp: Option<i32>,
    now: Instant,
) -> SessionInfo {
    SessionInfo {
        id: id.0,
        dims: GridDims {
            rows: meta.rows,
            cols: meta.cols,
            pixel_w: meta.pixel_w,
            pixel_h: meta.pixel_h,
        },
        title: meta.title,
        cwd: meta.cwd,
        // Absent, not `Some(0)`: presence alone says "detached", so a
        // reader never has to tell an attached session from one detached
        // for under a second.
        idle_seconds: (meta.subscribers == 0)
            .then(|| now.saturating_duration_since(meta.idle_since).as_secs()),
        tags: meta.tags.into_iter().collect(),
        // Stamped here, beside `idle_seconds`, so the client needs no clock
        // alignment with the daemon.
        last_notification: meta.last_notification.map(|n| {
            felis_protocol::messages::SessionNotification {
                notification: Notification {
                    title: n.title,
                    body: n.body,
                    urgency: n.urgency,
                },
                age_seconds: now.saturating_duration_since(n.at).as_secs(),
            }
        }),
        foreground: foreground_pgrp.and_then(crate::foreground::comm_for_pgid),
        exited: meta.exited,
        last_exit_code: meta.last_exit_code,
        attachments: meta.attachments,
        sequence: meta.sequence,
    }
}

/// Resolve a `SessionAttach`'s target and take its handle under one
/// lock, so a session reaped between the two cannot be attached to and
/// a prefix cannot resolve against a roster the attach then misses.
async fn take_attach_handle(
    pool: &Arc<Mutex<SessionPool>>,
    target: &AttachTarget,
) -> Result<(SessionId, crate::pool::SessionHandle), (AttachFailure, String)> {
    let guard = pool.lock().await;
    let id = match target {
        AttachTarget::Id(id) => SessionId(*id),
        AttachTarget::Prefix(prefix) => {
            match felis_protocol::session_prefix::resolve_session_prefix(
                prefix,
                guard.ids().map(|id| id.0),
            ) {
                ResolvedId::Ok { id } => SessionId(id),
                ResolvedId::NoMatch => {
                    return Err((
                        AttachFailure::NoMatch,
                        format!("no session id starts with `{prefix}`"),
                    ));
                }
                ResolvedId::Ambiguous { matches } => {
                    return Err((
                        AttachFailure::Ambiguous,
                        format!("`{prefix}` matches {matches} sessions"),
                    ));
                }
            }
        }
    };
    guard.handle_cloned(id).map_or_else(
        || {
            Err((
                AttachFailure::UnknownSession,
                format!("no session with id {:#x}", id.0),
            ))
        },
        |handle| Ok((id, handle)),
    )
}

/// The daemon-side half of every `id_prefix` field
/// (`felis_protocol::session_prefix`).
async fn resolve_prefix(pool: &Arc<Mutex<SessionPool>>, prefix: &str) -> ResolvedId {
    let guard = pool.lock().await;
    felis_protocol::session_prefix::resolve_session_prefix(prefix, guard.ids().map(|id| id.0))
}

/// Exchange opening preface bytes ([`felis_transport::preface`]).
///
/// Bad magic receives nothing; unsupported majors receive a refusal naming the
/// served range; accepted peers receive the 10-byte accept before schema framing.
async fn exchange_bootstrap<R, W>(
    read_half: &mut R,
    write_half: &mut W,
) -> Result<(u16, Option<CarrierBlock>), ConnError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let opened = read_client_bootstrap(read_half).await?;
    if let Some(version) = opened.skipped_carrier_version {
        warn!(
            carrier_format_version = version,
            "ignoring a relay carrier block in an unknown format; this connection's creates fall \
             back to the daemon's own environment"
        );
    }
    let ClientPreface { major, minor } = opened.preface;
    let Some(accept) = DaemonAccept::select(opened.preface) else {
        let refusal = DaemonRefuse::CURRENT;
        write_daemon_preface(write_half, refusal).await?;
        warn!(
            client_major = major,
            "refusing an unsupported protocol major"
        );
        return Err(ConnError::MajorUnsupported {
            client: major,
            min: refusal.min_major,
            max: refusal.max_major,
        });
    };
    write_daemon_preface(write_half, accept).await?;
    // The effective minor comes from the reply the client saw, not from
    // `PROTOCOL_MINOR`: the two are one decision in `select`, and reading
    // the global here would let them drift once a second major arm exists.
    Ok((
        preface::effective_minor(minor, accept.minor),
        opened.carrier,
    ))
}

/// Where a create's environment base comes from; one chain, resolved
/// in one place.
#[derive(Debug, Clone, Copy)]
pub(crate) enum EnvBaseSource<'a> {
    /// `SpawnArgs.env_base`, sent only over a local socket. Replaces the
    /// daemon's environment outright: an overlay would keep the stale
    /// entries the snapshot was sent to displace.
    Request(&'a [crate::child_env::EnvEntry]),
    /// The relay carrier snapshot: the sshd remote command's environment
    /// on this host, the fallback for an SSH-carried create whose dialer's
    /// environment describes the wrong host.
    Relay(&'a [crate::child_env::EnvEntry]),
    /// The child inherits what the daemon was born with.
    Birth,
}

pub(crate) const fn env_base_source<'a>(
    args: &'a felis_protocol::messages::SpawnArgs,
    relay_env: Option<&'a [crate::child_env::EnvEntry]>,
) -> EnvBaseSource<'a> {
    match (args.env_base.as_ref(), relay_env) {
        // Presence, not emptiness: an empty-but-present snapshot means
        // the client asked for an empty base and gets one.
        (Some(base), _) => EnvBaseSource::Request(base.as_slice()),
        (None, Some(relay)) => EnvBaseSource::Relay(relay),
        (None, None) => EnvBaseSource::Birth,
    }
}

/// What one create's environment resolved to. The hatch and the base
/// travel together because they are read from the same environment at
/// different moments: the hatch before the denylist scrub drops
/// `FELIS_TERM` / `FELIS_TERM_PROGRAM`, the entries after.
pub(crate) struct ResolvedEnv {
    /// The sanitized base, or `None` to leave the daemon's own birth
    /// environment in place.
    base: Option<Vec<crate::child_env::EnvEntry>>,
    hatch: crate::TermHatch,
    /// Not read from the base like the hatch: a base that named one would
    /// be naming a different daemon.
    endpoint: Option<OsString>,
    /// The `LANG` to fill in, `None` whenever the resolved environment
    /// already names a locale (`crate::locale`).
    lang: Option<String>,
}

impl ResolvedEnv {
    pub(crate) fn base(&self) -> Option<&[crate::child_env::EnvEntry]> {
        self.base.as_deref()
    }

    /// A create with no snapshot: the daemon's own environment, hatch
    /// included.
    #[cfg(test)]
    pub(crate) fn birth() -> Self {
        Self {
            base: None,
            hatch: crate::TermHatch::from_daemon_env(),
            endpoint: None,
            lang: None,
        }
    }
}

/// Resolve one create's environment and hatch from a base snapshot.
///
/// # Errors
/// Returns [`crate::child_env::EnvError`] on invalid entries or cap breaches.
pub(crate) fn resolve_env_base(
    base: EnvBaseSource<'_>,
    agent: Option<&crate::agent::AgentLink>,
    endpoint: Option<&OsStr>,
    windows: bool,
) -> Result<ResolvedEnv, crate::child_env::EnvError> {
    // Before the scrub removes the two keys. The daemon's own
    // environment stays behind each one a base leaves unset.
    let hatch = match base {
        EnvBaseSource::Request(entries) | EnvBaseSource::Relay(entries) => {
            crate::TermHatch::from_base(entries, windows)
        }
        EnvBaseSource::Birth => crate::TermHatch::default(),
    }
    .or(crate::TermHatch::from_daemon_env());
    // Read off the same entries as the hatch: the guard must see the
    // snapshot the client sent, not the daemon's own environment, which a
    // base replaces outright.
    let lang = match base {
        EnvBaseSource::Request(entries) | EnvBaseSource::Relay(entries) => {
            crate::locale::fill_lang(Some(entries), windows)
        }
        EnvBaseSource::Birth => crate::locale::fill_lang(None, windows),
    };
    let mut resolved = match base {
        EnvBaseSource::Request(entries) | EnvBaseSource::Relay(entries) => {
            Some(crate::child_env::sanitize_base(entries, windows)?)
        }
        EnvBaseSource::Birth => None,
    };
    // Only a relay-chain base is rewritten: a local dial's
    // `SSH_AUTH_SOCK` already names something as durable as the daemon.
    if let (true, Some(entries), Some(link)) = (
        matches!(base, EnvBaseSource::Relay(_)),
        resolved.as_mut(),
        agent,
    ) {
        rewrite_agent_socket(entries, link, windows);
    }
    Ok(ResolvedEnv {
        base: resolved,
        hatch,
        endpoint: endpoint.map(OsStr::to_owned),
        lang,
    })
}

/// This connection's forwarded agent socket, if its carrier block
/// named one. Derived from the sanitized block, so a block the create
/// would refuse cannot steer the link first and be refused afterwards.
fn forwarded_agent_socket(block: &CarrierBlock) -> Option<PathBuf> {
    let windows = crate::child_env::TARGET_IS_WINDOWS;
    let entries = crate::child_env::sanitize_base(&block.env, windows).ok()?;
    crate::child_env::lookup(&entries, crate::agent::AGENT_ENV, windows)
        .and_then(felis_pty::env_from_bytes)
        .map(PathBuf::from)
}

/// `SSH_AUTH_SOCK` in the platform's own name representation, so it can
/// be compared against a snapshot entry without decoding that entry
/// into a `String` it may not be.
fn agent_env_name_bytes() -> Vec<u8> {
    felis_pty::env_bytes(OsStr::new(crate::agent::AGENT_ENV))
}

/// Point a relay-chain base's `SSH_AUTH_SOCK` at the daemon's stable
/// link. Only an entry already there is rewritten: a relay that
/// forwarded no agent describes a session that never had one.
fn rewrite_agent_socket(
    entries: &mut [crate::child_env::EnvEntry],
    link: &crate::agent::AgentLink,
    windows: bool,
) {
    let wanted = crate::child_env::canonical_name_bytes(&agent_env_name_bytes(), windows);
    let stable = felis_pty::env_bytes(link.path().as_os_str());
    for (name, value) in entries.iter_mut() {
        if crate::child_env::canonical_name_bytes(name, windows) == wanted {
            value.clone_from(&stable);
        }
    }
}

async fn write_conn<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    msg: &ConnToClientMsg,
) -> Result<(), ConnError> {
    writer.send(msg).await?;
    Ok(())
}

/// The mode a `Hello` named that postdates this build, or `None` for
/// every other failure, which stays corruption. Read off the frame a
/// second time because the driver renders each decode failure as one
/// string: a newer peer's mode is the one handshake failure that is not
/// corruption (`docs/reference/ipc.md` "Handshake").
fn unreadable_mode(frame: &OwnedFrame) -> Option<i32> {
    if frame.kind != MessageKind::Conn as u16 {
        return None;
    }
    // `Conn` carries no envelope, and the decode below cannot see
    // field 100: without this the frame the driver refused for its
    // envelope would be answered as a newer peer's.
    if !matches!(
        felis_protocol::codec::peek_correlation(frame.body.clone()),
        Ok(None)
    ) {
        return None;
    }
    // Nor can it see a client-bound arm beside the `Hello`, which the
    // driver refused as a wrong-direction arm.
    if !matches!(
        felis_protocol::codec::arm_in(MessageKind::Conn, Direction::ToClient, &frame.body),
        Ok(None)
    ) {
        return None;
    }
    match felis_protocol::codec::decode::<ConnToDaemonMsg>(frame.body.clone()) {
        // `UNSPECIFIED` is not a mode a newer build could name, so a
        // `Hello` carrying it is malformed rather than ahead.
        Err(CodecError::Wire(WireError::UnknownEnum {
            field: "ConnectionMode",
            value,
        })) if value != 0 => Some(value),
        _ => None,
    }
}

/// Name the skew, then close.
async fn refuse_unknown_mode<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    value: i32,
    deadline: Duration,
) -> Result<(), ConnError> {
    with_deadline(
        deadline,
        HandshakePhase::Hello,
        write_conn(
            writer,
            &ConnToClientMsg::Refused {
                reason: RefusalReason::UnknownMode,
                detail: format!("connection mode {value} arrived after this daemon's build"),
            },
        ),
    )
    .await
}

/// Run the application handshake. `Hello` states only what the daemon
/// cannot infer (mode, pacing); version selection happened in the
/// preface. The frame goes through the driver so a non-`Conn` first
/// frame is refused by phase with the same error every later refusal
/// carries.
async fn handshake<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    driver: &mut DaemonDriver,
    effective_minor: u16,
    deadline: Duration,
) -> Result<ConnParams, ConnError> {
    let first = with_deadline(deadline, HandshakePhase::Hello, reader.next_frame())
        .await?
        .ok_or(ConnError::EofBeforeHello)?;
    let classified = match driver.classify(&first) {
        Ok(classified) => classified,
        Err(err) => {
            let Some(value) = unreadable_mode(&first) else {
                return Err(err.into());
            };
            refuse_unknown_mode(writer, value, deadline).await?;
            return Err(ConnError::UnknownMode { value });
        }
    };
    let Incoming::Control(ConnToDaemonMsg::Hello { mode, pull_paced }) = classified else {
        return Err(ConnError::NotHello);
    };
    info!(?mode, effective_minor, "handshake established");

    let welcome = ConnToClientMsg::Welcome {
        // Informational; the preface already settled compatibility.
        identity: Some(crate::build_identity()),
    };
    write_conn(writer, &welcome).await?;
    // The mode gates every later frame at the kind level, so the driver
    // learns it before anything but `Hello` has been read.
    driver.handshake_done(mode);
    Ok(ConnParams {
        mode,
        pull_paced,
        effective_minor,
    })
}

#[cfg(test)]
mod tests;

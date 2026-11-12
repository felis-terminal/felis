//! Per-session owner task owning the PTY-to-grid pipeline and subscriber fan-out.
//!
//! Slow subscribers past `SUBSCRIBER_BUFFER_CAP` are evicted rather than
//! backpressuring the shared PTY (session-lifecycle.md "Same-user mirroring").

use std::collections::{BTreeMap, VecDeque};
use std::sync::{
    Arc, Mutex as StdMutex, Weak,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

use felis_protocol::messages::{
    AttentionSource, InfoOutcome, KeyEvent, MAX_REGION_REPLY_BYTES, MouseEvent, MouseProtocol,
    PromptKind, RegionPosition, RegionSource, RequestedDims, SendOutcome, SpawnOutcome,
    ThemeChannel,
};
use felis_protocol::{
    ConnectionMode, MinorGated as _,
    messages::{
        AttachCarrier, Attachment, ConnToClientMsg, Correlation, ExitStatus, GridDims, GridMsg,
        ImageMsg, InputMsg, NotifyToClientMsg, OpsToClientMsg, PushMsg, RegionToClientMsg,
        RequestId, SearchToClientMsg, SearchToDaemonMsg, SessionField, SessionInfo,
        SessionToClientMsg, StreamErrorReason, StreamId, Subject, SwitchDenied, SwitchScope,
    },
};
use felis_pty::{ChildHandle, Size as PtySize};
use tokio::sync::{Mutex, Notify, mpsc, oneshot, watch};
use tracing::{debug, error, info, trace, warn};

use crate::{
    SessionId, SessionPool,
    pool::InputReservation,
    pool::{AttachmentIds, Listing, ParseCore, Session, SessionHandle, SessionMeta, SessionSlot},
    serve::{ConnError, IdlePolicy, key_encode, streaming, write_paste, write_pty},
};

mod idle;

/// 2× the per-session image-store cap, so a full image rehydrate burst
/// (the largest single push) can never trip it on a healthy
/// connection.
pub(crate) const SUBSCRIBER_BUFFER_CAP: usize = crate::pool::DEFAULT_IMAGE_BYTE_CAP * 2;

/// Sampled off live state when [`SessionCmd::Stats`] lands, not
/// tracked by the hot paths.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    /// Both screens' stores.
    pub image_bytes: usize,
    /// Read off the store, not the constant it was built from, so the
    /// reported limit is the one an insert is refused against, once
    /// per screen.
    pub image_bytes_cap: usize,
    /// `None` between transmissions.
    pub decode_bytes_in_flight: Option<usize>,
    /// The deepest subscriber outbox, in bytes queued but not written.
    pub max_subscriber_backlog: usize,
    /// Every subscriber outbox of this session summed: the deepest one
    /// alone cannot say how much the daemon is holding.
    pub total_subscriber_backlog: usize,
    /// Client input admitted against this session's budget and not yet
    /// written to the child.
    pub pty_input_bytes: usize,
}

/// Framing-plus-envelope allowance per event; also the smallest length
/// any push reports against the backlog gauge.
const WIRE_LEN_OVERHEAD: usize = 64;

/// `GridMsg::CommandOutput` frames one subscriber holds between its
/// diff cycles; past it the oldest drops, so a window that stops
/// pulling holds no more.
const ANNOUNCE_QUEUE_CAP: usize = 8;

/// Small on purpose: the channel backpressures a connection that
/// outruns the task, the same way the PTY would.
const CMD_CHANNEL_CAPACITY: usize = 64;

/// Past this many held commands the task stops taking commands until
/// the decode commits, so a window resizing for the whole of a long
/// decode backs up into the command channel instead of the task's memory.
const HELD_CMDS_CAP: usize = 1024;

/// The connection pump owns encoding, framing, and the correlation
/// envelope.
#[derive(Debug, Clone)]
pub enum OutEvent {
    Grid(GridMsg),
    Image(ImageMsg),
    /// `Reattach` / `SessionExited` / `RetargetHost` reach window
    /// subscribers only; `Evicted` reaches every one.
    Push(PushMsg),
    Region {
        msg: RegionToClientMsg,
        correlation: Correlation,
    },
    Search {
        msg: SearchToClientMsg,
        correlation: Correlation,
    },
    /// A `SessionToClientMsg::InputAccepted` answering this subscriber's fence.
    Session {
        msg: SessionToClientMsg,
        correlation: Correlation,
    },
    /// An `Ops` reply answered on an attached connection. It rides the
    /// outbox because that is the connection's one writer: written
    /// past it, a reply could overtake the grid frames before it.
    Ops {
        msg: Box<OpsToClientMsg>,
        correlation: Correlation,
    },
    /// A stream's one terminal, or a typed refusal. The connection
    /// pump routes it through its driver, which retires the id and
    /// enforces "exactly one".
    Control(ConnToClientMsg),
}

impl OutEvent {
    /// Used symmetrically by the task (gauge increment at push) and
    /// the pump (decrement after write).
    #[must_use]
    pub fn approx_wire_len(&self) -> usize {
        match self {
            Self::Grid(GridMsg::RowDelta { rows }) => {
                rows.iter().map(|(_, b)| b.0.len()).sum::<usize>() + WIRE_LEN_OVERHEAD
            }
            Self::Grid(GridMsg::CommandOutput { text, .. }) => text.len() + WIRE_LEN_OVERHEAD,
            Self::Image(ImageMsg::Chunk { bytes, .. }) => bytes.len() + WIRE_LEN_OVERHEAD,
            Self::Region {
                msg: RegionToClientMsg::Reply { data, .. },
                ..
            } => data.len() + WIRE_LEN_OVERHEAD,
            Self::Region {
                msg: RegionToClientMsg::Row { text, ansi, .. },
                ..
            } => text.len() + ansi.as_ref().map_or(0, String::len) + WIRE_LEN_OVERHEAD,
            Self::Ops { msg, .. } => match msg.as_ref() {
                OpsToClientMsg::Listed { sessions } => {
                    sessions.iter().map(session_info_wire_len).sum::<usize>() + WIRE_LEN_OVERHEAD
                }
                OpsToClientMsg::InfoReply {
                    outcome: InfoOutcome::Found { session, .. },
                } => session_info_wire_len(session) + WIRE_LEN_OVERHEAD,
                OpsToClientMsg::Spawned {
                    outcome: SpawnOutcome::Ok { info },
                } => session_info_wire_len(info) + WIRE_LEN_OVERHEAD,
                _ => WIRE_LEN_OVERHEAD,
            },
            _ => WIRE_LEN_OVERHEAD,
        }
    }
}

/// A roster row's variable-length bytes plus a per-entry allowance for
/// its tags and fixed fields: a peer that keeps asking for the roster
/// while reading slowly is charged for what it queues.
fn session_info_wire_len(info: &SessionInfo) -> usize {
    let opt = |s: &Option<String>| s.as_ref().map_or(0, String::len);
    let notification = info.last_notification.as_ref().map_or(0, |n| {
        opt(&n.notification.title) + n.notification.body.len()
    });
    let tags: usize = info.tags.iter().map(|t| t.len() + 3).sum();
    let records: usize = info
        .program_status
        .iter()
        .map(|r| {
            r.id.len()
                + r.state.len()
                + opt(&r.kind)
                + opt(&r.app)
                + opt(&r.title)
                + opt(&r.msg)
                + WIRE_LEN_OVERHEAD
        })
        .sum();
    opt(&info.title)
        + opt(&info.cwd)
        + opt(&info.spawn_cwd)
        + opt(&info.foreground)
        + notification
        + tags
        + records
        + info.attachments.len() * WIRE_LEN_OVERHEAD
        + WIRE_LEN_OVERHEAD
}

/// Keep the youngest `cap` bytes of an oversize region reply at a resumable boundary.
///
/// Unchunked replies exceeding the framing ceiling ship their tail rather than dropping
/// the connection. Viewport position is dropped because head line indices are invalid.
fn trim_region_reply(
    data: Vec<u8>,
    position: Option<RegionPosition>,
    cap: usize,
) -> (Vec<u8>, Option<RegionPosition>) {
    if data.len() <= cap {
        return (data, position);
    }
    let tail = data.len() - cap;
    let cut = resumable_cut(&data, tail);
    warn!(
        region_bytes = data.len(),
        cap, "region reply trimmed to its youngest lines"
    );
    (data[cut..].to_vec(), None)
}

/// Find the first offset at or after `from` where a reader can safely resume.
///
/// Walks from the buffer head because local context at `from` cannot tell if an
/// escape sequence is unterminated, avoiding split UTF-8 scalars or partial escapes.
fn resumable_cut(data: &[u8], from: usize) -> usize {
    let mut first_safe = None;
    let mut i = 0;
    while i < data.len() {
        if i >= from {
            if first_safe.is_none() {
                first_safe = Some(i);
            }
            if i > 0 && data[i - 1] == b'\n' {
                return i;
            }
        }
        i += token_len(data, i);
    }
    first_safe.unwrap_or(data.len())
}

/// Length of the escape sequence or UTF-8 scalar starting at `i`, never
/// zero. Malformed input costs one byte, which keeps the walk moving.
fn token_len(data: &[u8], i: usize) -> usize {
    match data[i] {
        0x1b => escape_len(data, i),
        b if b < 0x80 => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
    .min(data.len() - i)
}

fn escape_len(data: &[u8], i: usize) -> usize {
    let rest = data.len() - i;
    let Some(&intro) = data.get(i + 1) else {
        return rest;
    };
    match intro {
        // CSI: parameter and intermediate bytes up to a final byte.
        b'[' => (i + 2..data.len())
            .find(|&j| (0x40..=0x7e).contains(&data[j]))
            .map_or(rest, |j| j + 1 - i),
        // String-carrying introducers (OSC, DCS, SOS, PM, APC), ended
        // by BEL or ST. A bare `ESC` inside one is treated as ending
        // it, the same recovery a terminal's parser makes.
        b']' | b'P' | b'X' | b'^' | b'_' => {
            let mut j = i + 2;
            while j < data.len() {
                match data[j] {
                    0x07 => return j + 1 - i,
                    0x1b if data.get(j + 1) == Some(&b'\\') => return j + 2 - i,
                    0x1b => return j - i,
                    _ => j += 1,
                }
            }
            rest
        }
        _ => 2,
    }
}

/// Daemon-global, allocated from [`AttachmentIds`], never reused, and
/// the same number the roster publishes as `Attachment::id`.
/// Daemon-global rather than per-session so an id read off the roster
/// cannot mean a different window on a different session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriberId(u64);

impl SubscriberId {
    /// The attachment id as the wire spells it (`Attachment.id`).
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[cfg(test)]
    pub(crate) const fn for_test(raw: u64) -> Self {
        Self(raw)
    }
}

/// Daemon-global id of an unattached connection that addresses this
/// session by id, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequesterId(u64);

impl RequesterId {
    #[must_use]
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Whose outbox a stream writes to: an attached subscriber, or an
/// unattached connection that named the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Sub(SubscriberId),
    Conn(RequesterId),
}

/// An unattached connection's outbox, as [`SessionCmd::AddRequester`]
/// registers it with each session it addresses.
pub struct RequesterHandle {
    pub id: RequesterId,
    pub tx: mpsc::UnboundedSender<OutEvent>,
    /// Bytes queued and not yet written, settled by the connection's
    /// writer as for a subscriber.
    pub buffered: Arc<AtomicUsize>,
    /// Notified when the outbox passes [`SUBSCRIBER_BUFFER_CAP`]: the
    /// connection holds the outbox itself, so dropping it here would
    /// not end the connection.
    pub evict: Arc<Notify>,
}

/// What [`SessionCmd::PrepareSend`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prepared {
    /// Nothing to refuse; a requested watch is registered.
    Ready,
    /// The program has exited; no watch was registered.
    Ended,
    /// The mouse step at `step` would not reach the program.
    Unreported { step: u32, detail: String },
}

/// Close a pull-paced cycle after everything `compose_diffs` produced.
fn push_cycle_end(out: &mut Vec<OutEvent>, pull_pacing: bool) {
    if pull_pacing {
        out.push(OutEvent::Grid(GridMsg::CycleEnd));
    }
}

pub struct SubscribeReq {
    /// Only a [`ConnectionMode::Window`] subscriber is pushed to or
    /// flashed: an `Ops` attach has no window to move or alert.
    pub mode: ConnectionMode,
    /// `Hello.pull_paced`: the grid stream ships against per-vsync
    /// `NextGridFrame` pulls.
    pub pull_paced: bool,
    /// The connection's effective minor: no arm a later minor added
    /// may be queued for it.
    pub minor: u16,
    pub tx: mpsc::UnboundedSender<OutEvent>,
    /// Bytes pushed but not yet written; the connection pump decrements
    /// as it writes (see `SUBSCRIBER_BUFFER_CAP`).
    pub buffered: Arc<AtomicUsize>,
    pub if_exited: IfExited,
    /// `SessionToDaemonMsg::Attach { image_regions }`: the window reads
    /// frame-region transfers.
    pub image_regions: bool,
    pub carrier: AttachCarrier,
    /// Answered once the rehydrate burst is queued.
    pub reply: oneshot::Sender<Result<SubscribeOk, SubscribeRefused>>,
}

/// What a subscription does on a session whose shell has already exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfExited {
    /// A deliberate attach lands, to read the final screen.
    Land,
    /// `SessionToDaemonMsg::Attach { live_only }`.
    Refuse,
    /// The create's own attach. It is no viewer of a corpse: a shell that
    /// exits before it lands still owes it the `SessionExited` and close
    /// it would have heard had it landed first.
    Notify,
}

/// A shell's exit, with the status its reap found, if any.
#[derive(Debug, Clone, Copy)]
struct ShellExit(Option<ExitStatus>);

/// An enum rather than a dropped reply channel: the caller already
/// reads a dropped channel as "the task is unwinding", and the two
/// want different wire reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeRefused {
    Exited,
}

#[derive(Debug, Clone)]
pub struct SubscribeOk {
    pub sub: SubscriberId,
    /// The `SessionToClientMsg::Attached` roster row, with the subscribe-time grid
    /// geometry stamped into `dims`.
    pub info: SessionInfo,
}

pub enum SessionCmd {
    Subscribe(SubscribeReq),
    /// The creator made a [`Listing::Held`] row nameable: the observer
    /// items held until now go out behind `SessionAdded`.
    Published,
    /// Unknown ids are ignored: a post-eviction race is harmless.
    Unsubscribe {
        sub: SubscriberId,
    },
    Input {
        sub: SubscriberId,
        msg: InputMsg,
        /// The connection's admission against the session's input
        /// budget, released only once the PTY writer has handed the
        /// bytes to the OS (`docs/reference/ipc.md` "Backpressure").
        /// `None` for the kinds that reach no PTY.
        reservation: Option<InputReservation>,
    },
    Search {
        owner: Owner,
        msg: SearchToDaemonMsg,
        /// Client-allocated; stamped on every item and the terminal.
        stream: StreamId,
    },
    /// A `Conn::Cancel` the connection's driver already validated.
    CancelStream {
        /// A connection cannot cancel another's stream.
        owner: Owner,
        stream: StreamId,
    },
    /// The client's configured colors, so an `OSC 4/10/11/12 ; ?` query
    /// answers with the real surface color
    /// (`SessionToDaemonMsg::ConfigureTheme`).
    ConfigureTheme {
        sub: SubscriberId,
        fg: Option<(u8, u8, u8)>,
        bg: Option<(u8, u8, u8)>,
        cursor: Option<(u8, u8, u8)>,
        palette: BTreeMap<u8, (u8, u8, u8)>,
    },
    /// `SessionToDaemonMsg::InputFence`: the barrier answered once every
    /// `Input` command queued ahead of it on this channel has run.
    InputFence {
        sub: SubscriberId,
        request: RequestId,
    },
    /// `RegionToDaemonMsg::Request` (docs/explanation/data-model/scrollback.md
    /// "Piping to an external command").
    Region {
        owner: Owner,
        source: RegionSource,
        ansi: bool,
        request: RequestId,
    },
    /// `RegionToDaemonMsg::Rows`: the same region row by row
    /// (`Row` … `RowsDone`).
    RegionRows {
        owner: Owner,
        source: RegionSource,
        ansi: bool,
        /// Only the youngest `max_rows` rows (`--lines`).
        max_rows: Option<u32>,
        stream: StreamId,
    },
    /// Force-detach (docs/explanation/architecture/control-surfaces.md);
    /// the session keeps running parked.
    EvictAll {
        /// Answered after the eviction completed, not when asked.
        reply: oneshot::Sender<usize>,
    },
    /// `felis daemon status`; answered from the actor because the
    /// numbers live in state only it owns.
    Stats {
        reply: oneshot::Sender<SessionStats>,
    },
    /// Relay one push to window subscribers in `scope`.
    ///
    /// Resolves inside the actor rather than at pool level so a window cannot
    /// detach between resolve and enqueue.
    PushToTarget {
        msg: PushMsg,
        scope: SwitchScope,
        reply: oneshot::Sender<PushOutcome>,
    },
    Shutdown,
    /// Before an unattached connection's first stream on this session.
    AddRequester(RequesterHandle),
    /// The connection ended: its streams end with it, unterminated.
    DropRequester {
        id: RequesterId,
    },
    /// Before an `Ops::Send` writes anything: refuse it on an exited
    /// session or a mouse step the program would not see, and register
    /// the `SendAndWait` watch in the same step.
    PrepareSend {
        watch: Option<(RequesterId, StreamId)>,
        /// Each mouse step with its index among the request's steps.
        mouse: Vec<(u32, MouseEvent)>,
        reply: oneshot::Sender<Prepared>,
    },
    /// One `Ops::Send` step, admitted against the input budget by the
    /// connection that carries it. No subscriber: it moves neither size
    /// ownership nor the input owner.
    RemoteInput {
        msg: InputMsg,
        reservation: Option<InputReservation>,
    },
    /// Queued behind an `Ops::Send`'s last step: answered `false` when
    /// the program has exited. Releases a `SendAndWait` watch, whose
    /// `WaitSent` item it queues itself, ahead of any held mark.
    SendFence {
        watch: Option<(RequesterId, StreamId)>,
        /// The session's full id, for `WaitSent`.
        id: u128,
        reply: oneshot::Sender<bool>,
    },
    /// Answered once every command queued ahead of it has run, so input
    /// admitted before an upgrade's barrier reaches the writer before it
    /// parks. Until [`SessionCmd::Resume`] the task drains only for a parse
    /// thread waiting on one: a later drain would write replies the parked
    /// writer never reports and take effects out of the dumped grid.
    Settle(oneshot::Sender<()>),
    /// The session's state for an upgrade dump, read while its PTY
    /// threads are parked.
    Capture(oneshot::Sender<Box<crate::upgrade::dump::SessionState>>),
    /// Ends the hold [`SessionCmd::Settle`] began.
    Resume,
}

/// Two shapes, not a count with a sentinel: an `Accepted(0)` is a real
/// answer for a window that departed on the push instead of taking it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// How many outboxes took the frame.
    Accepted(u32),
    Denied(SwitchDenied),
}

enum EndReason {
    Destroyed,
    PoolDropped,
    PtyError,
    Reaped,
}

/// The id is minted by the caller before the PTY spawn so
/// `apply_env_policy` can stamp `FELIS_SESSION_ID` into the child.
/// `dims` is admitted (REQ-605a) and is what `spawned` was sized to.
/// `slot` is released as the handle registers, under one pool lock.
pub async fn spawn_session(
    pool: &Arc<Mutex<SessionPool>>,
    spawned: crate::SpawnedPty,
    spawn_cwd: Option<String>,
    policy: IdlePolicy,
    id: SessionId,
    dims: GridDims,
    tags: Vec<String>,
    slot: Option<SessionSlot>,
    listing: Listing,
) -> SessionLifecycle {
    let mut owned = Session::from_spawned(spawned);
    owned.spawn_cwd = spawn_cwd;
    spawn_owned(
        pool,
        owned,
        policy,
        id,
        dims.pixel_w,
        dims.pixel_h,
        tags,
        slot,
        listing,
    )
    .await
}

/// What a spawn hands its caller. `done` is the only way to *await* a
/// session's teardown: the owner task's `JoinHandle` belongs to the
/// detached supervisor, and [`SessionHandle`] carries no completion
/// signal.
pub struct SessionLifecycle {
    pub id: SessionId,
    pub info: SessionInfo,
    /// The command channel the registration published, so a caller
    /// needs no second pool lookup a concurrent destroy could lose.
    pub cmd: mpsc::Sender<SessionCmd>,
    pub input_budget: Arc<tokio::sync::Semaphore>,
    /// Flips to `true` once the owner task has exited *and* the child
    /// is reaped.
    pub done: watch::Receiver<bool>,
    pub shutdown: Arc<crate::pool::ShutdownSignal>,
}

/// Spawn a session owner task.
///
/// `tags` seeds metadata at registration so it is never listable untagged;
/// `listing` controls whether lookups answer immediately or hold until id delivery.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_owned(
    pool: &Arc<Mutex<SessionPool>>,
    session: Session,
    policy: IdlePolicy,
    id: SessionId,
    pixel_w: u16,
    pixel_h: u16,
    tags: Vec<String>,
    slot: Option<SessionSlot>,
    listing: Listing,
) -> SessionLifecycle {
    spawn_task(
        pool, session, policy, id, pixel_w, pixel_h, tags, slot, listing, None,
    )
    .await
}

/// What a session carried across an in-place upgrade keeps of its
/// registration, so the successor lists it where its predecessor did.
pub struct Restored {
    pub sequence: std::num::NonZeroU64,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub exited: bool,
    /// What the program was last told, so the first reconcile reports
    /// only what the departed windows changed.
    pub reported_focus: bool,
    pub reported_os_dark: bool,
    pub reported_resize: Option<ReportedResize>,
    pub idle_for: Duration,
    pub last_notification: Option<crate::pool::StoredNotification>,
}

/// The part of [`Restored`] the task itself holds.
struct Carried {
    reported_focus: bool,
    reported_os_dark: bool,
    reported_resize: Option<ReportedResize>,
    idle_since: Instant,
    last_notification: Option<crate::pool::StoredNotification>,
}

/// Registers a session an in-place upgrade carried over, under its own
/// id and sequence, listed at once: whoever created it is gone.
pub async fn spawn_restored(
    pool: &Arc<Mutex<SessionPool>>,
    session: Session,
    policy: IdlePolicy,
    id: SessionId,
    pixel_w: u16,
    pixel_h: u16,
    tags: Vec<String>,
    restored: Restored,
) -> SessionLifecycle {
    spawn_task(
        pool,
        session,
        policy,
        id,
        pixel_w,
        pixel_h,
        tags,
        None,
        Listing::Public,
        Some(restored),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn spawn_task(
    pool: &Arc<Mutex<SessionPool>>,
    mut session: Session,
    policy: IdlePolicy,
    id: SessionId,
    pixel_w: u16,
    pixel_h: u16,
    tags: Vec<String>,
    slot: Option<SessionSlot>,
    listing: Listing,
    restored: Option<Restored>,
) -> SessionLifecycle {
    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
    // One lock hold, so the epochs describe exactly the strings seeded
    // below.
    let (rows, cols, title, cwd, title_epoch, cwd_epoch) = {
        let core = session.lock_core();
        (
            core.grid.rows(),
            core.grid.cols(),
            core.grid.title().map(str::to_owned),
            core.grid.cwd().map(str::to_owned),
            core.grid.title_epoch(),
            core.grid.cwd_epoch(),
        )
    };
    let pty_pixels = {
        let mut core = session.lock_core();
        // Only a restore that carried no screen has a grid without the
        // winsize `SpawnedPty::spawn_sized` recorded.
        if core.grid.winsize_pixels().0 == 0 || core.grid.winsize_pixels().1 == 0 {
            let program = crate::program_size(
                PtySize {
                    rows,
                    cols,
                    pixel_width: pixel_w,
                    pixel_height: pixel_h,
                },
                crate::NOMINAL_CELL_PIXELS,
            );
            if let Err(err) = session.resizer.resize(program) {
                warn!(?err, "restore: PTY resize failed");
            }
            core.grid
                .set_winsize_pixels((program.pixel_width, program.pixel_height));
        }
        core.grid.winsize_pixels()
    };
    // The same derivation `apply_size` runs.
    session.cell_pixel_w = pty_pixels.0.checked_div(cols).unwrap_or(0);
    session.cell_pixel_h = pty_pixels.1.checked_div(rows).unwrap_or(0);
    let input_budget = crate::pool::new_input_budget();
    // Minted under the registration's lock, before the meta is
    // published: a session must never be listable without the sequence
    // that places it in the ring.
    let mut guard = pool.lock().await;
    let now = Instant::now();
    let (sequence, title, cwd, exited, carried) = match restored {
        Some(restored) => (
            restored.sequence,
            title.or(restored.title),
            cwd.or(restored.cwd),
            restored.exited,
            Some(Carried {
                reported_focus: restored.reported_focus,
                reported_os_dark: restored.reported_os_dark,
                reported_resize: restored.reported_resize,
                idle_since: now.checked_sub(restored.idle_for).unwrap_or(now),
                last_notification: restored.last_notification,
            }),
        ),
        None => (guard.next_sequence(), title, cwd, false, None),
    };
    let meta = Arc::new(StdMutex::new(SessionMeta {
        rows,
        cols,
        pixel_w,
        pixel_h,
        title,
        cwd,
        idle_since: carried.as_ref().map_or(now, |carried| carried.idle_since),
        subscribers: 0,
        tags: tags.into_iter().collect(),
        last_notification: carried
            .as_ref()
            .and_then(|carried| carried.last_notification.clone()),
        exited,
        // A restored grid may already carry a D mark.
        last_exit_code: session.lock_core().grid.last_command_exit(),
        attachments: Vec::new(),
        program_status: session.program_status.snapshot(),
        spawn_cwd: session.spawn_cwd.clone(),
        terminal_progress: session.terminal_progress,
        sequence,
    }));
    let meta_status_generation = session.program_status.generation();
    let publication = crate::pool::Publication::new();
    let shutdown = Arc::new(crate::pool::ShutdownSignal::default());
    let (hub, attachment_ids, events_hub) = {
        guard.register(
            id,
            SessionHandle {
                cmd: cmd_tx.clone(),
                input_budget: Arc::clone(&input_budget),
                meta: Arc::clone(&meta),
                resizer: Some(Arc::clone(&session.resizer)),
                upgrade: Some(crate::pool::UpgradeParts {
                    quiescer: session.quiescer.clone(),
                    child: Arc::clone(&session.child),
                }),
                publication: Arc::clone(&publication),
                shutdown: Arc::clone(&shutdown),
            },
            slot,
            listing,
        );
        let handles = (
            guard.notify_hub(),
            guard.attachment_ids(),
            guard.events_hub(),
        );
        drop(guard);
        handles
    };
    // From the meta just seeded, not a second handle lookup: a
    // concurrent destroy or an immediately exiting child can remove
    // the pool entry first, and the client would wait forever.
    let info = super::session_info_from_meta(
        id,
        meta.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        session.resizer.foreground_pgrp(),
        Instant::now(),
    );
    let mut task = SessionTask {
        id,
        session,
        input_budget: Arc::clone(&input_budget),
        subs: Vec::new(),
        requesters: Vec::new(),
        attachment_ids,
        meta,
        pool: Arc::downgrade(pool),
        hub,
        observers: ObserverOutbox::new(events_hub, id, Arc::clone(&publication)),
        policy,
        active_sub: None,
        input_owner: None,
        attachments_dirty: false,
        pty_eof: false,
        told_exit: None,
        child_exited: exited,
        reaped_exit: None,
        row_cache: streaming::RowEncodeCache::default(),
        reported_focus: false,
        focus_dirty: false,
        reported_os_dark: false,
        scheme_dirty: false,
        resize_notify_dirty: false,
        reported_resize: None,
        #[cfg(all(test, unix))]
        pty_steps: Vec::new(),
        #[cfg(all(test, unix))]
        pty_resized_over: Vec::new(),
        meta_title_epoch: title_epoch,
        meta_cwd_epoch: cwd_epoch,
        meta_status_generation,
        pending_chunks: VecDeque::new(),
        upgrade_hold: false,
        decode: None,
        held_cmds: VecDeque::new(),
        awaiting_post_release_drain: false,
        shutdown: Arc::clone(&shutdown),
        idle: idle::IdleCompaction::new(idle::QUIET),
        #[cfg(all(test, unix))]
        before_decode: None,
    };
    if let Some(carried) = carried {
        task.reported_focus = carried.reported_focus;
        task.reported_os_dark = carried.reported_os_dark;
        task.reported_resize = carried.reported_resize;
        // The windows went with the predecessor; the program hears what
        // their detach would have told it.
        task.focus_dirty = true;
        task.resize_notify_dirty = true;
        task.recompute_presentation();
        // The predecessor held its drain from the settle on, so the core
        // may carry replies the program is still waiting for.
        task.session.signals.mark_dirty();
    }
    task.observers.publish();
    let child = Arc::clone(&task.session.child);
    let handle = tokio::spawn(run_session(task, cmd_rx));
    let (done_tx, done) = watch::channel(false);
    drop(tokio::spawn(supervise(
        handle,
        Arc::downgrade(pool),
        id,
        child,
        publication,
        done_tx,
    )));
    SessionLifecycle {
        id,
        info,
        cmd: cmd_tx,
        input_budget,
        done,
        shutdown,
    }
}

/// Supervise the session task across panics.
///
/// Unwinds skip normal tail cleanup and `Drop` cannot await `SIGKILL`,
/// so the watcher cleans up the pool entry and reaps the child.
async fn supervise(
    handle: tokio::task::JoinHandle<()>,
    pool: Weak<Mutex<SessionPool>>,
    id: SessionId,
    child: Arc<ChildHandle>,
    publication: Arc<crate::pool::Publication>,
    done: watch::Sender<bool>,
) {
    let outcome = handle.await;
    if let Err(err) = outcome {
        report_dead_task(err, &pool, id, &child, &publication).await;
    }
    // Last on both paths: a rollback waiting on `done` must not observe
    // completion before `run_session`'s tail (or the recovery above)
    // has reaped the child.
    let _observed = done.send(true);
}

/// The task did not return: `run_session`'s own cleanup never ran.
async fn report_dead_task(
    err: tokio::task::JoinError,
    pool: &Weak<Mutex<SessionPool>>,
    id: SessionId,
    child: &Arc<ChildHandle>,
    publication: &crate::pool::Publication,
) {
    let reason = match err.try_into_panic() {
        Ok(payload) => payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_owned()),
        Err(err) => err.to_string(),
    };
    error!(?id, %reason, "session task died without cleanup; removing from pool");
    let teardown = if let Some(pool) = pool.upgrade() {
        let mut guard = pool.lock().await;
        let teardown = guard.begin_teardown();
        guard.remove(id);
        // The task's own lifecycle items died with it.
        publication.end(&guard.events_hub(), id);
        Some(teardown)
    } else {
        None
    };
    end_child(child).await;
    drop(teardown);
}

// Independent per-subscriber facts, not an encodable state machine:
// any combination of window/pacing/pull/focus is reachable.
#[allow(clippy::struct_excessive_bools)]
struct Subscriber {
    id: SubscriberId,
    is_window: bool,
    attached_at: SystemTime,
    carrier: AttachCarrier,
    /// Pull pacing (docs/explanation/rendering/pipeline.md
    /// "Demand-driven emission"): diffs ship only against a
    /// pending `NextGridFrame` pull.
    pull_pacing: bool,
    pull_pending: bool,
    minor: u16,
    image_regions: bool,
    /// `InputMsg::AnnounceOutput`; a fresh attachment starts without it.
    announce_output: bool,
    /// Shipped with this subscriber's next diff cycle, never in a
    /// rehydrate burst.
    command_outputs: VecDeque<GridMsg>,
    tx: mpsc::UnboundedSender<OutEvent>,
    buffered: Arc<AtomicUsize>,
    stream: streaming::SubscriberStream,
    /// Applied only while this subscriber owns the PTY size: at request
    /// time, or later when its input promotes it (session-lifecycle.md
    /// "Same-user mirroring").
    desired_size: Option<PtySize>,
    /// This window's own OS focus; always `false` for non-window modes.
    focused: bool,
    presentation: PresentationReports,
    /// Bounded by the connection's driver, which refuses a stream past
    /// the cap before the request reaches this task.
    producers: Vec<Producer>,
}

/// Presentation chain layer (session-lifecycle.md "Client-derived presentation state").
///
/// One `Option` over the whole report because `ConfigureTheme` is atomic;
/// an unset channel or palette slot means "no color", not fallback to
/// another window.
#[derive(Default)]
struct PresentationReports {
    theme: Option<ThemeReport>,
    os_dark: Option<bool>,
}

struct ThemeReport {
    /// Indexed by `ThemeChannel as usize`.
    channels: [Option<(u8, u8, u8)>; 3],
    palette: BTreeMap<u8, (u8, u8, u8)>,
}

impl Subscriber {
    const fn reads_regions(&self) -> bool {
        self.image_regions && self.minor >= felis_protocol::minor::IMAGE_HEADER_REGION.minor
    }

    /// `false` means the subscriber is dead (pump hung up, or backlog
    /// over cap); the caller removes it.
    fn push(&self, ev: OutEvent) -> bool {
        let len = ev.approx_wire_len();
        if self.buffered.fetch_add(len, Ordering::Relaxed) + len > SUBSCRIBER_BUFFER_CAP {
            warn!(sub = self.id.0, "subscriber outbox over cap; evicting");
            return false;
        }
        self.tx.send(ev).is_ok()
    }

    /// Past the cap: the cap is often why the subscriber is departing,
    /// and [`Self::push`] would refuse the frames that say so.
    fn force_push(&self, ev: OutEvent) -> bool {
        self.buffered
            .fetch_add(ev.approx_wire_len(), Ordering::Relaxed);
        self.tx.send(ev).is_ok()
    }
}

/// A dropped outbox is a bare EOF, which every consumer reports as a
/// transport failure rather than a stream that ended.
fn close_open_streams(sub: &Subscriber, detail: &str) {
    for producer in &sub.producers {
        let _queued = sub.force_push(OutEvent::Control(ConnToClientMsg::Error {
            subject: Subject::Stream(producer.stream),
            reason: StreamErrorReason::Unavailable,
            detail: detail.to_owned(),
        }));
    }
}

/// An unattached connection with streams on this session: its search
/// and capture producers, which read at a fresh attach's viewpoint, and
/// its `SendAndWait` watches.
struct Requester {
    id: RequesterId,
    tx: mpsc::UnboundedSender<OutEvent>,
    buffered: Arc<AtomicUsize>,
    evict: Arc<Notify>,
    producers: Vec<Producer>,
    watches: Vec<MarkWatch>,
}

impl Requester {
    /// As [`Subscriber::push`]; on `false` the caller drops the
    /// requester, and the connection learns of it through `evict`.
    fn push(&self, ev: OutEvent) -> bool {
        let len = ev.approx_wire_len();
        if self.buffered.fetch_add(len, Ordering::Relaxed) + len > SUBSCRIBER_BUFFER_CAP {
            warn!(conn = self.id.0, "unattached outbox over cap; evicting");
            self.evict.notify_one();
            return false;
        }
        self.tx.send(ev).is_ok()
    }

    /// As [`Subscriber::force_push`].
    fn force_push(&self, ev: OutEvent) -> bool {
        self.buffered
            .fetch_add(ev.approx_wire_len(), Ordering::Relaxed);
        self.tx.send(ev).is_ok()
    }

    /// Every stream ends `Unavailable`: the session is going away.
    fn close_all(&mut self, detail: &str) {
        let streams = self
            .producers
            .drain(..)
            .map(|p| p.stream)
            .chain(self.watches.drain(..).map(|w| w.stream))
            .collect::<Vec<_>>();
        for stream in streams {
            let _queued = self.force_push(OutEvent::Control(ConnToClientMsg::Error {
                subject: Subject::Stream(stream),
                reason: StreamErrorReason::Unavailable,
                detail: detail.to_owned(),
            }));
        }
    }
}

/// A `SendAndWait` watch: the first `OSC 133 ; D` from `from` on,
/// shipped only once the send's fence released it, so its `Mark`
/// follows the `WaitSent`.
struct MarkWatch {
    stream: StreamId,
    /// Absolute prompt-mark ordinal the next scan starts at.
    from: usize,
    released: bool,
    /// The mark found before the release.
    held: Option<HeldMark>,
}

#[derive(Clone, Copy)]
struct HeldMark {
    exit_code: Option<u32>,
}

/// Prompt items a session holds before it is published; older ones
/// give way and the loss is announced with `Lagged` behind
/// `SessionAdded`. Program status and row hints collapse instead, since
/// nobody can have seen the sets they replace.
const HELD_PROMPT_ITEMS: usize = 64;

/// The observer items this session publishes, every one from its own
/// task so they leave in the order the task produced them. A row the
/// creator still holds publishes nothing: its items wait, and go out
/// behind `SessionAdded` once it is published, or never if it is rolled
/// back.
struct ObserverOutbox {
    hub: tokio::sync::broadcast::Sender<NotifyToClientMsg>,
    id: SessionId,
    publication: Arc<crate::pool::Publication>,
    held: Vec<NotifyToClientMsg>,
    dropped: u64,
    exit_reported: bool,
}

impl ObserverOutbox {
    const fn new(
        hub: tokio::sync::broadcast::Sender<NotifyToClientMsg>,
        id: SessionId,
        publication: Arc<crate::pool::Publication>,
    ) -> Self {
        Self {
            hub,
            id,
            publication,
            held: Vec::new(),
            dropped: 0,
            exit_reported: false,
        }
    }

    /// Announced, and nobody is listening: an item built now reaches no
    /// one. A held row's items still wait for the observers to come.
    fn unheard(&self) -> bool {
        self.publication.is_announced() && self.hub.receiver_count() == 0
    }

    fn send(&mut self, msg: NotifyToClientMsg) {
        self.publish();
        if self.publication.is_announced() {
            crate::pool::emit(&self.hub, msg);
        } else {
            self.hold(msg);
        }
    }

    fn hold(&mut self, mut msg: NotifyToClientMsg) {
        use felis_protocol::messages::ObserverKind;
        let is_prompt = |m: &NotifyToClientMsg| ObserverKind::of(m) == Some(ObserverKind::Prompt);
        let prompt = is_prompt(&msg);
        match &mut msg {
            NotifyToClientMsg::ProgramStatus { .. } => self
                .held
                .retain(|m| !matches!(m, NotifyToClientMsg::ProgramStatus { .. })),
            NotifyToClientMsg::SessionChanged { fields, .. } => {
                self.held.retain(|m| match m {
                    NotifyToClientMsg::SessionChanged {
                        fields: earlier, ..
                    } => {
                        fields.extend(earlier.iter().copied());
                        false
                    }
                    _ => true,
                });
                fields.sort_unstable();
                fields.dedup();
            }
            _ if prompt => {
                if self.held.iter().filter(|m| is_prompt(m)).count() >= HELD_PROMPT_ITEMS
                    && let Some(oldest) = self.held.iter().position(is_prompt)
                {
                    self.held.remove(oldest);
                    self.dropped += 1;
                }
            }
            _ => {}
        }
        self.held.push(msg);
    }

    /// Announce the row once the pool has made it nameable, whoever
    /// noticed first: the creator's signal, or the next item.
    fn publish(&mut self) {
        if !self.publication.announce() {
            return;
        }
        crate::pool::emit(
            &self.hub,
            NotifyToClientMsg::SessionAdded {
                session_id: self.id.0,
            },
        );
        let missed = std::mem::take(&mut self.dropped);
        if missed > 0 {
            crate::pool::emit(&self.hub, NotifyToClientMsg::Lagged { missed });
        }
        for msg in std::mem::take(&mut self.held) {
            crate::pool::emit(&self.hub, msg);
        }
    }

    /// Once per session, whichever exit detection comes first.
    fn exited(&mut self, status: Option<ExitStatus>) {
        if std::mem::replace(&mut self.exit_reported, true) {
            return;
        }
        self.send(NotifyToClientMsg::SessionExited {
            session_id: self.id.0,
            status,
        });
    }

    /// The last item. A row that was ever nameable is announced here if
    /// the creator's signal never arrived; one that never was leaves
    /// nothing.
    fn removed(&mut self) {
        self.publish();
        self.held.clear();
        self.publication.end(&self.hub, self.id);
    }
}

// Independent lifecycle/reporting flags, not an encodable state
// machine: EOF, child exit, and the focus mirror vary freely.
#[allow(clippy::struct_excessive_bools)]
struct SessionTask {
    id: SessionId,
    session: Session,
    /// The byte budget connections admit input against; the actor holds
    /// it only to report it (`felis daemon status`).
    input_budget: Arc<tokio::sync::Semaphore>,
    subs: Vec<Subscriber>,
    requesters: Vec<Requester>,
    attachment_ids: AttachmentIds,
    meta: Arc<StdMutex<SessionMeta>>,
    /// Weak so a parked task keeps no pool alive.
    pool: Weak<Mutex<SessionPool>>,
    hub: tokio::sync::broadcast::Sender<NotifyToClientMsg>,
    observers: ObserverOutbox,
    policy: IdlePolicy,
    /// Owner of the PTY size (active-owns-winsize). `None` until anyone
    /// types; any resize applies meanwhile.
    active_sub: Option<SubscriberId>,
    /// The window whose input most recently reached the PTY: the
    /// default target of `sessions switch` (`SwitchScope::Default`).
    /// Distinct from [`Self::active_sub`], which any subscriber's input
    /// promotes, an `Ops` sender included.
    input_owner: Option<SubscriberId>,
    /// The roster in [`Self::meta`] is behind [`Self::subs`]; the next
    /// `refresh_meta` rebuilds it.
    attachments_dirty: bool,
    pty_eof: bool,
    /// What `SessionExited` carried, once EOF told the windows.
    told_exit: Option<ShellExit>,
    child_exited: bool,
    /// Reaped by the exit poll and not yet acted on: EOF or the end of a
    /// decode is owed it first.
    reaped_exit: Option<ShellExit>,
    row_cache: streaming::RowEncodeCache,
    /// Focus last reported to the PTY, so reports are edge-triggered
    /// per session-level transition rather than per window event.
    reported_focus: bool,
    /// Set on sync paths (`remove_sub`) that cannot write the `CSI O`;
    /// the run loop reconciles.
    focus_dirty: bool,
    /// The scheme last reported to the PTY, as the answer the program
    /// observes: `false` is light, which is also what no report at all
    /// answers.
    reported_os_dark: bool,
    /// As [`Self::focus_dirty`], for the scheme report.
    scheme_dirty: bool,
    /// As [`Self::focus_dirty`], for the `DECSET 2048` report.
    resize_notify_dirty: bool,
    /// The set's own answer and the notification on a change are one
    /// emitter, so a report is owed by what the program was last told,
    /// never by which edge fired.
    reported_resize: Option<ReportedResize>,
    /// Grid epochs already mirrored into [`Self::meta`], gating the
    /// `String` clones.
    meta_title_epoch: u64,
    meta_cwd_epoch: u64,
    /// [`crate::program_status::ProgramStatusStore::generation`] already
    /// mirrored into [`Self::meta`].
    meta_status_generation: u64,
    /// Test-only: a resize is an `ioctl` on the master while reports
    /// queue through the writer thread, so no observer outside the task
    /// can see the order.
    #[cfg(all(test, unix))]
    pty_steps: Vec<PtyStep>,
    /// Test-only: the grid's geometry as each PTY resize goes out.
    #[cfg(all(test, unix))]
    pty_resized_over: Vec<(u16, u16)>,
    /// An in-task queue rather than a self-addressed `SessionCmd`: the
    /// command channel is bounded, so a producer re-queueing through it
    /// could block while holding the only thread that drains it.
    pending_chunks: VecDeque<(Owner, StreamId)>,
    /// See [`SessionCmd::Settle`].
    upgrade_hold: bool,
    /// The decode the parse is stopped on, running off this task
    /// (docs/explanation/protocols/kitty-graphics.md "Dispatcher
    /// architecture"). While it runs no effect is drained and the sink
    /// is not released.
    decode: Option<InFlightDecode>,
    /// Commands held during [`Self::decode`], replayed in arrival order
    /// at its commit as the task would have run them after it
    /// ([`SessionTask::runs_during_decode`] names the exceptions).
    held_cmds: VecDeque<HeldCmd>,
    /// From a decode's commit until a drain covers the parse the sink
    /// makes after the release, which it always reports.
    awaiting_post_release_drain: bool,
    shutdown: Arc<crate::pool::ShutdownSignal>,
    idle: idle::IdleCompaction,
    /// Test-only: the next decode runs this first, on the blocking pool.
    #[cfg(all(test, unix))]
    before_decode: Option<Box<dyn FnOnce() + Send + Sync>>,
}

/// A mutex only to keep the task `Sync`: a command is `Send` alone.
struct HeldCmd(std::sync::Mutex<SessionCmd>);

impl HeldCmd {
    fn into_inner(self) -> SessionCmd {
        self.0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct InFlightDecode {
    plan: crate::graphics::CommitPlan,
    handle: tokio::task::JoinHandle<crate::graphics::Decoded>,
    payload_len: usize,
}

/// What the run loop's arms may do in one iteration.
#[derive(Clone, Copy)]
struct Hold {
    upgrade: bool,
    /// A decode in flight, or committed with the output after it not yet
    /// drained.
    decode: bool,
}

impl Hold {
    /// A pending decode still drains: its commit and the drain after its
    /// release are what end it.
    const fn drains(self) -> bool {
        !self.upgrade
    }

    /// What must follow the output after the image: the next chunk of a
    /// walk, EOF, the reap and the report reconcile.
    const fn quiesced(self) -> bool {
        !self.decode
    }
}

enum DecodeAdmit {
    Hold,
    Run,
    /// Window input, with whether it promotes its window.
    Typed {
        promotes: bool,
    },
}

const fn is_typed(msg: &InputMsg) -> bool {
    matches!(
        msg,
        InputMsg::KeyBytes(_) | InputMsg::Paste(_) | InputMsg::Mouse(_) | InputMsg::Key(_)
    )
}

/// Resolves when the in-flight decode, if any, is done. Cancel-safe in
/// `select!`.
async fn decode_done(
    decode: &mut Option<InFlightDecode>,
) -> Result<crate::graphics::Decoded, tokio::task::JoinError> {
    match decode {
        Some(decode) => (&mut decode.handle).await,
        None => std::future::pending().await,
    }
}

/// PTY-visible steps, recorded so a test can pin their order.
#[cfg(all(test, unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtyStep {
    SchemeReport,
    FocusReport,
    Resize,
    ResizeReport,
}

/// Minimum logical lines per search slice: small enough that a cancel
/// lands promptly, large enough that rebuilding the surface's row view
/// per slice is not the dominant cost.
const SEARCH_CHUNK_LINES: usize = 512;

/// Matches per search slice; bounds the burst one slice can put in a
/// subscriber's outbox.
const SEARCH_CHUNK_ITEMS: u32 = 128;

/// Ceiling on slices per walk. Each slice rebuilds the surface's row
/// view, so a fixed line budget would make a deep scrollback pay that
/// O(rows) setup hundreds of times.
const MAX_SEARCH_CHUNKS: usize = 64;

const ROWS_CHUNK: usize = 256;

struct Producer {
    stream: StreamId,
    canceled: bool,
    /// The count the terminal reports: rows and matches only, not a
    /// trailer like `RegionToClientMsg::RowsDone` (`docs/reference/ipc.md`
    /// "Conn (kind = 0)").
    emitted: u32,
    kind: ProducerKind,
}

enum ProducerKind {
    Search {
        query: felis_grid::SearchQuery,
        cursor: felis_grid::SearchCursor,
        /// Logical lines per slice.
        budget: usize,
    },
    /// Resumed by row position, as `Rows` is: the logical-line cursor
    /// of `Search` assumes history that only grows at its newest end.
    VisibleSearch {
        query: felis_grid::SearchQuery,
        row: usize,
    },
    Rows {
        source: RegionSource,
        ansi: bool,
        max_rows: Option<u32>,
        offset: usize,
    },
}

/// What a `DECSET 2048` report states, and the epoch that tells a
/// re-set apart from the set it follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReportedResize {
    pub epoch: u32,
    pub rows: u16,
    pub cols: u16,
    pub pixel_w: u16,
    pub pixel_h: u16,
}

/// `None` while the program has not opted in.
fn resize_notify_key(grid: &felis_grid::Grid) -> Option<ReportedResize> {
    let (pixel_w, pixel_h) = grid.winsize_pixels();
    grid.in_band_resize_notify().then(|| ReportedResize {
        epoch: grid.resize_notify_epoch(),
        rows: grid.rows(),
        cols: grid.cols(),
        pixel_w,
        pixel_h,
    })
}

struct SinkRelease(Arc<crate::parse_sink::ParseSignals>);

impl Drop for SinkRelease {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Coalescing window for the parsed-effect drain.
///
/// Re-locking `ParseCore` on every dirty edge ping-pongs the lock and collapses
/// PTY drain throughput. 4 ms sits within a 16 ms vsync frame while keeping pulls fresh.
const DRAIN_COALESCE: Duration = Duration::from_millis(4);

/// Bounds how long a program that closed its terminal and kept running
/// holds up `SessionExited`; that program's status is then unknown.
const REAP_AFTER_EOF_TRIES: u32 = 20;
const REAP_AFTER_EOF_STEP: Duration = Duration::from_millis(5);

fn exit_status_to_wire(status: std::process::ExitStatus) -> Option<ExitStatus> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return u32::try_from(signal).ok().map(ExitStatus::Signal);
        }
        status
            .code()
            .and_then(|code| u32::try_from(code).ok())
            .map(ExitStatus::Code)
    }
    #[cfg(windows)]
    {
        status
            .code()
            .map(|code| ExitStatus::Code(code.cast_unsigned()))
    }
}

async fn run_session(mut t: SessionTask, mut cmd_rx: mpsc::Receiver<SessionCmd>) {
    // Every way out, an unwinding panic included, releases a sink
    // waiting in `wait_for_drain`.
    let _release_sink = SinkRelease(Arc::clone(&t.session.signals));
    let mut drain_tick = tokio::time::interval(t.policy.drain_interval);
    let mut last_drain = tokio::time::Instant::now();
    let mut drain_at: Option<tokio::time::Instant> = None;
    drain_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let reason = loop {
        // docs/reference/protocols/kitty-graphics.md "Animation". With
        // nothing animating the arm parks on a long sleep
        // (idle-zero-redraw); any event re-enters the loop and
        // re-evaluates.
        let anim_now = crate::graphics::anim_now_ms();
        let anim_sleep = match t.session.images.next_animation_due_ms(anim_now) {
            Some(due) => Duration::from_millis(due.saturating_sub(anim_now)),
            None => Duration::from_secs(3600),
        };
        let drain_deadline =
            drain_at.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
        // Per iteration rather than at every `subs` mutation site; a
        // one-chunk-stale read only mis-paces ~1 KiB.
        t.session.signals.set_attached_subs(t.subs.len());
        // While a decode is pending, an arm runs only if it serves the
        // state from before the image or commutes with the commit and the
        // output after it; every other arm, and any added later, is gated
        // on `quiesced`.
        let hold = t.hold();
        t.idle.observe(
            t.session.signals.output_generation(),
            tokio::time::Instant::now(),
        );
        let idle_deadline = t
            .idle
            .deadline()
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
        let compacts = hold.drains() && hold.quiesced();
        tokio::select! {
            biased;
            // Ahead of everything and ungated by an upgrade hold: the sink
            // stays stopped on the image until its commit, and a park
            // waits for the sink.
            decoded = decode_done(&mut t.decode) => {
                let committed = t.commit_decode(decoded);
                last_drain = tokio::time::Instant::now();
                match committed {
                    Ok(None) => {}
                    Ok(Some(reason)) => break reason,
                    Err(err) => {
                        warn!(?err, "session task: PTY pipeline error");
                        break EndReason::PtyError;
                    }
                }
            }
            // Ahead of commands: the sink is blocked until this drain
            // dispatches the image placement it stopped after.
            () = t.session.signals.drain_requested() => {
                let drained = t.drain_and_fan();
                last_drain = tokio::time::Instant::now();
                if let Err(err) = drained {
                    warn!(?err, "session task: PTY pipeline error");
                    break EndReason::PtyError;
                }
            }
            // Commands next: under pull pacing they carry the
            // per-vsync pulls, which a sustained PTY burst must not
            // starve.
            cmd = cmd_rx.recv(), if t.held_cmds.len() < HELD_CMDS_CAP => {
                match cmd {
                    None => break EndReason::PoolDropped,
                    Some(cmd) => {
                        if let Some(reason) = t.handle_cmd(cmd) {
                            break reason;
                        }
                    }
                }
            }
            // Only a decode stops the command intake; otherwise the
            // command carries the same shutdown in its place.
            () = t.shutdown.requested(), if t.decode.is_some() => break EndReason::Destroyed,
            // After commands in the biased order: a `Cancel` sent
            // mid-walk lands before the next slice. The yield lets the
            // connection pump that forwards it run: this arm is always
            // ready, so without it the walk finishes in one poll.
            () = std::future::ready(()), if t.has_pending_chunks() && hold.quiesced() => {
                t.run_next_chunk();
                tokio::task::yield_now().await;
            }
            // Ungated by `attached`: a parked session still owes query
            // responses to the program and notifications to observers.
            () = t.session.signals.parsed(), if drain_at.is_none() && hold.drains() => {
                if last_drain.elapsed() >= DRAIN_COALESCE {
                    let drained = t.drain_and_fan();
                    // From the drain's end, not its start: a drain that
                    // waits out the sink's lock for longer than the window
                    // would find it elapsed and drain again at once, and a
                    // task that never yields leaves the connection pumps
                    // unpolled and the pull sitting in the socket.
                    last_drain = tokio::time::Instant::now();
                    if let Err(err) = drained {
                        warn!(?err, "session task: PTY pipeline error");
                        break EndReason::PtyError;
                    }
                } else {
                    // The dirty flag stays set across the window, so
                    // the sink's per-chunk marks do not re-notify (see
                    // `DRAIN_COALESCE`).
                    drain_at = Some(last_drain + DRAIN_COALESCE);
                }
            }
            () = tokio::time::sleep_until(drain_deadline), if drain_at.is_some() && hold.drains() => {
                drain_at = None;
                let drained = t.drain_and_fan();
                last_drain = tokio::time::Instant::now();
                if let Err(err) = drained {
                    warn!(?err, "session task: PTY pipeline error");
                    break EndReason::PtyError;
                }
            }
            res = t.session.eof.eof(), if !t.pty_eof && hold.quiesced() => {
                match res {
                    Ok(()) => {
                        t.pty_eof = true;
                        t.stamp_exited_meta();
                        // `felis_pty` guarantees the child's final
                        // output is parsed by the time EOF lands here.
                        if let Err(err) = t.drive_cycle() {
                            debug!(?err, "session task: effect drain at EOF failed");
                        }
                        let before = t.session.program_status.generation();
                        t.session.program_status.expire_transient();
                        streaming::note_status_change(&mut t.session, before);
                        t.refresh_meta();
                        t.publish_observed();
                        let status = t.reap_after_eof().await;
                        t.observers.exited(status);
                        t.notify_shell_exit(status);
                        t.evict_all(None);
                        t.end_watches();
                    }
                    Err(err) => {
                        warn!(?err, "session task: PTY read error");
                        break EndReason::PtyError;
                    }
                }
            }
            // No drain here: PTY flow control is the sink's pacer
            // (`crate::parse_sink`).
            // Not while held for an upgrade: the dump names this child
            // unreaped, so reaping it now would hand the successor a pid
            // that may be reused.
            _ = drain_tick.tick(), if hold.drains() && hold.quiesced() => {
                // Ungated by `attached`: a program left holding the PTY
                // keeps EOF away, and an observer is owed the exit.
                t.poll_child_exit();
                if t.reap_due() {
                    break EndReason::Reaped;
                }
            }
            // Ungated by subscribers and by pull pacing: the store's
            // frame index advances while parked so a reattach lands on
            // the live frame (REQ-307).
            () = tokio::time::sleep(anim_sleep) => {
                t.animation_tick();
            }
            () = tokio::time::sleep_until(idle_deadline), if t.idle.deadline().is_some() && compacts => {
                t.idle
                    .timer_fired(t.session.signals.output_generation(), tokio::time::Instant::now());
            }
            // Last, so every other arm goes first between slices; the
            // yield lets the connection pumps run.
            () = std::future::ready(()), if t.idle.running() && compacts => {
                t.compact_slice();
                tokio::task::yield_now().await;
            }
        }
        // Sync paths (detach, eviction, resize) cannot write to the
        // PTY; the edges they leave are reconciled here, behind the
        // replies to everything parsed before them.
        // Read again: the arm may have committed the decode or drained
        // past it.
        if t.hold().quiesced()
            && let Some(reason) = t.reconcile_reports()
        {
            break reason;
        }
    };
    match reason {
        EndReason::Destroyed => debug!(id = ?t.id, "session task: destroyed"),
        EndReason::PoolDropped => debug!(id = ?t.id, "session task: pool dropped"),
        EndReason::PtyError => info!(id = ?t.id, "session task: dropping session on PTY error"),
        EndReason::Reaped => info!(id = ?t.id, "session task: reaped post-exit idle session"),
    }
    // A command can win the race against the parse drain, and
    // `SessionRemoved` must follow what was parsed. Not under an upgrade
    // hold: the dump already took the parse state it describes.
    if !t.upgrade_hold
        && let Err(err) = t.drain_and_fan()
    {
        debug!(?err, "session task: effect drain at teardown failed");
    }
    t.evict_all(None);
    t.close_requesters("the session ended");
    let teardown = if let Some(pool) = t.pool.upgrade() {
        let mut guard = pool.lock().await;
        let teardown = guard.begin_teardown();
        guard.remove(t.id);
        Some(teardown)
    } else {
        None
    };
    t.publish_observed();
    t.observers.removed();
    end_child(&t.session.child).await;
    drop(teardown);
}

/// The window is for a shell that traps `SIGHUP` to save state; it is
/// short because the session is already gone from the pool, so
/// anything still running is unreachable by the user.
#[cfg(not(test))]
const HANGUP_GRACE: Duration = Duration::from_secs(2);
/// Only a fixture that ignores `SIGHUP` waits the grace out; a shell
/// that dies on the hangup and is polled late is killed instead, which
/// ends it the same way.
#[cfg(test)]
const HANGUP_GRACE: Duration = Duration::from_millis(250);

/// How long the reap after the `SIGKILL` is polled for. Bounded so a
/// child stuck in uninterruptible sleep cannot hold [`end_child`] open
/// for the connection's life.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// The longest [`end_child`] can take: a child that ignores `SIGHUP`
/// spends both graces in turn. `serve::ROLLBACK_TIMEOUT` is derived
/// from this so the rollback's wait cannot expire on the very child the
/// graces are for.
pub(crate) const CHILD_TEARDOWN_BUDGET: Duration = match HANGUP_GRACE.checked_add(REAP_GRACE) {
    Some(budget) => budget,
    None => unreachable!(),
};

/// Reaped here rather than left to the OS: the daemon stays the
/// child's parent for its whole life, so an unreaped exit is a zombie
/// until the daemon itself dies.
pub(crate) async fn end_child(child: &ChildHandle) {
    match child.hangup() {
        Ok(false) => return,
        Ok(true) => {}
        Err(err) => warn!(?err, "session teardown: hangup"),
    }
    match child.wait_timeout(HANGUP_GRACE).await {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(err) => {
            warn!(?err, "session teardown: waiting on the child");
            return;
        }
    }
    match child.kill() {
        Ok(true) => info!("session teardown: child ignored the hangup; killed"),
        Ok(false) => {}
        Err(err) => warn!(?err, "session teardown: kill"),
    }
    // Waited to an observed exit, not looked at once: `SIGKILL` is
    // delivered, not completed, so a single look can miss it and leave a
    // zombie the daemon parents for the rest of its life.
    match child.wait_timeout(REAP_GRACE).await {
        Ok(Some(_)) => {}
        Ok(None) => warn!("session teardown: the killed child was not reaped within the grace"),
        Err(err) => warn!(?err, "session teardown: reaping the killed child"),
    }
}

/// Why `event` would never reach the program as a report: the
/// encoder's own filter, plus the grid bounds. The texts are what
/// `felis sessions send --mouse` prints.
fn unreported_mouse(
    event: MouseEvent,
    protocol: MouseProtocol,
    rows: u16,
    cols: u16,
) -> Option<String> {
    if event.x == 0 || event.y == 0 || event.x > cols || event.y > rows {
        return Some(format!(
            "the cell is outside the {cols}x{rows} grid (COL and ROW are 0-based)"
        ));
    }
    if felis_grid::mouse_event_reported(protocol, event.action) {
        return None;
    }
    Some(if protocol == MouseProtocol::Off {
        "the program has no mouse reporting on; nothing was sent".to_owned()
    } else {
        "the program reports clicks but not drags (?1000 without ?1002); nothing was sent"
            .to_owned()
    })
}

/// The keyboard modes come off the same locked core the mouse encoding
/// reads, so a mode the child set is in effect for the very next key.
/// Keys are encoded daemon-side so the bytes follow the modes this daemon
/// has already parsed rather than the ones the client's last grid frame
/// carried (docs/explanation/input.md "Keyboard").
fn encode_input(core: &ParseCore, msg: &InputMsg) -> Option<Vec<u8>> {
    match msg {
        InputMsg::Mouse(event) => felis_grid::encode_mouse(
            felis_grid::fill_cell_center_pixels(
                *event,
                (core.grid.cols(), core.grid.rows()),
                core.grid.winsize_pixels(),
            ),
            core.grid.mouse_protocol(),
            core.grid.mouse_encoding(),
        ),
        InputMsg::Key(event) => encode_key(core, event),
        _ => None,
    }
}

fn encode_key(core: &ParseCore, event: &KeyEvent) -> Option<Vec<u8>> {
    let modes = core.grid.mode_snapshot();
    key_encode::encode(
        &event.key,
        event.text.as_deref(),
        event.mods,
        event.kind,
        core.grid.kitty_kbd_flags(),
        modes.modify_other_keys,
        modes.application_cursor,
        modes.application_keypad,
        modes.win32_input_mode,
        event.location,
    )
}

struct MetaFacts {
    rows: u16,
    cols: u16,
    last_exit_code: Option<u32>,
    title: Option<(u64, Option<String>)>,
    cwd: Option<(u64, Option<String>)>,
}

type Composed = (SubscriberId, Result<Vec<OutEvent>, ConnError>);

struct Cycle {
    meta: MetaFacts,
    notifications: Option<streaming::TakenNotifications>,
    facets: Vec<GridMsg>,
    clipboard: ClipboardBatch,
    composed: Vec<Composed>,
}

/// A cycle's OSC 52 writes and the window they belong to, resolved
/// while composing: every push in `finish_cycle` can evict the active
/// window, and a target resolved after that would land on a bystander.
#[derive(Default)]
struct ClipboardBatch {
    target: Option<SubscriberId>,
    writes: Vec<felis_grid::ClipboardWrite>,
}

fn pty_write_outcome(res: Result<(), ConnError>) -> Option<EndReason> {
    res.err().map(|err| {
        warn!(?err, "session task: PTY write error");
        EndReason::PtyError
    })
}

impl SessionTask {
    fn handle_cmd(&mut self, cmd: SessionCmd) -> Option<EndReason> {
        if self.decode.is_none() {
            return self.run_cmd(cmd);
        }
        match self.admits_during_decode(&cmd) {
            DecodeAdmit::Hold => {
                self.held_cmds
                    .push_back(HeldCmd(std::sync::Mutex::new(cmd)));
                None
            }
            DecodeAdmit::Run => self.run_cmd(cmd),
            DecodeAdmit::Typed { promotes } => {
                let SessionCmd::Input {
                    sub,
                    msg,
                    reservation,
                } = cmd
                else {
                    unreachable!("only window input is admitted as typed")
                };
                self.input_with(sub, msg, reservation, promotes)
            }
        }
    }

    /// What a decode does not hold: input with no size or input-owner
    /// change while nothing is held and no report of the task's own is
    /// owed (the input path writes one first), reads no held command's
    /// order depends on, and the shutdown. Everything else, and every
    /// variant added later, waits for the commit.
    fn admits_during_decode(&self, cmd: &SessionCmd) -> DecodeAdmit {
        match cmd {
            SessionCmd::Input {
                msg: InputMsg::NextGridFrame,
                ..
            }
            | SessionCmd::Stats { .. }
            // Ends the session, so nothing held after it matters, and a
            // decode that never finishes cannot keep it alive.
            | SessionCmd::Shutdown => DecodeAdmit::Run,
            SessionCmd::Input { sub, msg, .. } if is_typed(msg) && self.writes_input_now() => {
                let promotes = self.input_promotes(msg);
                if self.input_keeps_size(*sub, promotes) {
                    DecodeAdmit::Typed { promotes }
                } else {
                    DecodeAdmit::Hold
                }
            }
            SessionCmd::RemoteInput { msg, .. } if is_typed(msg) && self.writes_input_now() => {
                DecodeAdmit::Run
            }
            _ => DecodeAdmit::Hold,
        }
    }

    fn input_keeps_size(&self, sub: SubscriberId, promotes: bool) -> bool {
        !promotes
            || (self.active_sub == Some(sub)
                && self
                    .sub_index(sub)
                    .is_some_and(|i| !self.subs[i].is_window || self.input_owner == Some(sub)))
    }

    #[allow(
        clippy::missing_const_for_fn,
        reason = "VecDeque::is_empty is const only above the MSRV"
    )]
    fn writes_input_now(&self) -> bool {
        self.held_cmds.is_empty() && !self.owes_reports()
    }

    fn run_cmd(&mut self, cmd: SessionCmd) -> Option<EndReason> {
        match cmd {
            SessionCmd::Subscribe(req) => {
                self.subscribe(req);
                None
            }
            SessionCmd::Unsubscribe { sub } => {
                self.remove_sub(sub);
                None
            }
            SessionCmd::Input {
                sub,
                msg,
                reservation,
            } => self.handle_admitted_input(sub, msg, reservation),
            SessionCmd::Search { owner, msg, stream } => self.handle_search(owner, msg, stream),
            SessionCmd::CancelStream { owner, stream } => {
                self.cancel_producer(owner, stream);
                None
            }
            SessionCmd::AddRequester(handle) => {
                self.add_requester(handle);
                None
            }
            SessionCmd::DropRequester { id } => {
                self.remove_requester(id);
                None
            }
            SessionCmd::PrepareSend {
                watch,
                mouse,
                reply,
            } => {
                let prepared = self.prepare_send(watch, &mouse);
                drop(reply.send(prepared));
                None
            }
            SessionCmd::RemoteInput { msg, reservation } => {
                self.handle_remote_input(msg, reservation)
            }
            SessionCmd::SendFence { watch, id, reply } => {
                let _ = reply.send(self.send_fence(watch, id));
                None
            }
            SessionCmd::ConfigureTheme {
                sub,
                fg,
                bg,
                cursor,
                palette,
            } => {
                let i = self.sub_index(sub)?;
                self.subs[i].presentation.theme = Some(ThemeReport {
                    channels: [fg, bg, cursor],
                    palette,
                });
                self.recompute_presentation_now()
            }
            SessionCmd::InputFence { sub, request } => {
                let i = self.sub_index(sub)?;
                let _evicted = self.push_to(
                    sub,
                    i,
                    OutEvent::Session {
                        msg: SessionToClientMsg::InputAccepted,
                        correlation: Correlation::request(request),
                    },
                );
                None
            }
            SessionCmd::Region {
                owner,
                source,
                ansi,
                request,
            } => {
                self.handle_region(owner, source, ansi, request);
                None
            }
            SessionCmd::RegionRows {
                owner,
                source,
                ansi,
                max_rows,
                stream,
            } => {
                self.start_rows(owner, source, ansi, max_rows, stream);
                None
            }
            SessionCmd::Stats { reply } => {
                let _ = reply.send(self.stats());
                None
            }
            SessionCmd::EvictAll { reply } => {
                let n = self.subs.len();
                self.evict_all(Some("evicted by `felis sessions evict`"));
                let _ = reply.send(n);
                None
            }
            SessionCmd::PushToTarget { msg, scope, reply } => {
                let outcome = self.push_to_scope(&msg, scope);
                let _ = reply.send(outcome);
                None
            }
            SessionCmd::Shutdown => Some(EndReason::Destroyed),
            SessionCmd::Published => {
                self.observers.publish();
                None
            }
            SessionCmd::Settle(done) => {
                self.upgrade_hold = true;
                let _settled = done.send(());
                None
            }
            SessionCmd::Capture(reply) => {
                let _sent = reply.send(Box::new(self.capture_state()));
                None
            }
            SessionCmd::Resume => {
                self.upgrade_hold = false;
                None
            }
        }
    }

    fn subscribe(&mut self, req: SubscribeReq) {
        let if_exited = req.if_exited;
        // Checked inside the actor, not off the roster's `exited` flag
        // client-side: that flag is a snapshot, and a session exiting
        // between the listing and the attach would leave a window
        // parked on a corpse that never pushes it off and blocks the
        // reap.
        if if_exited == IfExited::Refuse && self.program_gone() {
            drop(req.reply.send(Err(SubscribeRefused::Exited)));
            return;
        }
        // A scroll the effect queue still holds has already moved the
        // cells the rehydrate snapshots; drained later it would fan to
        // the fresh shadow too and double-apply.
        let mut out = Vec::new();
        let (stream, rows, cols) = {
            let shared = Arc::clone(&self.session.core);
            let mut core = shared.lock();
            if let Err(err) = self.drain_effects_under(&mut core) {
                warn!(?err, "session task: effect drain before subscribe failed");
            }
            self.fan_out_grid_state_under(&mut core);
            let stream = match streaming::SubscriberStream::rehydrated(
                req.mode,
                &core.grid,
                &self.session.images,
                &self.session.placements,
                &mut out,
            ) {
                Ok(stream) => stream,
                Err(err) => {
                    // Dropping `reply` surfaces `AttachFailed`; the
                    // session stays alive.
                    warn!(?err, "session task: rehydrate compose failed");
                    return;
                }
            };
            (stream, core.grid.rows(), core.grid.cols())
        };
        let id = SubscriberId(self.attachment_ids.mint());
        let sub = Subscriber {
            id,
            is_window: req.mode.is_window(),
            attached_at: SystemTime::now(),
            carrier: req.carrier,
            pull_pacing: req.pull_paced,
            pull_pending: false,
            minor: req.minor,
            image_regions: req.image_regions,
            announce_output: false,
            command_outputs: VecDeque::new(),
            tx: req.tx,
            buffered: req.buffered,
            stream,
            desired_size: None,
            focused: false,
            presentation: PresentationReports::default(),
            producers: Vec::new(),
        };
        let alive = out.into_iter().all(|ev| sub.push(ev));
        if !alive {
            return;
        }
        self.subs.push(sub);
        self.attachments_dirty = true;
        // Not `recompute_presentation_now`: the attach must finish even
        // when the PTY is gone; the run loop owns the report.
        self.recompute_presentation();
        info!(id = ?self.id, sub = id.0, total = self.subs.len(), "subscriber attached");
        // Before the reply, so the attach ack reports this subscriber rather
        // than the parked session it joined.
        self.refresh_meta();
        let mut info = super::session_info_from_meta(
            self.id,
            self.meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            self.session.resizer.foreground_pgrp(),
            Instant::now(),
        );
        info.dims.rows = rows;
        info.dims.cols = cols;
        if req.reply.send(Ok(SubscribeOk { sub: id, info })).is_err() {
            self.remove_sub(id);
            return;
        }
        if if_exited == IfExited::Notify
            && let Some(ShellExit(status)) = self.told_exit
        {
            if let Some(sub) = self.subs.iter().find(|s| s.id == id && s.is_window) {
                let _ = sub.push(OutEvent::Push(PushMsg::SessionExited {
                    id: self.id.0,
                    status,
                }));
            }
            self.remove_sub(id);
        }
    }

    fn sub_index(&self, id: SubscriberId) -> Option<usize> {
        self.subs.iter().position(|s| s.id == id)
    }

    fn remove_sub(&mut self, id: SubscriberId) {
        if let Some(i) = self.sub_index(id) {
            let was_window = self.subs[i].is_window;
            let sub = self.subs.remove(i);
            close_open_streams(&sub, "the subscription ended");
            if sub.announce_output {
                let capture = self.announces_output();
                self.session
                    .lock_core()
                    .grid
                    .set_capture_command_output(capture);
            }
            drop(sub);
            debug!(id = ?self.id, sub = id.0, left = self.subs.len(), "subscriber detached");
            self.attachments_dirty = true;
            if was_window {
                self.focus_dirty = true;
            }
        }
        if self.active_sub == Some(id) {
            // The PTY keeps its size until the next subscriber types.
            self.active_sub = None;
        }
        if self.input_owner == Some(id) {
            // Cleared, not a tombstone: a tombstone would refuse a
            // default-scope switch on a session with exactly one
            // possible window target.
            self.input_owner = None;
        }
        // After the marker clears: the departing window must not still
        // head the chain it is being removed from.
        self.recompute_presentation();
        self.refresh_meta();
    }

    fn announces_output(&self) -> bool {
        self.subs.iter().any(|s| s.announce_output)
    }

    /// The OR, not last-writer: a background mirror blurring must not
    /// un-focus a session the foreground window still shows focused,
    /// and window events arrive in any order across connections.
    fn window_focused(&self) -> bool {
        self.subs.iter().any(|s| s.is_window && s.focused)
    }

    fn capture_state(&self) -> crate::upgrade::dump::SessionState {
        use crate::upgrade::dump::{NotificationDump, SessionState};
        let millis = |elapsed: Duration| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let now = Instant::now();
        let (idle_ms, last_notification) = {
            let meta = self
                .meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // An attached session is not idle: its windows leave at the
            // exec, which is when its idle time starts.
            let idle_ms = if self.subs.is_empty() {
                millis(now.saturating_duration_since(meta.idle_since))
            } else {
                0
            };
            let last = meta.last_notification.as_ref().map(|n| NotificationDump {
                title: n.title.clone(),
                body: n.body.clone(),
                urgency: n.urgency,
                age_ms: millis(now.saturating_duration_since(n.at)),
            });
            (idle_ms, last)
        };
        let core = self.session.lock_core();
        SessionState {
            grid: core.grid.clone(),
            parser: core.parser.clone(),
            table_gc: core.table_gc.clone(),
            images: self.session.images.clone(),
            placements: self.session.placements.clone(),
            saved_screens: self.session.saved_screens.clone(),
            reassembler: self.session.graphics_reassembler.clone(),
            shm_segments: self
                .session
                .shm_segments
                .names()
                .map(str::to_owned)
                .collect(),
            reported_focus: self.reported_focus,
            reported_os_dark: self.reported_os_dark,
            reported_resize: self.reported_resize,
            idle_ms,
            last_notification,
            program_status: self.session.program_status.reports(),
            terminal_progress: self.session.terminal_progress,
        }
    }

    fn sync_focus_to_pty(&mut self) -> Option<EndReason> {
        self.focus_dirty = false;
        // Same reason [`Self::sync_color_scheme_to_pty`] stops here.
        if self.pty_eof {
            return None;
        }
        let now = self.window_focused();
        if now == self.reported_focus {
            return None;
        }
        self.reported_focus = now;
        if self.session.lock_core().grid.focus_reporting() {
            let payload: &[u8] = if now { b"\x1b[I" } else { b"\x1b[O" };
            #[cfg(all(test, unix))]
            self.pty_steps.push(PtyStep::FocusReport);
            if let Some(reason) = pty_write_outcome(write_pty(&self.session.writer, payload)) {
                return Some(reason);
            }
        }
        None
    }

    /// Most preferred first: the last window input owner, then the
    /// remaining windows in reverse attach order
    /// (docs/explanation/architecture/session-lifecycle.md
    /// "Client-derived presentation state").
    fn presentation_chain(&self) -> impl Iterator<Item = &Subscriber> {
        let owner = self
            .input_owner
            .and_then(|id| self.subs.iter().find(|s| s.id == id && s.is_window));
        owner.into_iter().chain(
            self.subs
                .iter()
                .rev()
                .filter(move |s| s.is_window && owner.map(|o| o.id) != Some(s.id)),
        )
    }

    fn recompute_presentation(&mut self) {
        let theme = self
            .presentation_chain()
            .find_map(|s| s.presentation.theme.as_ref())
            .map(|t| {
                let mut palette = [None; 256];
                for (&index, &rgb) in &t.palette {
                    palette[usize::from(index)] = Some(rgb);
                }
                (t.channels, palette)
            });
        let os_dark = self
            .presentation_chain()
            .find_map(|s| s.presentation.os_dark);
        let mut core = self.session.lock_core();
        let grid = &mut core.grid;
        // Not change-tested here: whether the scheme moved is a
        // question about the last answer the program was given, which
        // only the reconcile can tell (two recomputes in one pass may
        // leave and re-enter dark).
        if let Some((channels, palette)) = theme {
            for channel in [
                ThemeChannel::Foreground,
                ThemeChannel::Background,
                ThemeChannel::Cursor,
            ] {
                grid.set_theme_config_default(channel, channels[channel as usize]);
            }
            for (index, rgb) in (0..=u8::MAX).zip(palette) {
                grid.set_palette_config_default(index, rgb);
            }
        }
        if let Some(dark) = os_dark {
            grid.set_os_dark(dark);
        }
        drop(core);
        self.scheme_dirty = true;
    }

    /// Compared on the answer the program can observe, not the stored
    /// `Option`: an unset preference answers light, so a first light
    /// report changes nothing the program could see. The mode is read
    /// at write time, so a program enabling `DECSET 2031` after the
    /// change queries rather than receiving a stale unsolicited report.
    fn sync_color_scheme_to_pty(&mut self) -> Option<EndReason> {
        self.scheme_dirty = false;
        // Attaching to a corpse is allowed, and the client's reports
        // arrive right behind the attach, but the daemon holds no slave
        // fd: on the BSDs the write fails with `EIO`, takes the writer
        // thread with it, and the next write reports a dead session
        // mid-reap-grace.
        if self.pty_eof {
            return None;
        }
        let now = self.session.lock_core().grid.os_dark() == Some(true);
        if now == self.reported_os_dark {
            return None;
        }
        self.reported_os_dark = now;
        let report = {
            let core = self.session.lock_core();
            core.grid
                .color_scheme_notify()
                .then(|| core.grid.color_scheme_report_bytes())
        };
        let report = report?;
        #[cfg(all(test, unix))]
        self.pty_steps.push(PtyStep::SchemeReport);
        pty_write_outcome(write_pty(&self.session.writer, &report))
    }

    /// Two independently triggered emitters would double-report one
    /// geometry and could deliver the geometry history backwards.
    fn sync_resize_notify_to_pty(&mut self) -> Option<EndReason> {
        self.resize_notify_dirty = false;
        // Same reason [`Self::sync_color_scheme_to_pty`] stops here.
        if self.pty_eof {
            return None;
        }
        let now = {
            let core = self.session.lock_core();
            resize_notify_key(&core.grid).map(|key| (key, core.grid.resize_notify_report_bytes()))
        };
        // A reset forgets the last answer, so re-entering the mode at
        // the same geometry still answers; a reset coalesced with the
        // re-set in one burst is only visible as the epoch.
        let Some((key, report)) = now else {
            self.reported_resize = None;
            return None;
        };
        if self.reported_resize == Some(key) {
            return None;
        }
        self.reported_resize = Some(key);
        #[cfg(all(test, unix))]
        self.pty_steps.push(PtyStep::ResizeReport);
        pty_write_outcome(write_pty(&self.session.writer, &report))
    }

    fn recompute_presentation_now(&mut self) -> Option<EndReason> {
        self.recompute_presentation();
        self.sync_color_scheme_to_pty()
    }

    fn push_to(&mut self, sub_id: SubscriberId, i: usize, ev: OutEvent) -> bool {
        if self.subs[i].push(ev) {
            true
        } else {
            self.remove_sub(sub_id);
            false
        }
    }

    /// Queued past the backlog cap: the terminal is the only frame that
    /// tells a reader "no more results" from "the connection ended",
    /// so the cap must not turn it into a bare EOF.
    fn push_terminal(&mut self, owner: Owner, terminal: ConnToClientMsg) {
        let alive = match owner {
            Owner::Sub(id) => match self.sub_index(id) {
                Some(i) => self.subs[i].force_push(OutEvent::Control(terminal)),
                None => return,
            },
            Owner::Conn(id) => match self.requester_index(id) {
                Some(i) => self.requesters[i].force_push(OutEvent::Control(terminal)),
                None => return,
            },
        };
        if !alive {
            self.remove_owner(owner);
        }
    }

    /// As [`Self::push_to`], for either kind of owner; `false` once the
    /// owner is gone.
    fn push_owner(&mut self, owner: Owner, ev: OutEvent) -> bool {
        match owner {
            Owner::Sub(id) => match self.sub_index(id) {
                Some(i) => self.push_to(id, i, ev),
                None => false,
            },
            Owner::Conn(id) => match self.requester_index(id) {
                Some(i) => {
                    if self.requesters[i].push(ev) {
                        true
                    } else {
                        self.remove_requester(id);
                        false
                    }
                }
                None => false,
            },
        }
    }

    fn remove_owner(&mut self, owner: Owner) {
        match owner {
            Owner::Sub(id) => self.remove_sub(id),
            Owner::Conn(id) => self.remove_requester(id),
        }
    }

    fn producers_mut(&mut self, owner: Owner) -> Option<&mut Vec<Producer>> {
        match owner {
            Owner::Sub(id) => self
                .subs
                .iter_mut()
                .find(|s| s.id == id)
                .map(|s| &mut s.producers),
            Owner::Conn(id) => self
                .requesters
                .iter_mut()
                .find(|r| r.id == id)
                .map(|r| &mut r.producers),
        }
    }

    /// A subscriber reads where it scrolled; an unattached connection
    /// reads where a fresh attach would land, the live bottom.
    fn viewport_of(&self, owner: Owner) -> u32 {
        match owner {
            Owner::Sub(id) => self.subs.iter().find(|s| s.id == id).map_or(0, |s| {
                s.stream.diff.viewport_now(&self.session.lock_core().grid)
            }),
            Owner::Conn(_) => 0,
        }
    }

    fn requester_index(&self, id: RequesterId) -> Option<usize> {
        self.requesters.iter().position(|r| r.id == id)
    }

    /// Streams end unterminated: the connection that would read the
    /// terminals is gone or evicted.
    fn remove_requester(&mut self, id: RequesterId) {
        if let Some(i) = self.requester_index(id) {
            self.requesters.remove(i);
            self.pending_chunks
                .retain(|(owner, _)| *owner != Owner::Conn(id));
        }
    }

    fn add_requester(&mut self, handle: RequesterHandle) {
        if self.requester_index(handle.id).is_some() {
            return;
        }
        self.requesters.push(Requester {
            id: handle.id,
            tx: handle.tx,
            buffered: handle.buffered,
            evict: handle.evict,
            producers: Vec::new(),
            watches: Vec::new(),
        });
    }

    fn resolve_scope(&self, scope: SwitchScope) -> Result<Vec<SubscriberId>, SwitchDenied> {
        let windows = || self.subs.iter().filter(|s| s.is_window);
        match scope {
            SwitchScope::Default => {
                if let Some(owner) = self.input_owner {
                    return Ok(vec![owner]);
                }
                let mut only = windows();
                match (only.next(), only.next()) {
                    (Some(sole), None) => Ok(vec![sole.id]),
                    _ => Err(SwitchDenied::NoInputOwner),
                }
            }
            SwitchScope::Attachment(id) => windows()
                .find(|s| s.id.0 == id)
                .map(|s| vec![s.id])
                .ok_or(SwitchDenied::NoSuchAttachment { attachment: id }),
        }
    }

    /// [`PushOutcome::Accepted`] counts outboxes that took the frame,
    /// not windows that landed; it reaches the wire as
    /// `OpsSwitched.queued`.
    fn push_to_scope(&mut self, msg: &PushMsg, scope: SwitchScope) -> PushOutcome {
        let targets = match self.resolve_scope(scope) {
            Ok(targets) => targets,
            Err(denied) => return PushOutcome::Denied(denied),
        };
        let mut accepted = 0_u32;
        // `remove_sub` rather than a `retain` sweep: an evicted
        // subscriber can still hold open streams, and dropping its
        // outbox in place would turn each terminal into a bare EOF.
        let mut dead = Vec::new();
        for id in targets {
            let Some(sub) = self.subs.iter().find(|s| s.id == id) else {
                continue;
            };
            if sub.push(OutEvent::Push(msg.clone())) {
                accepted = accepted.saturating_add(1);
            } else {
                dead.push(sub.id);
            }
        }
        for id in dead {
            self.remove_sub(id);
        }
        self.refresh_meta();
        PushOutcome::Accepted(accepted)
    }

    /// `session-lifecycle.md` "Post-exit reaping". Sent just before
    /// [`Self::evict_all`]; the connection pump (`serve.rs`) flushes
    /// buffered events before it observes the channel close, so the
    /// message lands ahead of the drop.
    fn notify_shell_exit(&mut self, status: Option<ExitStatus>) {
        self.told_exit = Some(ShellExit(status));
        for sub in &self.subs {
            if sub.is_window {
                let _ = sub.push(OutEvent::Push(PushMsg::SessionExited {
                    id: self.id.0,
                    status,
                }));
            }
        }
    }

    /// A child closes its PTY descriptors before the kernel makes it
    /// waitable, so EOF can land a moment ahead of the status. Never
    /// while held for an upgrade: the dump names this child unreaped.
    async fn reap_after_eof(&mut self) -> Option<ExitStatus> {
        if let Some(ShellExit(status)) = self.reaped_exit.take() {
            self.child_exited = true;
            return status;
        }
        if self.upgrade_hold {
            return None;
        }
        for _ in 0..REAP_AFTER_EOF_TRIES {
            match self.session.child.try_wait() {
                Ok(Some(status)) => {
                    self.child_exited = true;
                    return exit_status_to_wire(status);
                }
                Ok(None) => tokio::time::sleep(REAP_AFTER_EOF_STEP).await,
                Err(err) => {
                    debug!(?err, "session task: reaping the child at EOF failed");
                    return None;
                }
            }
        }
        None
    }

    fn stamp_exited_meta(&self) {
        self.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .exited = true;
    }

    fn stats(&self) -> SessionStats {
        // One load per outbox feeding both numbers: two passes over
        // gauges the writer tasks are draining can report a max the
        // total does not contain.
        let (max_subscriber_backlog, total_subscriber_backlog) = self
            .subs
            .iter()
            .map(|sub| &sub.buffered)
            .chain(self.requesters.iter().map(|r| &r.buffered))
            .fold((0usize, 0usize), |(max, total), buffered| {
                let queued = buffered.load(Ordering::Relaxed);
                (max.max(queued), total.saturating_add(queued))
            });
        SessionStats {
            image_bytes: self.session.images.bytes_used()
                + self
                    .session
                    .saved_screens
                    .primary
                    .iter()
                    .chain(&self.session.saved_screens.alternate)
                    .map(|saved| saved.images.bytes_used())
                    .sum::<usize>(),
            image_bytes_cap: self.session.images.bytes_cap() * 2,
            decode_bytes_in_flight: self
                .decode
                .as_ref()
                .map(|decode| decode.payload_len)
                .or_else(|| self.session.graphics_reassembler.in_flight_bytes()),
            max_subscriber_backlog,
            total_subscriber_backlog,
            pty_input_bytes: felis_protocol::limits::PTY_INPUT_BUDGET
                .saturating_sub(self.input_budget.available_permits()),
        }
    }

    fn evict_all(&mut self, reason: Option<&str>) {
        for sub in self.subs.drain(..) {
            // Not `push_to`: it would try to evict a subscriber this
            // loop already owns.
            close_open_streams(&sub, reason.unwrap_or("the session ended"));
            if let Some(reason) = reason {
                let _queued = sub.force_push(OutEvent::Push(PushMsg::Evicted {
                    reason: reason.to_owned(),
                }));
            }
        }
        self.active_sub = None;
        self.input_owner = None;
        self.attachments_dirty = true;
        self.recompute_presentation();
        self.refresh_meta();
    }

    /// `idle_since` restamps whenever the session goes parked: the reap
    /// grace and the recency listing measure from the last detach.
    fn refresh_meta(&mut self) {
        // The core and meta locks never nest.
        let facts = {
            let core = self.session.lock_core();
            self.read_meta_facts(&core)
        };
        self.apply_meta(facts);
    }

    fn read_meta_facts(&self, core: &ParseCore) -> MetaFacts {
        let title = (core.grid.title_epoch() != self.meta_title_epoch).then(|| {
            (
                core.grid.title_epoch(),
                core.grid.title().map(str::to_owned),
            )
        });
        let cwd = (core.grid.cwd_epoch() != self.meta_cwd_epoch)
            .then(|| (core.grid.cwd_epoch(), core.grid.cwd().map(str::to_owned)));
        MetaFacts {
            rows: core.grid.rows(),
            cols: core.grid.cols(),
            last_exit_code: core.grid.last_command_exit(),
            title,
            cwd,
        }
    }

    fn apply_meta(&mut self, facts: MetaFacts) {
        let MetaFacts {
            rows,
            cols,
            last_exit_code,
            title,
            cwd,
        } = facts;
        let status_generation = self.session.program_status.generation();
        let program_status = (status_generation != self.meta_status_generation)
            .then(|| self.session.program_status.snapshot());
        let mut meta = self
            .meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(records) = program_status {
            self.meta_status_generation = status_generation;
            meta.program_status = records;
        }
        meta.terminal_progress = self.session.terminal_progress;
        meta.rows = rows;
        meta.cols = cols;
        let mut changed = Vec::new();
        if let Some((epoch, title)) = title {
            self.meta_title_epoch = epoch;
            if meta.title != title {
                changed.push(SessionField::Title);
            }
            meta.title = title;
        }
        if let Some((epoch, cwd)) = cwd {
            self.meta_cwd_epoch = epoch;
            if meta.cwd != cwd {
                changed.push(SessionField::Cwd);
            }
            meta.cwd = cwd;
        }
        meta.last_exit_code = last_exit_code;
        if meta.subscribers != self.subs.len() {
            if self.subs.is_empty() {
                meta.idle_since = Instant::now();
            }
            meta.subscribers = self.subs.len();
        }
        // Its own signal, not the count comparison above: an
        // input-owner transfer flips `input_owner` on two attachments
        // while the count stands still.
        if self.attachments_dirty {
            self.attachments_dirty = false;
            let attachments: Vec<Attachment> = self
                .subs
                .iter()
                .filter(|sub| sub.is_window)
                .map(|sub| Attachment {
                    id: sub.id.0,
                    attached_at: sub.attached_at,
                    input_owner: self.input_owner == Some(sub.id),
                    carrier: Some(sub.carrier.clone()),
                })
                .collect();
            if meta.attachments != attachments {
                changed.push(SessionField::Attachments);
                meta.attachments = attachments;
            }
        }
        drop(meta);
        if !changed.is_empty() {
            self.observers.send(NotifyToClientMsg::SessionChanged {
                session_id: self.id.0,
                fields: changed,
            });
        }
    }

    /// A running shell is never reaped (`session-lifecycle.md`
    /// "Post-exit reaping").
    fn reap_due(&self) -> bool {
        if !self.child_exited || !self.subs.is_empty() {
            return false;
        }
        let idle_since = self
            .meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .idle_since;
        Instant::now().saturating_duration_since(idle_since) >= self.policy.post_exit_grace
    }

    fn drain_effects_under(&mut self, core: &mut ParseCore) -> Result<(), ConnError> {
        let drained = if self.decode.is_some() {
            streaming::Drained::Done(Ok(()))
        } else {
            streaming::drain_effects(&mut self.session, core)
        };
        self.settle_drain(core, drained)
    }

    fn settle_drain(
        &mut self,
        core: &ParseCore,
        drained: streaming::Drained,
    ) -> Result<(), ConnError> {
        let drained = match drained {
            streaming::Drained::Done(written) => written,
            streaming::Drained::Suspended(prepared) => {
                self.start_decode(prepared);
                Ok(())
            }
        };
        // The parse thread records a `DECSET 2048` set without writing
        // anything; the burst it rode in on is drained here, so the
        // reconcile owes the state a look. Judged under this guard: the
        // reconcile takes the lock again, a second wait per cycle.
        if resize_notify_key(&core.grid) != self.reported_resize {
            self.resize_notify_dirty = true;
        }
        self.queue_command_outputs();
        drained
    }

    fn queue_command_outputs(&mut self) {
        for msg in std::mem::take(&mut self.session.command_outputs) {
            let minor = msg.requires().minor;
            for sub in &mut self.subs {
                if !sub.announce_output || sub.minor < minor {
                    continue;
                }
                if sub.command_outputs.len() == ANNOUNCE_QUEUE_CAP {
                    sub.command_outputs.pop_front();
                }
                sub.command_outputs.push_back(msg.clone());
            }
        }
    }

    fn drain_and_fan(&mut self) -> Result<(), ConnError> {
        // Re-arm before draining: a chunk parsed mid-drain must
        // re-notify, or its effects strand until the next unrelated
        // wake.
        let parsed = self.session.signals.clear_dirty();
        let generation = self.session.signals.drain_generation();
        let drained = self.drive_cycle();
        if self.decode.is_none() {
            self.session.signals.publish_drained(generation);
            // The commit cleared the flag, so only the sink's parse after
            // the release sets it again.
            self.awaiting_post_release_drain &= !parsed;
        }
        drained
    }

    fn start_decode(&mut self, prepared: crate::graphics::PreparedDecode) {
        let (job, plan) = prepared.into_job();
        #[cfg(all(test, unix))]
        let before = self.before_decode.take();
        let payload_len = job.payload_len();
        let handle = tokio::task::spawn_blocking(move || {
            #[cfg(all(test, unix))]
            if let Some(before) = before {
                before();
            }
            job.run()
        });
        self.decode = Some(InFlightDecode {
            plan,
            handle,
            payload_len,
        });
    }

    const fn hold(&self) -> Hold {
        Hold {
            upgrade: self.upgrade_hold,
            decode: self.decode.is_some() || self.awaiting_post_release_drain,
        }
    }

    fn compact_slice(&mut self) {
        let shared = Arc::clone(&self.session.core);
        let mut core = shared.lock();
        let rows = idle::slice_rows(core.grid.scrollback().cols());
        let generation = self.session.signals.output_generation();
        self.idle
            .slice(generation, tokio::time::Instant::now(), || {
                core.grid.compact_history(rows)
            });
    }

    const fn owes_reports(&self) -> bool {
        self.focus_dirty || self.scheme_dirty || self.resize_notify_dirty
    }

    fn reconcile_reports(&mut self) -> Option<EndReason> {
        if !self.owes_reports() {
            return None;
        }
        if self.focus_dirty
            && let Some(reason) = self.sync_focus_to_pty()
        {
            return Some(reason);
        }
        if self.scheme_dirty
            && let Some(reason) = self.sync_color_scheme_to_pty()
        {
            return Some(reason);
        }
        if self.resize_notify_dirty {
            return self.sync_resize_notify_to_pty();
        }
        None
    }

    /// The decode's commit and the rest of the drain it stopped, under
    /// one lock, then the release of the sink waiting on it. A panicked
    /// decode answers as a failed one rather than ending the session.
    fn commit_decode(
        &mut self,
        decoded: Result<crate::graphics::Decoded, tokio::task::JoinError>,
    ) -> Result<Option<EndReason>, ConnError> {
        let Some(InFlightDecode { plan, .. }) = self.decode.take() else {
            return Ok(None);
        };
        let decoded = decoded
            .map_err(|err| error!(?err, "session task: Kitty graphics decode panicked"))
            .ok();
        let generation = self.session.signals.drain_generation();
        let core = Arc::clone(&self.session.core);
        let mut guard = core.lock();
        let resumed = streaming::resume_drain(&mut self.session, &mut guard, &plan, decoded);
        let drained = self.settle_drain(&guard, resumed);
        self.ship_cycle(guard);
        if self.decode.is_some() {
            return drained.map(|()| None);
        }
        // Before the release, or the bytes after the image parse at a
        // size a later resize trims on the alternate screen. The parse
        // stays stopped until the release, so the core lock is not held.
        let mut ended = None;
        while let Some(held) = self.held_cmds.pop_front() {
            ended = match held.into_inner() {
                // Admitted input outlives its window, as a keystroke
                // lost on a detach would be gone with no sign; a window
                // gone without a held detach was evicted mid-decode.
                SessionCmd::Input {
                    sub,
                    msg,
                    reservation,
                } if self.sub_index(sub).is_none() => self.handle_remote_input(msg, reservation),
                cmd => self.run_cmd(cmd),
            };
            if ended.is_some() {
                self.held_cmds.clear();
                break;
            }
        }
        self.session.signals.clear_dirty();
        self.awaiting_post_release_drain = true;
        self.session.signals.publish_drained(generation);
        drained.map(|()| ended)
    }

    /// The drain's error is returned after the cycle ships rather than
    /// before: at EOF the child's final frame must still reach the
    /// windows.
    fn drive_cycle(&mut self) -> Result<(), ConnError> {
        let core = Arc::clone(&self.session.core);
        let mut guard = core.lock();
        let drained = self.drain_effects_under(&mut guard);
        self.ship_cycle(guard);
        drained
    }

    fn ship_cycle(&mut self, mut core: parking_lot::MutexGuard<'_, ParseCore>) {
        let cycle = self.compose_cycle(&mut core);
        drop(core);
        self.finish_cycle(cycle);
        self.scan_watches();
    }

    /// Nothing here may push to a subscriber: an eviction's
    /// `refresh_meta` relocks the core, and the lock is not reentrant.
    fn compose_cycle(&mut self, core: &mut ParseCore) -> Cycle {
        self.fan_out_grid_state_under(core);
        let notifications = streaming::take_notifications(&mut core.grid);
        let (facets, clipboard) = self.collect_facets_under(core);
        let composed = self.compose_all(core);
        Cycle {
            meta: self.read_meta_facts(core),
            notifications,
            facets,
            clipboard,
            composed,
        }
    }

    fn finish_cycle(&mut self, cycle: Cycle) {
        // First: an eviction below republishes newer facts through
        // `refresh_meta`, which these must not overwrite.
        self.apply_meta(cycle.meta);
        self.push_image_events();
        // `attached` counts window subscribers only: a consumer uses it
        // to notify the sessions whose window did not flash, and an
        // `Ops` reader flashes nothing.
        let attached = self.subs.iter().any(|sub| sub.is_window);
        let latest = cycle.notifications.and_then(|taken| {
            streaming::publish_notifications(&self.hub, self.id, taken, attached)
        });
        if let Some(latest) = latest {
            {
                let mut meta = self
                    .meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                meta.last_notification = Some(latest);
            }
            self.broadcast(|sub| {
                if sub.is_window {
                    vec![OutEvent::Grid(GridMsg::Attention {
                        source: AttentionSource::Notification,
                    })]
                } else {
                    Vec::new()
                }
            });
        }
        self.push_facets(&cycle.facets, cycle.clipboard);
        self.push_composed(cycle.composed);
        self.publish_observed();
    }

    /// After the row they concern is updated, so a client that re-reads
    /// it on an item sees what the item reports.
    fn publish_observed(&mut self) {
        if self.observers.unheard() {
            self.session.observed.clear();
            return;
        }
        for observed in std::mem::take(&mut self.session.observed) {
            let session_id = self.id.0;
            self.observers.send(match observed {
                streaming::Observed::ProgramStatus(records) => NotifyToClientMsg::ProgramStatus {
                    session_id,
                    records,
                },
                streaming::Observed::PromptShown => NotifyToClientMsg::PromptShown { session_id },
                streaming::Observed::CommandFinished(exit_code) => {
                    NotifyToClientMsg::CommandFinished {
                        session_id,
                        exit_code,
                    }
                }
            });
        }
    }

    fn push_image_events(&mut self) {
        if self.session.image_events.is_empty() {
            return;
        }
        // Taken unconditionally (the queue is otherwise unbounded for a
        // parked session), materialized only for a window: a parked mpv
        // would otherwise pay a frame read per video frame for nobody.
        let events = std::mem::take(&mut self.session.image_events);
        // A batch without a region marker materializes the same for every
        // window, so only one with one splits the windows in two.
        let split = events
            .iter()
            .any(|e| matches!(e, crate::graphics::ImageEvent::TransmitRegion { .. }));
        let bucket = |sub: &Subscriber| usize::from(split && sub.reads_regions());
        let mut msgs: [Option<Vec<ImageMsg>>; 2] = [None, None];
        for sub in self.subs.iter().filter(|sub| sub.is_window) {
            msgs[bucket(sub)].get_or_insert_with(|| {
                crate::graphics::materialize_image_events(
                    &events,
                    &self.session.images,
                    sub.reads_regions(),
                )
            });
        }
        if msgs.iter().all(Option::is_none) {
            return;
        }
        self.broadcast(|sub| match (&msgs[bucket(sub)], sub.is_window) {
            (Some(msgs), true) => msgs.iter().cloned().map(OutEvent::Image).collect(),
            _ => Vec::new(),
        });
    }

    /// Drains the parse's effects first, as a drive cycle does: a scroll
    /// still queued in the grid would leave this subscriber's rows ahead
    /// of every directive it holds, and the compose would replay the
    /// whole grid to catch up.
    fn grid_cycle_for(&mut self, sub_id: SubscriberId) -> Result<(), ConnError> {
        let core = Arc::clone(&self.session.core);
        let mut guard = core.lock();
        let drained = self.drain_effects_under(&mut guard);
        self.fan_out_grid_state_under(&mut guard);
        // Reset even for one composition: an in-place edit moves neither
        // the geometry nor the scroll sequence, so `sync` would hand out a
        // row an earlier cycle cached.
        self.row_cache.begin_cycle(1);
        let composed = self
            .sub_index(sub_id)
            .and_then(|i| self.compose_at(&mut guard, i));
        let meta = self.read_meta_facts(&guard);
        drop(guard);
        self.apply_meta(meta);
        self.push_image_events();
        self.push_composed(composed.into_iter().collect());
        drained
    }

    /// Ungated by `?2026`: only daemon-side state moves here; the
    /// wire-facing compose stays gated.
    fn fan_out_grid_state_under(&mut self, core: &mut ParseCore) {
        // Scrolls before damage: a subscriber's owed rows are in the
        // coordinates of the grid before this drain's shifts, the grid's
        // damage in the coordinates after them.
        let (current_gen, rows) = (core.grid.geometry_gen(), core.grid.rows());
        for queued in std::mem::take(&mut self.session.scroll_ops) {
            for sub in &mut self.subs {
                sub.stream.offer_scroll(&queued, current_gen, rows);
            }
        }
        for sub in &mut self.subs {
            sub.stream.catch_up_to(&core.grid);
        }
        let grid = &mut core.grid;
        if grid.damage().dirty_rows().next().is_some() {
            for sub in &mut self.subs {
                sub.stream.damage.merge(grid.damage());
            }
            grid.damage_mut().clear();
        }
    }

    /// An `Ops` attach takes the facets without receiving them: a
    /// bell or clipboard write is a moment, and leaving it armed for a
    /// window attaching minutes later would ship a lie. The
    /// state-shaped facets come off the grid on that window's own
    /// rehydrate.
    fn collect_facets_under(&self, core: &mut ParseCore) -> (Vec<GridMsg>, ClipboardBatch) {
        if self.subs.is_empty() || !core.grid.ready_to_present(Instant::now()) {
            return (Vec::new(), ClipboardBatch::default());
        }
        let writes = core.grid.take_pending_clipboard_sets();
        let target = if writes.is_empty() {
            None
        } else {
            self.clipboard_target()
        };
        (
            streaming::collect_facets(&mut core.grid),
            ClipboardBatch { target, writes },
        )
    }

    fn push_facets(&mut self, facets: &[GridMsg], clipboard: ClipboardBatch) {
        if !facets.is_empty() {
            self.broadcast(|sub| {
                if sub.is_window {
                    facets.iter().cloned().map(OutEvent::Grid).collect()
                } else {
                    Vec::new()
                }
            });
        }
        if clipboard.writes.is_empty() {
            return;
        }
        let Some(target) = clipboard.target else {
            debug!(
                id = ?self.id,
                "dropping OSC 52 write: no attributable window subscriber"
            );
            return;
        };
        let Some(i) = self.sub_index(target) else {
            return;
        };
        for write in clipboard.writes {
            if !self.push_to(target, i, OutEvent::Grid(GridMsg::ClipboardSet { write })) {
                debug!(id = ?self.id, sub = target.0, "OSC 52 target outbox died");
                return;
            }
        }
    }

    /// Per-client clipboard scope (security-model.md): with
    /// `clipboard.osc_52 = "system"` a broadcast would overwrite every
    /// mirror host's clipboard. The active subscriber is the best
    /// attribution; a sole window is unambiguous; otherwise the write
    /// is dropped rather than guessed.
    fn clipboard_target(&self) -> Option<SubscriberId> {
        self.active_sub
            .filter(|&id| self.subs.iter().any(|s| s.id == id && s.is_window))
            .or_else(|| {
                let mut windows = self.subs.iter().filter(|s| s.is_window);
                match (windows.next(), windows.next()) {
                    (Some(only), None) => Some(only.id),
                    _ => None,
                }
            })
    }

    fn broadcast(&mut self, f: impl Fn(&Subscriber) -> Vec<OutEvent>) {
        let mut dead: Vec<SubscriberId> = Vec::new();
        for sub in &self.subs {
            for ev in f(sub) {
                if !sub.push(ev) {
                    dead.push(sub.id);
                    break;
                }
            }
        }
        for id in dead {
            self.remove_sub(id);
        }
    }

    /// The row cache spans the loop, so mirrors owed the same row share
    /// one encode of it.
    fn compose_all(&mut self, core: &mut ParseCore) -> Vec<Composed> {
        self.row_cache.begin_cycle(self.subs.len());
        (0..self.subs.len())
            .rev()
            .filter_map(|i| self.compose_at(core, i))
            .collect()
    }

    fn compose_at(&mut self, core: &mut ParseCore, i: usize) -> Option<Composed> {
        let sub = &mut self.subs[i];
        if sub.pull_pacing && !sub.pull_pending {
            return None;
        }
        let mut out = Vec::new();
        let composed = streaming::compose_diffs(
            &mut core.grid,
            &mut sub.stream,
            &mut self.row_cache,
            &mut out,
        )
        .map(|()| {
            out.extend(sub.command_outputs.drain(..).map(OutEvent::Grid));
            out
        });
        Some((sub.id, composed))
    }

    /// Demand-driven pacing (docs/explanation/rendering/pipeline.md): a
    /// clean pull emits nothing and stays armed.
    fn push_composed(&mut self, composed: Vec<Composed>) {
        for (id, result) in composed {
            let Some(i) = self.sub_index(id) else {
                continue;
            };
            let mut out = match result {
                Ok(out) => out,
                Err(err) => {
                    warn!(?err, sub = id.0, "diff compose failed; evicting subscriber");
                    self.remove_sub(id);
                    continue;
                }
            };
            if out.is_empty() {
                continue;
            }
            let sub = &mut self.subs[i];
            trace!(
                target: "felis_daemon::serve",
                sub = id.0,
                events = out.len(),
                "ship diffs",
            );
            sub.pull_pending = false;
            push_cycle_end(&mut out, sub.pull_pacing);
            let alive = out.into_iter().all(|ev| sub.push(ev));
            if !alive {
                self.remove_sub(id);
            }
        }
    }

    #[cfg(all(test, unix))]
    fn drain_effects(&mut self) -> Result<(), ConnError> {
        let core = Arc::clone(&self.session.core);
        self.drain_effects_under(&mut core.lock())
    }

    #[cfg(all(test, unix))]
    fn after_drive(&mut self) {
        let core = Arc::clone(&self.session.core);
        let cycle = self.compose_cycle(&mut core.lock());
        self.finish_cycle(cycle);
    }

    #[cfg(all(test, unix))]
    fn fan_out_grid_state(&mut self) {
        let core = Arc::clone(&self.session.core);
        self.fan_out_grid_state_under(&mut core.lock());
    }

    #[cfg(all(test, unix))]
    fn broadcast_facets(&mut self) {
        let (facets, clipboard) = self.collect_facets_under(&mut self.session.lock_core());
        self.push_facets(&facets, clipboard);
    }

    #[cfg(all(test, unix))]
    fn ship_all(&mut self) {
        let core = Arc::clone(&self.session.core);
        let composed = self.compose_all(&mut core.lock());
        self.push_composed(composed);
    }

    #[cfg(all(test, unix))]
    fn ship_sub_at(&mut self, i: usize) {
        let core = Arc::clone(&self.session.core);
        let composed = self.compose_at(&mut core.lock(), i);
        self.push_composed(composed.into_iter().collect());
    }

    /// Drive one input message without budget reservation for direct actor tests.
    ///
    /// Gated `unix` and `test` because in-process tests calling this are Unix-only,
    /// avoiding `dead_code` errors on Windows test targets.
    #[cfg(all(test, unix))]
    fn handle_input(&mut self, sub_id: SubscriberId, msg: InputMsg) -> Option<EndReason> {
        self.handle_admitted_input(sub_id, msg, None)
    }

    fn input_promotes(&self, msg: &InputMsg) -> bool {
        match msg {
            InputMsg::KeyBytes(_) | InputMsg::Paste(_) => true,
            InputMsg::Mouse(_) | InputMsg::Key(_) => {
                encode_input(&self.session.lock_core(), msg).is_some()
            }
            _ => false,
        }
    }

    fn handle_admitted_input(
        &mut self,
        sub_id: SubscriberId,
        msg: InputMsg,
        reservation: Option<InputReservation>,
    ) -> Option<EndReason> {
        let promotes = self.input_promotes(&msg);
        self.input_with(sub_id, msg, reservation, promotes)
    }

    /// `promotes` is [`Self::input_promotes`] for `msg`, read under the
    /// same modes.
    fn input_with(
        &mut self,
        sub_id: SubscriberId,
        msg: InputMsg,
        reservation: Option<InputReservation>,
        promotes: bool,
    ) -> Option<EndReason> {
        let i = self.sub_index(sub_id)?;
        // Active-owns-winsize (session-lifecycle.md "Same-user mirroring"): size
        // ownership follows input reaching the PTY. Mouse motion promotes only if it
        // encodes to bytes, preventing passive mirror hovers from stealing size.
        // Promoted before dispatch so new winsize applies before bytes reach shell.
        if promotes {
            // Only a window's input moves the switch-target marker: an
            // `Ops` sender has no window to move.
            if self.subs[i].is_window && self.input_owner != Some(sub_id) {
                self.input_owner = Some(sub_id);
                self.attachments_dirty = true;
                // Presentation before size ownership before the bytes:
                // the resize's `SIGWINCH` and the keystroke can each
                // make the child query colors before this event
                // finishes, and a later recompute would answer with the
                // window the user just left.
                if let Some(reason) = self.recompute_presentation_now() {
                    return Some(reason);
                }
            }
            self.promote_active(sub_id, i);
            // The reconcile at the bottom of the run loop would land
            // after the bytes below, and a `?2048` program is promised
            // geometry ordered against the input around it
            // (docs/explanation/protocols/landscape.md, "CSI").
            if self.resize_notify_dirty
                && let Some(reason) = self.sync_resize_notify_to_pty()
            {
                return Some(reason);
            }
        }
        let session = &mut self.session;
        match msg {
            InputMsg::KeyBytes(bytes) => {
                if let Some(reason) = pty_write_outcome(streaming::write_pty_reserved(
                    &session.writer,
                    bytes,
                    reservation,
                )) {
                    return Some(reason);
                }
            }
            InputMsg::Paste(bytes) => {
                let bracketed = session.lock_core().grid.bracketed_paste();
                if let Some(reason) =
                    pty_write_outcome(write_paste(&session.writer, bracketed, bytes, reservation))
                {
                    return Some(reason);
                }
            }
            InputMsg::Resize { dims } => {
                if !self.apply_resize_input(sub_id, i, dims) {
                    return None;
                }
            }
            InputMsg::FocusChange { focused } => {
                self.subs[i].focused = focused;
                if let Some(reason) = self.sync_focus_to_pty() {
                    return Some(reason);
                }
            }
            InputMsg::ColorScheme { dark } => {
                self.subs[i].presentation.os_dark = Some(dark);
                if let Some(reason) = self.recompute_presentation_now() {
                    return Some(reason);
                }
            }
            InputMsg::Mouse(_) | InputMsg::Key(_) => {
                // Encoded again rather than reusing the promote check's
                // bytes: promotion can SIGWINCH the child or send it a
                // resize report, and its answer may change the modes
                // these bytes must follow.
                let encoded = encode_input(&session.lock_core(), &msg);
                // The reservation covers `MAX_MOUSE_REPORT_BYTES` /
                // `MAX_KEY_REPORT_BYTES` whatever the active mode produced,
                // so the write is admitted rather than dropped; an event
                // with no encoding releases it here instead.
                if let Some(bytes) = encoded
                    && let Some(reason) = pty_write_outcome(streaming::write_pty_reserved(
                        &session.writer,
                        bytes,
                        reservation,
                    ))
                {
                    return Some(reason);
                }
            }
            InputMsg::Viewport {
                lines_from_bottom,
                anchor,
            } => {
                let stream = &mut self.subs[i].stream;
                let moved = stream.diff.request_viewport(
                    &session.lock_core().grid,
                    lines_from_bottom,
                    anchor,
                );
                if moved {
                    stream.damage.mark_all();
                }
            }
            InputMsg::ViewportStep { rows } => {
                let stream = &mut self.subs[i].stream;
                let moved = stream.diff.step_viewport(&session.lock_core().grid, rows);
                if moved {
                    stream.damage.mark_all();
                }
            }
            InputMsg::JumpPrompt { direction } => {
                let stream = &mut self.subs[i].stream;
                let target = {
                    let core = session.lock_core();
                    stream.diff.rebase_viewport(&core.grid);
                    core.grid
                        .prompt_jump_target(stream.diff.viewport, direction)
                };
                if let Some(target) = target {
                    stream.diff.viewport = target;
                    stream.damage.mark_all();
                }
            }
            InputMsg::NextGridFrame => {
                self.subs[i].pull_pending = true;
                trace!(target: "felis_daemon::serve", "grid frame pull");
            }
            InputMsg::AnnounceOutput { enabled } => {
                // Pending output is handed out before the flag changes,
                // so a window opting in never receives output whose `D`
                // preceded its request.
                let core = Arc::clone(&self.session.core);
                let mut guard = core.lock();
                let drained = self.drain_effects_under(&mut guard);
                let sub = &mut self.subs[i];
                sub.announce_output = enabled;
                if !enabled {
                    sub.command_outputs.clear();
                }
                guard
                    .grid
                    .set_capture_command_output(self.announces_output());
                drop(guard);
                if let Some(reason) = pty_write_outcome(drained) {
                    return Some(reason);
                }
            }
        }
        // Ship after every input, not only on the PTY arm: a Viewport
        // request into a quiet prompt would otherwise sit unrendered
        // until the user types.
        pty_write_outcome(self.grid_cycle_for(sub_id))
    }

    /// `Ops::Send`'s checks against live state, before the connection
    /// writes a step: a refusal here leaves the PTY untouched.
    fn prepare_send(
        &mut self,
        watch: Option<(RequesterId, StreamId)>,
        mouse: &[(u32, MouseEvent)],
    ) -> Prepared {
        if self.program_gone() {
            return Prepared::Ended;
        }
        {
            let core = self.session.lock_core();
            let (rows, cols) = (core.grid.rows(), core.grid.cols());
            let protocol = core.grid.mouse_protocol();
            for &(step, event) in mouse {
                if let Some(detail) = unreported_mouse(event, protocol, rows, cols) {
                    return Prepared::Unreported { step, detail };
                }
            }
        }
        if let Some((id, stream)) = watch {
            let from = streaming::prompt_marks_ordinal_end(&self.session.lock_core().grid);
            let Some(i) = self.requester_index(id) else {
                return Prepared::Ended;
            };
            self.requesters[i].watches.push(MarkWatch {
                stream,
                from,
                released: false,
                held: None,
            });
        }
        Prepared::Ready
    }

    /// One admitted `Ops::Send` step. Unlike a subscriber's input it
    /// moves neither size ownership nor the input owner: an unattached
    /// connection has no window to size or switch to.
    fn handle_remote_input(
        &mut self,
        msg: InputMsg,
        reservation: Option<InputReservation>,
    ) -> Option<EndReason> {
        let session = &mut self.session;
        let written = match msg {
            InputMsg::KeyBytes(bytes) => {
                streaming::write_pty_reserved(&session.writer, bytes, reservation)
            }
            InputMsg::Paste(bytes) => {
                let bracketed = session.lock_core().grid.bracketed_paste();
                write_paste(&session.writer, bracketed, bytes, reservation)
            }
            msg @ (InputMsg::Mouse(_) | InputMsg::Key(_)) => {
                let encoded = encode_input(&session.lock_core(), &msg);
                encoded.map_or(Ok(()), |bytes| {
                    streaming::write_pty_reserved(&session.writer, bytes, reservation)
                })
            }
            _ => Ok(()),
        };
        pty_write_outcome(written)
    }

    /// `false` once the program has exited, whose watch the exit already
    /// ended. Releasing a watch queues its `WaitSent` here, ahead of the
    /// mark any later scan finds.
    fn send_fence(&mut self, watch: Option<(RequesterId, StreamId)>, id: u128) -> bool {
        if self.pty_eof || self.child_exited {
            return false;
        }
        let Some((conn, stream)) = watch else {
            return true;
        };
        self.scan_watches();
        let Some(i) = self.requester_index(conn) else {
            return true;
        };
        let Some(w) = self.requesters[i]
            .watches
            .iter()
            .position(|w| w.stream == stream)
        else {
            return true;
        };
        let correlation = Correlation::stream(stream);
        let sent = OutEvent::Ops {
            msg: Box::new(OpsToClientMsg::WaitSent {
                outcome: SendOutcome::Sent { id },
            }),
            correlation,
        };
        if !self.push_owner(Owner::Conn(conn), sent) {
            return true;
        }
        let Some(i) = self.requester_index(conn) else {
            return true;
        };
        let watch = &mut self.requesters[i].watches[w];
        watch.released = true;
        if let Some(mark) = watch.held {
            self.finish_watch(conn, stream, mark.exit_code);
        }
        true
    }

    /// Finds each watch's first `OSC 133 ; D` since it registered. A
    /// released watch ships it at once; an unreleased one holds it for
    /// the fence.
    fn scan_watches(&mut self) {
        if self.requesters.iter().all(|r| r.watches.is_empty()) {
            return;
        }
        let mut found = Vec::new();
        {
            let core = self.session.lock_core();
            let marks = core.grid.prompt_marks();
            let pruned = usize::try_from(core.grid.prompt_marks_pruned()).unwrap_or(usize::MAX);
            let end = pruned.saturating_add(marks.len());
            for requester in &mut self.requesters {
                for watch in &mut requester.watches {
                    if watch.held.is_some() {
                        continue;
                    }
                    let start = watch.from.max(pruned);
                    let hit = marks
                        .get(start - pruned..)
                        .unwrap_or_default()
                        .iter()
                        .find(|m| m.kind == PromptKind::CommandEnd);
                    watch.from = end;
                    if let Some(mark) = hit {
                        watch.held = Some(HeldMark {
                            exit_code: mark.exit_code,
                        });
                        if watch.released {
                            found.push((requester.id, watch.stream, mark.exit_code));
                        }
                    }
                }
            }
        }
        for (conn, stream, exit_code) in found {
            self.finish_watch(conn, stream, exit_code);
        }
    }

    fn finish_watch(&mut self, conn: RequesterId, stream: StreamId, exit_code: Option<u32>) {
        let Some(i) = self.requester_index(conn) else {
            return;
        };
        self.requesters[i].watches.retain(|w| w.stream != stream);
        let mark = OutEvent::Ops {
            msg: Box::new(OpsToClientMsg::Mark { exit_code }),
            correlation: Correlation::stream(stream),
        };
        if self.push_owner(Owner::Conn(conn), mark) {
            self.push_terminal(
                Owner::Conn(conn),
                ConnToClientMsg::End {
                    stream_id: stream,
                    count: 1,
                },
            );
        }
    }

    /// `true` when `stream` was a watch: it ends with no mark.
    fn cancel_watch(&mut self, conn: RequesterId, stream: StreamId) -> bool {
        let Some(i) = self.requester_index(conn) else {
            return false;
        };
        let before = self.requesters[i].watches.len();
        self.requesters[i].watches.retain(|w| w.stream != stream);
        if self.requesters[i].watches.len() == before {
            return false;
        }
        self.push_terminal(
            Owner::Conn(conn),
            ConnToClientMsg::End {
                stream_id: stream,
                count: 0,
            },
        );
        true
    }

    /// Nothing written now reaches the program, though output it wrote
    /// before exiting may still end a wait admitted earlier.
    const fn program_gone(&self) -> bool {
        self.pty_eof || self.child_exited || self.reaped_exit.is_some()
    }

    fn poll_child_exit(&mut self) {
        if self.child_exited {
            return;
        }
        let status = self.reaped_exit.take().or_else(|| {
            let reaped = self.session.child.try_wait().ok().flatten();
            reaped.map(|status| ShellExit(exit_status_to_wire(status)))
        });
        let Some(ShellExit(status)) = status else {
            return;
        };
        // Probed after the reap, so it covers everything the child wrote,
        // which the sink may not have read yet.
        if !self.pty_eof && !self.session.resizer.output_drained() {
            self.reaped_exit = Some(ShellExit(status));
            return;
        }
        self.on_child_exit(status);
    }

    /// Seen before EOF when a process the program left behind holds the
    /// PTY open: no mark it owes a wait can come any more once the output
    /// it wrote before exiting is parsed, which the caller has seen to. That output can stop on an image
    /// whose decode this drain starts, and the exit then waits for it.
    fn on_child_exit(&mut self, status: Option<ExitStatus>) {
        self.stamp_exited_meta();
        if let Err(err) = self.drive_cycle() {
            debug!(?err, "session task: effect drain at child exit failed");
        }
        if self.decode.is_some() {
            self.reaped_exit = Some(ShellExit(status));
            return;
        }
        self.child_exited = true;
        self.observers.exited(status);
        self.end_watches();
    }

    /// At exit: no `OSC 133 ; D` can arrive any more. Searches and
    /// captures keep reading the final screen.
    fn end_watches(&mut self) {
        for requester in &mut self.requesters {
            for watch in std::mem::take(&mut requester.watches) {
                let _queued = requester.force_push(OutEvent::Control(ConnToClientMsg::Error {
                    subject: Subject::Stream(watch.stream),
                    reason: StreamErrorReason::Unavailable,
                    detail: "the session ended".to_owned(),
                }));
            }
        }
    }

    fn close_requesters(&mut self, detail: &str) {
        for mut requester in self.requesters.drain(..) {
            requester.close_all(detail);
        }
        self.pending_chunks
            .retain(|(owner, _)| matches!(owner, Owner::Sub(_)));
    }

    fn handle_search(
        &mut self,
        owner: Owner,
        msg: SearchToDaemonMsg,
        stream: StreamId,
    ) -> Option<EndReason> {
        self.producers_mut(owner)?;
        let SearchToDaemonMsg::Query {
            query,
            options,
            source,
            session: _,
        } = msg;
        let compiled = match felis_grid::SearchQuery::new(&query, options) {
            Ok(q) => q,
            Err(err) => {
                // The stream is already open on the wire, so it owes
                // its one terminal even now.
                self.push_terminal(
                    owner,
                    ConnToClientMsg::Error {
                        subject: Subject::Stream(stream),
                        reason: StreamErrorReason::InvalidRequest,
                        detail: err.to_string(),
                    },
                );
                return None;
            }
        };
        if source == felis_grid::SearchSource::Visible {
            self.producers_mut(owner)?.push(Producer {
                stream,
                canceled: false,
                emitted: 0,
                kind: ProducerKind::VisibleSearch {
                    query: compiled,
                    row: 0,
                },
            });
            self.pending_chunks.push_back((owner, stream));
            return None;
        }
        // `lock_core` is not reentrant; one guard for both reads.
        let total = {
            let core = self.session.lock_core();
            core.grid.scrollback().len() + usize::from(core.grid.rows())
        };
        let budget = SEARCH_CHUNK_LINES.max(total.div_ceil(MAX_SEARCH_CHUNKS));
        self.producers_mut(owner)?.push(Producer {
            stream,
            canceled: false,
            emitted: 0,
            kind: ProducerKind::Search {
                query: compiled,
                cursor: felis_grid::SearchCursor::default(),
                budget,
            },
        });
        self.pending_chunks.push_back((owner, stream));
        None
    }

    /// An unresolved source (no closed `OSC 133` range yet) ends an
    /// empty stream rather than going silent.
    fn start_rows(
        &mut self,
        owner: Owner,
        source: RegionSource,
        ansi: bool,
        max_rows: Option<u32>,
        stream: StreamId,
    ) {
        let Some(producers) = self.producers_mut(owner) else {
            return;
        };
        producers.push(Producer {
            stream,
            canceled: false,
            emitted: 0,
            kind: ProducerKind::Rows {
                source,
                ansi,
                max_rows,
                offset: 0,
            },
        });
        self.pending_chunks.push_back((owner, stream));
    }

    /// A canceled producer stays queued for one more turn: it still
    /// owes the stream its terminal.
    fn cancel_producer(&mut self, owner: Owner, stream: StreamId) {
        if let Owner::Conn(id) = owner
            && self.cancel_watch(id, stream)
        {
            return;
        }
        let Some(producer) = self
            .producers_mut(owner)
            .and_then(|producers| producers.iter_mut().find(|p| p.stream == stream))
        else {
            // The terminal was already on the wire; the connection
            // driver treats this cancel as idempotent.
            return;
        };
        producer.canceled = true;
        if !self.pending_chunks.contains(&(owner, stream)) {
            self.pending_chunks.push_back((owner, stream));
        }
    }

    /// One slice per run-loop turn, never looped: a cancel, a
    /// keystroke, and the PTY drain get a turn between slices.
    fn run_next_chunk(&mut self) {
        let Some((owner, stream)) = self.pending_chunks.pop_front() else {
            return;
        };
        self.run_chunk(owner, stream);
    }

    fn has_pending_chunks(&self) -> bool {
        !self.pending_chunks.is_empty()
    }

    fn producer_index(&mut self, owner: Owner, stream: StreamId) -> Option<usize> {
        self.producers_mut(owner)?
            .iter()
            .position(|p| p.stream == stream)
    }

    fn run_chunk(&mut self, owner: Owner, stream: StreamId) {
        let Some(p) = self.producer_index(owner, stream) else {
            return;
        };
        if self.producers_mut(owner).is_some_and(|ps| ps[p].canceled) {
            self.close_stream(owner, p, None);
            return;
        }
        let (events, done, failure) = self.next_slice(owner, p);
        for ev in events {
            if !self.push_owner(owner, ev) {
                return;
            }
        }
        // `push_owner` may have evicted the owner.
        let Some(p) = self.producer_index(owner, stream) else {
            return;
        };
        if done || failure.is_some() {
            self.close_stream(owner, p, failure);
        } else {
            self.pending_chunks.push_back((owner, stream));
        }
    }

    /// Returns the events, whether the walk finished, and a failure
    /// that terminates the stream instead of a clean end.
    #[allow(clippy::type_complexity)]
    fn next_slice(
        &mut self,
        owner: Owner,
        p: usize,
    ) -> (Vec<OutEvent>, bool, Option<(StreamErrorReason, String)>) {
        let viewport = self.viewport_of(owner);
        let session = &self.session;
        let producers = match owner {
            Owner::Sub(id) => self
                .subs
                .iter_mut()
                .find(|s| s.id == id)
                .map(|s| &mut s.producers),
            Owner::Conn(id) => self
                .requesters
                .iter_mut()
                .find(|r| r.id == id)
                .map(|r| &mut r.producers),
        };
        let Some(producer) = producers.and_then(|ps| ps.get_mut(p)) else {
            return (Vec::new(), true, None);
        };
        let correlation = Correlation::stream(producer.stream);
        let mut out = Vec::new();
        let (done, results) = match &mut producer.kind {
            ProducerKind::Search {
                query,
                cursor,
                budget,
            } => {
                let core = session.lock_core();
                let anchor = core.grid.viewport_anchor();
                let (advanced, walk) = core.grid.search_window(query, *cursor, *budget);
                let mut emitted = 0u32;
                let mut stopped_at = None;
                for (ordinal, hit) in walk {
                    out.push(OutEvent::Search {
                        msg: SearchToClientMsg::Match {
                            line_index: hit.line_index,
                            text: hit.text,
                            byte_spans: hit.byte_spans,
                            col_spans: hit.col_spans,
                            anchor: Some(anchor),
                        },
                        correlation,
                    });
                    emitted += 1;
                    if emitted >= SEARCH_CHUNK_ITEMS {
                        stopped_at = Some(ordinal);
                        break;
                    }
                }
                // A slice cut short by the item cap resumes past the hit
                // it stopped on; a hit count could not say that, since
                // the lines between hits are scanned and never reported.
                *cursor = felis_grid::SearchCursor {
                    visited: match stopped_at {
                        Some(ordinal) => ordinal + 1,
                        None => advanced
                            .visited
                            .saturating_add(*budget)
                            .min(advanced.seen()),
                    },
                    seen_total: advanced.seen_total,
                };
                (cursor.visited >= advanced.seen(), emitted)
            }
            ProducerKind::VisibleSearch { query, row } => {
                let core = session.lock_core();
                let anchor = core.grid.viewport_anchor();
                let (resume, walk) =
                    core.grid
                        .search_visible_window(query, *row, SEARCH_CHUNK_LINES);
                let mut emitted = 0u32;
                let mut stopped_at = None;
                for (next_row, hit) in walk {
                    out.push(OutEvent::Search {
                        msg: SearchToClientMsg::Match {
                            line_index: hit.line_index,
                            text: hit.text,
                            byte_spans: hit.byte_spans,
                            col_spans: hit.col_spans,
                            anchor: Some(anchor),
                        },
                        correlation,
                    });
                    emitted += 1;
                    if emitted >= SEARCH_CHUNK_ITEMS {
                        stopped_at = Some(next_row);
                        break;
                    }
                }
                *row = stopped_at.unwrap_or(resume);
                (*row >= usize::from(core.grid.rows()), emitted)
            }
            ProducerKind::Rows {
                source,
                ansi,
                max_rows,
                offset,
            } => {
                let core = session.lock_core();
                let rows = crate::serve::region::region_rows_window(
                    &core.grid, viewport, *source, *ansi, *max_rows, *offset, ROWS_CHUNK,
                );
                let resolved = rows.is_some();
                let shipped = rows.as_ref().map_or(0, Vec::len);
                for row in rows.into_iter().flatten() {
                    out.push(OutEvent::Region {
                        msg: RegionToClientMsg::Row {
                            row: row.row,
                            text: row.text,
                            ansi: row.ansi,
                            soft_wrap_continued: row.soft_wrap_continued,
                        },
                        correlation,
                    });
                }
                *offset += shipped;
                let done = shipped < ROWS_CHUNK;
                if done {
                    out.push(OutEvent::Region {
                        msg: RegionToClientMsg::RowsDone {
                            exit_code: resolved.then(|| core.grid.last_command_exit()).flatten(),
                        },
                        correlation,
                    });
                }
                (done, u32::try_from(shipped).unwrap_or(u32::MAX))
            }
        };
        producer.emitted = producer.emitted.saturating_add(results);
        (out, done, None)
    }

    fn close_stream(
        &mut self,
        owner: Owner,
        p: usize,
        failure: Option<(StreamErrorReason, String)>,
    ) {
        let Some(producers) = self.producers_mut(owner) else {
            return;
        };
        let producer = producers.remove(p);
        let terminal = match failure {
            Some((reason, detail)) => ConnToClientMsg::Error {
                subject: Subject::Stream(producer.stream),
                reason,
                detail,
            },
            None => ConnToClientMsg::End {
                stream_id: producer.stream,
                count: producer.emitted,
            },
        };
        self.push_terminal(owner, terminal);
    }

    fn promote_active(&mut self, sub_id: SubscriberId, i: usize) {
        if self.active_sub == Some(sub_id) {
            return;
        }
        self.active_sub = Some(sub_id);
        let Some(size) = self.subs[i].desired_size else {
            return;
        };
        if self.size_differs(size) {
            debug!(
                sub = sub_id.0,
                rows = size.rows,
                cols = size.cols,
                "size ownership transferred on input"
            );
            self.apply_size(size);
            return;
        }
        // The new owner's pixels can fill to the PTY's current extents
        // while what it supplied (an unknown axis, say) differs: the
        // announcement follows the owner without a PTY resize.
        let supplied = (size.pixel_width, size.pixel_height);
        {
            let mut meta = self
                .meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if (meta.pixel_w, meta.pixel_h) == supplied {
                return;
            }
            (meta.pixel_w, meta.pixel_h) = supplied;
        }
        self.broadcast(|_sub| {
            vec![OutEvent::Grid(GridMsg::Size {
                dims: GridDims {
                    rows: size.rows,
                    cols: size.cols,
                    pixel_w: size.pixel_width,
                    pixel_h: size.pixel_height,
                },
            })]
        });
    }

    /// `false` when the refused requester's notice could not be queued
    /// and the subscriber was dropped.
    fn apply_resize_input(&mut self, sub_id: SubscriberId, i: usize, dims: RequestedDims) -> bool {
        // Clamp, never refuse (REQ-605a); only the clamped tuple
        // is stored, so no surface can report a geometry the
        // session never had.
        let dims = dims.clamp();
        let size = PtySize {
            rows: dims.rows,
            cols: dims.cols,
            pixel_width: dims.pixel_w,
            pixel_height: dims.pixel_h,
        };
        self.subs[i].desired_size = Some(size);
        if self.active_sub.is_none() || self.active_sub == Some(sub_id) {
            // Pairs with the client's `reflow` debug line.
            debug!(
                rows = dims.rows,
                cols = dims.cols,
                pixel_w = dims.pixel_w,
                pixel_h = dims.pixel_h,
                "applying InputMsg::Resize to pty"
            );
            self.apply_size(size);
        } else {
            // The mirror's shadow already resized optimistically
            // client-side; the authoritative dims plus a full
            // replay let it letterbox the difference.
            debug!(
                rows = dims.rows,
                cols = dims.cols,
                "resize ignored — another subscriber owns the PTY size"
            );
            let (cur_rows, cur_cols) = {
                let core = self.session.lock_core();
                (core.grid.rows(), core.grid.cols())
            };
            let (pixel_w, pixel_h) = {
                let meta = self
                    .meta
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (meta.pixel_w, meta.pixel_h)
            };
            if !self.push_to(
                sub_id,
                i,
                OutEvent::Grid(GridMsg::Size {
                    dims: GridDims {
                        rows: cur_rows,
                        cols: cur_cols,
                        pixel_w,
                        pixel_h,
                    },
                }),
            ) {
                return false;
            }
            self.subs[i].stream.damage.mark_all();
        }
        true
    }

    /// Announced to every subscriber, the requester included: its
    /// client treats a same-dims resize as a no-op.
    /// A pixel axis the resize leaves unknown keeps the current cell:
    /// the nominal one would re-resolve every natural image extent for
    /// a window that only stopped knowing its pixels.
    fn program_size(&self, size: PtySize) -> PtySize {
        crate::program_size(size, (self.session.cell_pixel_w, self.session.cell_pixel_h))
    }

    fn size_differs(&self, size: PtySize) -> bool {
        let program = self.program_size(size);
        let core = self.session.lock_core();
        size.rows != core.grid.rows()
            || size.cols != core.grid.cols()
            || (program.pixel_width, program.pixel_height) != core.grid.winsize_pixels()
    }

    fn apply_size(&mut self, size: PtySize) {
        debug_assert!(
            self.decode.is_none(),
            "a resize during a decode is held until its commit"
        );
        let program = self.program_size(size);
        // Floor division matches how producers derive cell size from
        // TIOCGWINSZ.
        let cell_px = (
            program.pixel_width.checked_div(program.cols).unwrap_or(0),
            program.pixel_height.checked_div(program.rows).unwrap_or(0),
        );
        let cell_px_changed = cell_px != (self.session.cell_pixel_w, self.session.cell_pixel_h);
        (self.session.cell_pixel_w, self.session.cell_pixel_h) = cell_px;
        {
            // `refresh_meta` samples the grid, whose pixels are the
            // filled ones, not the supplied ones the roster states.
            let mut meta = self
                .meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            meta.pixel_w = size.pixel_width;
            meta.pixel_h = size.pixel_height;
        }
        let shared = Arc::clone(&self.session.core);
        let mut core = shared.lock();
        let pixels = (program.pixel_width, program.pixel_height);
        if (core.grid.rows(), core.grid.cols()) != (size.rows, size.cols)
            || pixels != core.grid.winsize_pixels()
        {
            self.resize_notify_dirty = true;
        }
        core.grid.set_winsize_pixels(pixels);
        // The primary screen reflows (REQ-604); the alternate screen
        // trims/pads, its rows being a distinct surface rather than a
        // continuation of the shared scrollback.
        let remap = if core.grid.on_alternate_screen() {
            core.grid.resize(size.rows, size.cols);
            None
        } else {
            core.grid.reflow(size.rows, size.cols)
        };
        self.idle.invalidate();
        // Every queued directive names the geometry it was recorded
        // under, so the new dimensions retire them all; the reflow has
        // marked every row dirty, which is the replay that replaces them.
        for sub in &mut self.subs {
            sub.stream.retire_scrolls(size.rows);
        }
        // Before the remap, whose full re-statement then carries the
        // new extents.
        if cell_px_changed {
            crate::graphics::rescale_placements(
                &self.session.images,
                &mut self.session.placements,
                &mut self.session.saved_screens,
                &mut self.session.image_events,
                cell_px,
            );
        }
        // Not `PlacementsShifted`: each anchor moves by its own delta
        // under a reflow, so survivors are re-stated in full (REQ-604).
        if let Some(remap) = remap {
            crate::graphics::apply_reflow_remap(
                &remap,
                &mut self.session.images,
                &mut self.session.placements,
                &mut self.session.image_events,
            );
        }
        // Before the replay is composed: the reflow's damage is that
        // replay, and leaving it armed would make the caller's own
        // fan-out re-arm every subscriber for a second one.
        self.fan_out_grid_state_under(&mut core);
        let composed = self.compose_all(&mut core);
        let meta = self.read_meta_facts(&core);
        drop(core);
        // SIGWINCH only once the grid has its new geometry: a repaint
        // parsed before the reflow would be re-wrapped, or blanked with
        // the prompt it replaces. Not under the core lock: ConPTY's resize
        // can block on its output pipe, which drains only through the
        // parser that lock serializes.
        #[cfg(all(test, unix))]
        {
            self.pty_steps.push(PtyStep::Resize);
            let core = self.session.lock_core();
            self.pty_resized_over
                .push((core.grid.rows(), core.grid.cols()));
        }
        if let Err(err) = self.session.resizer.resize(program) {
            warn!(?err, "pty resize failed");
        }
        self.apply_meta(meta);
        self.broadcast(|_sub| {
            vec![OutEvent::Grid(GridMsg::Size {
                dims: GridDims {
                    rows: size.rows,
                    cols: size.cols,
                    pixel_w: size.pixel_width,
                    pixel_h: size.pixel_height,
                },
            })]
        });
        // Otherwise only a PTY drive or a grid cycle ships the rescale's
        // and the remap's re-statements, and an idle shell has neither.
        self.push_image_events();
        // After the announcement, so the replay follows the dimensions
        // it is composed at. Every subscriber, not just the requester:
        // an idle mirror's pull is armed and nothing else would answer
        // it, leaving it painting locally resized stale rows.
        self.push_composed(composed);
    }

    /// `RegionToDaemonMsg::Request` (docs/explanation/data-model/scrollback.md
    /// "Piping to an external command"). The reply goes to the
    /// requesting subscriber only; a source the daemon cannot resolve
    /// answers with empty bytes rather than silence.
    fn handle_region(
        &mut self,
        owner: Owner,
        source: RegionSource,
        ansi: bool,
        request: RequestId,
    ) {
        if self.producers_mut(owner).is_none() {
            return;
        }
        let viewport = self.viewport_of(owner);
        let (data, position, exit_code) = {
            let core = self.session.lock_core();
            let serialized = crate::serve::region::serialize_region_positioned(
                &core.grid, viewport, source, ansi,
            );
            let exit_code = if serialized.is_some() {
                core.grid.last_command_exit()
            } else {
                None
            };
            let (data, position) = serialized.unwrap_or_default();
            (data, position, exit_code)
        };
        let (data, position) = trim_region_reply(data, position, MAX_REGION_REPLY_BYTES);
        let _evicted = self.push_owner(
            owner,
            OutEvent::Region {
                msg: RegionToClientMsg::Reply {
                    data,
                    position,
                    exit_code,
                },
                correlation: Correlation::request(request),
            },
        );
    }

    #[cfg(all(test, unix))]
    fn for_tests(mut session: Session) -> Self {
        let (rows, cols) = {
            let core = session.lock_core();
            (core.grid.rows(), core.grid.cols())
        };
        session.cell_pixel_w = crate::NOMINAL_CELL_PIXELS.0;
        session.cell_pixel_h = crate::NOMINAL_CELL_PIXELS.1;
        let meta = Arc::new(StdMutex::new(SessionMeta {
            rows,
            cols,
            pixel_w: 0,
            pixel_h: 0,
            title: None,
            cwd: None,
            idle_since: Instant::now(),
            subscribers: 0,
            tags: std::collections::BTreeSet::new(),
            last_notification: None,
            last_exit_code: None,
            exited: false,
            attachments: Vec::new(),
            program_status: Vec::new(),
            spawn_cwd: None,
            terminal_progress: None,
            sequence: std::num::NonZeroU64::MIN,
        }));
        Self {
            id: SessionId::new(),
            session,
            input_budget: crate::pool::new_input_budget(),
            subs: Vec::new(),
            requesters: Vec::new(),
            attachment_ids: AttachmentIds::detached(),
            meta,
            pool: Weak::new(),
            hub: tokio::sync::broadcast::channel(8).0,
            observers: ObserverOutbox::new(
                tokio::sync::broadcast::channel(64).0,
                SessionId::new(),
                crate::pool::Publication::new(),
            ),
            policy: IdlePolicy::default(),
            active_sub: None,
            input_owner: None,
            attachments_dirty: false,
            pty_eof: false,
            told_exit: None,
            child_exited: false,
            reaped_exit: None,
            row_cache: streaming::RowEncodeCache::default(),
            reported_focus: false,
            focus_dirty: false,
            reported_os_dark: false,
            scheme_dirty: false,
            resize_notify_dirty: false,
            reported_resize: None,
            pty_steps: Vec::new(),
            pty_resized_over: Vec::new(),
            pending_chunks: VecDeque::new(),
            upgrade_hold: false,
            decode: None,
            held_cmds: VecDeque::new(),
            awaiting_post_release_drain: false,
            shutdown: Arc::default(),
            idle: idle::IdleCompaction::new(idle::QUIET),
            before_decode: None,
            meta_title_epoch: 0,
            meta_cwd_epoch: 0,
            meta_status_generation: 0,
        }
    }

    fn animation_tick(&mut self) {
        self.animation_tick_at(crate::graphics::anim_now_ms());
    }

    fn animation_tick_at(&mut self, now: u64) {
        let advanced = self.session.images.advance_animations(now);
        if advanced.is_empty() {
            return;
        }
        trace!(
            target: "felis_daemon::graphics",
            frames = ?advanced.iter().map(|(id, idx)| (id.0, *idx)).collect::<Vec<_>>(),
            "animation tick",
        );
        let events: Vec<OutEvent> = advanced
            .into_iter()
            .map(|(id, idx)| {
                OutEvent::Image(ImageMsg::ShowFrame {
                    id,
                    number: crate::graphics::frame_number(idx),
                })
            })
            .collect();
        self.broadcast(|_| events.clone());
    }
}

#[cfg(all(test, unix))]
mod tests;

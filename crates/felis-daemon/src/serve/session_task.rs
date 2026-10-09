//! Per-session owner task owning the PTY-to-grid pipeline and subscriber fan-out.
//!
//! Slow subscribers past `SUBSCRIBER_BUFFER_CAP` are evicted rather than
//! backpressuring the shared PTY (session-lifecycle.md "Same-user mirroring").

use std::sync::{
    Arc, Mutex as StdMutex, Weak,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

use felis_protocol::messages::{
    AttentionSource, KeyEvent, MAX_REGION_REPLY_BYTES, RegionPosition, RegionSource, RequestedDims,
    ThemeChannel,
};
use felis_protocol::{
    ConnectionMode,
    messages::{
        Attachment, ConnToClientMsg, Correlation, GridDims, GridMsg, ImageMsg, InputMsg,
        NotifyToClientMsg, OpsToClientMsg, PushMsg, RegionToClientMsg, RequestId,
        SearchToClientMsg, SearchToDaemonMsg, SessionInfo, SessionToClientMsg, StreamErrorReason,
        StreamId, Subject, SwitchDenied, SwitchScope,
    },
};
use felis_pty::{ChildHandle, Size as PtySize};
use tokio::io::AsyncReadExt as _;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tracing::{debug, error, info, trace, warn};

use crate::{
    SessionId, SessionPool,
    pool::InputReservation,
    pool::{AttachmentIds, Listing, ParseCore, Session, SessionHandle, SessionMeta, SessionSlot},
    serve::{ConnError, IdlePolicy, key_encode, streaming, write_paste, write_pty},
};

/// 2× the per-session image-store cap, so a full image rehydrate burst
/// (the largest single push) can never trip it on a healthy
/// connection.
pub(crate) const SUBSCRIBER_BUFFER_CAP: usize = crate::pool::DEFAULT_IMAGE_BYTE_CAP * 2;

/// Sampled off live state when [`SessionCmd::Stats`] lands, not
/// tracked by the hot paths.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    pub image_bytes: usize,
    /// Read off the store, not the constant it was built from, so the
    /// reported limit is the one an insert is refused against.
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

/// Small on purpose: the channel backpressures a connection that
/// outruns the task, the same way the PTY would.
const CMD_CHANNEL_CAPACITY: usize = 64;

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
            Self::Image(ImageMsg::Chunk { bytes, .. }) => bytes.len() + WIRE_LEN_OVERHEAD,
            Self::Region {
                msg: RegionToClientMsg::Reply { data, .. },
                ..
            } => data.len() + WIRE_LEN_OVERHEAD,
            Self::Region {
                msg: RegionToClientMsg::Row { text, ansi, .. },
                ..
            } => text.len() + ansi.as_ref().map_or(0, String::len) + WIRE_LEN_OVERHEAD,
            _ => WIRE_LEN_OVERHEAD,
        }
    }
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
    pub tx: mpsc::UnboundedSender<OutEvent>,
    /// Bytes pushed but not yet written; the connection pump decrements
    /// as it writes (see `SUBSCRIBER_BUFFER_CAP`).
    pub buffered: Arc<AtomicUsize>,
    /// `SessionToDaemonMsg::Attach { live_only }`: refuse when the shell has
    /// exited.
    pub live_only: bool,
    /// Answered once the rehydrate burst is queued.
    pub reply: oneshot::Sender<Result<SubscribeOk, SubscribeRefused>>,
}

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
        sub: SubscriberId,
        msg: SearchToDaemonMsg,
        /// Client-allocated; stamped on every item and the terminal.
        stream: StreamId,
    },
    /// A `Conn::Cancel` the connection's driver already validated.
    CancelStream {
        /// A subscriber cannot cancel another's stream.
        sub: SubscriberId,
        stream: StreamId,
    },
    /// The client's configured colors, so an `OSC 10/11/12 ; ?` query
    /// answers with the real surface color
    /// (`SessionToDaemonMsg::ConfigureTheme`).
    ConfigureTheme {
        sub: SubscriberId,
        fg: Option<(u8, u8, u8)>,
        bg: Option<(u8, u8, u8)>,
        cursor: Option<(u8, u8, u8)>,
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
        sub: SubscriberId,
        source: RegionSource,
        ansi: bool,
        request: RequestId,
    },
    /// `RegionToDaemonMsg::Rows`: the same region row by row
    /// (`Row` … `RowsDone`).
    RegionRows {
        sub: SubscriberId,
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
/// `dims` is admitted already (REQ-605a) and applied here, not by the
/// first attach's resize, so the child sees its winsize from byte one.
/// `slot` is released as the handle registers, under one pool lock.
pub async fn spawn_session(
    pool: &Arc<Mutex<SessionPool>>,
    spawned: crate::SpawnedPty,
    policy: IdlePolicy,
    id: SessionId,
    dims: GridDims,
    tags: Vec<String>,
    slot: Option<SessionSlot>,
    listing: Listing,
) -> SessionLifecycle {
    let owned = Session::from_spawned(spawned);
    let (rows, cols) = (dims.rows, dims.cols);
    let (cur_rows, cur_cols) = {
        let core = owned.lock_core();
        (core.grid.rows(), core.grid.cols())
    };
    // The pixel axes reach the child even when the cell count already
    // matches: a producer reading TIOCGWINSZ derives its cell size from
    // them.
    if rows != cur_rows || cols != cur_cols || dims.pixel_w != 0 || dims.pixel_h != 0 {
        if let Err(err) = owned.resizer.resize(PtySize {
            rows,
            cols,
            pixel_width: dims.pixel_w,
            pixel_height: dims.pixel_h,
        }) {
            warn!(?err, "spawn: initial PTY resize failed");
        }
        owned.lock_core().grid.resize(rows, cols);
    }
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
}

/// Spawn a session owner task.
///
/// `tags` seeds metadata at registration so it is never listable untagged;
/// `listing` controls whether lookups answer immediately or hold until id delivery.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_owned(
    pool: &Arc<Mutex<SessionPool>>,
    mut session: Session,
    policy: IdlePolicy,
    id: SessionId,
    pixel_w: u16,
    pixel_h: u16,
    tags: Vec<String>,
    slot: Option<SessionSlot>,
    listing: Listing,
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
    // The same derivation `apply_size` runs.
    session.cell_pixel_w = pixel_w.checked_div(cols).unwrap_or(0);
    session.cell_pixel_h = pixel_h.checked_div(rows).unwrap_or(0);
    let input_budget = crate::pool::new_input_budget();
    // Minted under the registration's lock, before the meta is
    // published: a session must never be listable without the sequence
    // that places it in the ring.
    let mut guard = pool.lock().await;
    let sequence = guard.next_sequence();
    let meta = Arc::new(StdMutex::new(SessionMeta {
        rows,
        cols,
        pixel_w,
        pixel_h,
        title,
        cwd,
        idle_since: Instant::now(),
        subscribers: 0,
        tags: tags.into_iter().collect(),
        last_notification: None,
        exited: false,
        // A restored grid may already carry a D mark.
        last_exit_code: session.lock_core().grid.last_command_exit(),
        attachments: Vec::new(),
        sequence,
    }));
    let (hub, attachment_ids) = {
        guard.register(
            id,
            SessionHandle {
                cmd: cmd_tx.clone(),
                input_budget: Arc::clone(&input_budget),
                meta: Arc::clone(&meta),
                resizer: Some(Arc::clone(&session.resizer)),
            },
            slot,
            listing,
        );
        let handles = (guard.notify_hub(), guard.attachment_ids());
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
    let task = SessionTask {
        id,
        session,
        input_budget: Arc::clone(&input_budget),
        subs: Vec::new(),
        attachment_ids,
        meta,
        pool: Arc::downgrade(pool),
        hub,
        policy,
        active_sub: None,
        input_owner: None,
        attachments_dirty: false,
        pty_eof: false,
        child_exited: false,
        row_cache: streaming::RowEncodeCache::default(),
        reported_focus: false,
        focus_dirty: false,
        reported_os_dark: false,
        scheme_dirty: false,
        resize_notify_dirty: false,
        reported_resize: None,
        #[cfg(all(test, unix))]
        pty_steps: Vec::new(),
        meta_title_epoch: title_epoch,
        meta_cwd_epoch: cwd_epoch,
        pending_chunks: std::collections::VecDeque::new(),
    };
    let child = Arc::clone(&task.session.child);
    let handle = tokio::spawn(run_session(task, cmd_rx));
    let (done_tx, done) = watch::channel(false);
    drop(tokio::spawn(supervise(
        handle,
        Arc::downgrade(pool),
        id,
        child,
        done_tx,
    )));
    SessionLifecycle {
        id,
        info,
        cmd: cmd_tx,
        input_budget,
        done,
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
    done: watch::Sender<bool>,
) {
    let outcome = handle.await;
    if let Err(err) = outcome {
        report_dead_task(err, &pool, id, &child).await;
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
    /// Pull pacing (docs/explanation/rendering/pipeline.md
    /// "Demand-driven emission"): diffs ship only against a
    /// pending `NextGridFrame` pull.
    pull_pacing: bool,
    pull_pending: bool,
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
/// One `Option` over the trio because `ConfigureTheme` is an atomic report;
/// an unset channel means "no color", not fallback to another window.
#[derive(Default)]
struct PresentationReports {
    /// Indexed by `ThemeChannel as usize`.
    theme: Option<[Option<(u8, u8, u8)>; 3]>,
    os_dark: Option<bool>,
}

impl Subscriber {
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
    attachment_ids: AttachmentIds,
    meta: Arc<StdMutex<SessionMeta>>,
    /// Weak so a parked task keeps no pool alive.
    pool: Weak<Mutex<SessionPool>>,
    hub: tokio::sync::broadcast::Sender<NotifyToClientMsg>,
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
    child_exited: bool,
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
    reported_resize: Option<(u32, (u16, u16))>,
    /// Grid epochs already mirrored into [`Self::meta`], gating the
    /// `String` clones.
    meta_title_epoch: u64,
    meta_cwd_epoch: u64,
    /// Test-only: a resize is an `ioctl` on the master while reports
    /// queue through the writer thread, so no observer outside the task
    /// can see the order.
    #[cfg(all(test, unix))]
    pty_steps: Vec<PtyStep>,
    /// An in-task queue rather than a self-addressed `SessionCmd`: the
    /// command channel is bounded, so a producer re-queueing through it
    /// could block while holding the only thread that drains it.
    pending_chunks: std::collections::VecDeque<(SubscriberId, StreamId)>,
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
    Rows {
        source: RegionSource,
        ansi: bool,
        max_rows: Option<u32>,
        offset: usize,
    },
}

/// What a `DECSET 2048` report states, or `None` while the program has
/// not opted in.
fn resize_notify_key(grid: &felis_grid::Grid) -> Option<(u32, (u16, u16))> {
    grid.in_band_resize_notify()
        .then(|| (grid.resize_notify_epoch(), (grid.rows(), grid.cols())))
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

async fn run_session(mut t: SessionTask, mut cmd_rx: mpsc::Receiver<SessionCmd>) {
    // Every way out, an unwinding panic included, releases a sink
    // waiting in `wait_for_drain`.
    let _release_sink = SinkRelease(Arc::clone(&t.session.signals));
    let mut eof_buf = [0u8; 16];
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
        let attached = !t.subs.is_empty();
        // Per iteration rather than at every `subs` mutation site; a
        // one-chunk-stale read only mis-paces ~1 KiB.
        t.session.signals.set_attached_subs(t.subs.len());
        tokio::select! {
            biased;
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
            cmd = cmd_rx.recv() => {
                match cmd {
                    None => break EndReason::PoolDropped,
                    Some(cmd) => {
                        if let Some(reason) = t.handle_cmd(cmd) {
                            break reason;
                        }
                    }
                }
            }
            // After commands in the biased order: a `Cancel` sent
            // mid-walk lands before the next slice.
            () = std::future::ready(()), if t.has_pending_chunks() => {
                t.run_next_chunk();
            }
            // Ungated by `attached`: a parked session still owes query
            // responses to the program and notifications to observers.
            () = t.session.signals.parsed(), if drain_at.is_none() => {
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
            () = tokio::time::sleep_until(drain_deadline), if drain_at.is_some() => {
                drain_at = None;
                let drained = t.drain_and_fan();
                last_drain = tokio::time::Instant::now();
                if let Err(err) = drained {
                    warn!(?err, "session task: PTY pipeline error");
                    break EndReason::PtyError;
                }
            }
            // No data ever lands here (`felis_pty::PtyReader`).
            res = t.session.reader.read(&mut eof_buf), if !t.pty_eof => {
                match res {
                    Ok(0) => {
                        t.pty_eof = true;
                        t.stamp_exited_meta();
                        // `felis_pty` guarantees the child's final
                        // output is parsed by the time EOF lands here.
                        if let Err(err) = t.drive_cycle() {
                            debug!(?err, "session task: effect drain at EOF failed");
                        }
                        t.notify_shell_exit();
                        t.evict_all(None);
                    }
                    Ok(_) => {
                        warn!("session task: unexpected data on the sink-mode reader");
                    }
                    Err(err) => {
                        warn!(?err, "session task: PTY read error");
                        break EndReason::PtyError;
                    }
                }
            }
            // No drain here: PTY flow control is the sink's pacer
            // (`crate::parse_sink`).
            _ = drain_tick.tick(), if !attached => {
                if !t.child_exited
                    && matches!(t.session.child.try_wait(), Ok(Some(_)))
                {
                    t.child_exited = true;
                    t.stamp_exited_meta();
                }
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
        }
        // Sync paths (detach, eviction, resize) cannot write to the
        // PTY; the edges they leave are reconciled here.
        if t.focus_dirty
            && let Some(reason) = t.sync_focus_to_pty()
        {
            break reason;
        }
        if t.scheme_dirty
            && let Some(reason) = t.sync_color_scheme_to_pty()
        {
            break reason;
        }
        if t.resize_notify_dirty
            && let Some(reason) = t.sync_resize_notify_to_pty()
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
    t.evict_all(None);
    let teardown = if let Some(pool) = t.pool.upgrade() {
        let mut guard = pool.lock().await;
        let teardown = guard.begin_teardown();
        guard.remove(t.id);
        Some(teardown)
    } else {
        None
    };
    end_child(&t.session.child).await;
    drop(teardown);
}

/// The window is for a shell that traps `SIGHUP` to save state; it is
/// short because the session is already gone from the pool, so
/// anything still running is unreachable by the user.
const HANGUP_GRACE: Duration = Duration::from_secs(2);

const HANGUP_POLL: Duration = Duration::from_millis(50);

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
    let deadline = Instant::now() + HANGUP_GRACE;
    while Instant::now() < deadline {
        tokio::time::sleep(HANGUP_POLL).await;
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(err) => {
                warn!(?err, "session teardown: waiting on the child");
                return;
            }
        }
    }
    match child.kill() {
        Ok(true) => info!("session teardown: child ignored the hangup; killed"),
        Ok(false) => {}
        Err(err) => warn!(?err, "session teardown: kill"),
    }
    // Polled to an observed exit, not looked at once: `SIGKILL` is
    // delivered, not completed, so a single look can miss it and leave a
    // zombie the daemon parents for the rest of its life.
    let deadline = Instant::now() + REAP_GRACE;
    loop {
        tokio::time::sleep(HANGUP_POLL).await;
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(err) => {
                warn!(?err, "session teardown: reaping the killed child");
                return;
            }
        }
        if Instant::now() >= deadline {
            warn!("session teardown: the killed child was not reaped within the grace");
            return;
        }
    }
}

/// The keyboard modes come off the same locked core the mouse encoding
/// reads, so a mode the child set is in effect for the very next key.
/// Keys are encoded daemon-side so the bytes follow the modes this daemon
/// has already parsed rather than the ones the client's last grid frame
/// carried (docs/explanation/input.md "Keyboard").
fn encode_input(core: &ParseCore, msg: &InputMsg) -> Option<Vec<u8>> {
    match msg {
        InputMsg::Mouse(event) => felis_grid::encode_mouse(
            *event,
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
    clipboard: Option<felis_grid::ClipboardWrite>,
    composed: Vec<Composed>,
}

fn pty_write_outcome(res: Result<(), ConnError>) -> Option<EndReason> {
    res.err().map(|err| {
        warn!(?err, "session task: PTY write error");
        EndReason::PtyError
    })
}

impl SessionTask {
    fn handle_cmd(&mut self, cmd: SessionCmd) -> Option<EndReason> {
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
            SessionCmd::Search { sub, msg, stream } => self.handle_search(sub, msg, stream),
            SessionCmd::CancelStream { sub, stream } => {
                self.cancel_producer(sub, stream);
                None
            }
            SessionCmd::ConfigureTheme {
                sub,
                fg,
                bg,
                cursor,
            } => {
                let i = self.sub_index(sub)?;
                self.subs[i].presentation.theme = Some([fg, bg, cursor]);
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
                sub,
                source,
                ansi,
                request,
            } => {
                self.handle_region(sub, source, ansi, request);
                None
            }
            SessionCmd::RegionRows {
                sub,
                source,
                ansi,
                max_rows,
                stream,
            } => {
                self.start_rows(sub, source, ansi, max_rows, stream);
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
        }
    }

    fn subscribe(&mut self, req: SubscribeReq) {
        // Checked inside the actor, not off the roster's `exited` flag
        // client-side: that flag is a snapshot, and a session exiting
        // between the listing and the attach would leave a window
        // parked on a corpse that never pushes it off and blocks the
        // reap.
        if req.live_only && (self.pty_eof || self.child_exited) {
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
            pull_pacing: req.pull_paced,
            pull_pending: false,
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

    /// The OR, not last-writer: a background mirror blurring must not
    /// un-focus a session the foreground window still shows focused,
    /// and window events arrive in any order across connections.
    fn window_focused(&self) -> bool {
        self.subs.iter().any(|s| s.is_window && s.focused)
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
            .find_map(|s| s.presentation.theme)
            .unwrap_or([None; 3]);
        let os_dark = self
            .presentation_chain()
            .find_map(|s| s.presentation.os_dark);
        let mut core = self.session.lock_core();
        let grid = &mut core.grid;
        // Not change-tested here: whether the scheme moved is a
        // question about the last answer the program was given, which
        // only the reconcile can tell (two recomputes in one pass may
        // leave and re-enter dark).
        for channel in [
            ThemeChannel::Foreground,
            ThemeChannel::Background,
            ThemeChannel::Cursor,
        ] {
            grid.set_theme_config_default(channel, theme[channel as usize]);
        }
        match os_dark {
            Some(dark) => grid.set_os_dark(dark),
            None => grid.clear_os_dark(),
        }
        drop(core);
        self.scheme_dirty = true;
    }

    /// Compared on the answer the program can observe, not the stored
    /// `Option`: an unset preference answers light, so a light reporter
    /// leaving changes nothing the program could see. The mode is read
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
    fn push_terminal(&mut self, sub_id: SubscriberId, i: usize, terminal: ConnToClientMsg) {
        if !self.subs[i].force_push(OutEvent::Control(terminal)) {
            self.remove_sub(sub_id);
        }
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
    fn notify_shell_exit(&self) {
        for sub in &self.subs {
            if sub.is_window {
                let _ = sub.push(OutEvent::Push(PushMsg::SessionExited { id: self.id.0 }));
            }
        }
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
        let (max_subscriber_backlog, total_subscriber_backlog) =
            self.subs
                .iter()
                .fold((0usize, 0usize), |(max, total), sub| {
                    let queued = sub.buffered.load(Ordering::Relaxed);
                    (max.max(queued), total.saturating_add(queued))
                });
        SessionStats {
            image_bytes: self.session.images.bytes_used(),
            image_bytes_cap: self.session.images.bytes_cap(),
            decode_bytes_in_flight: self.session.graphics_reassembler.in_flight_bytes(),
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
        let mut meta = self
            .meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        meta.rows = rows;
        meta.cols = cols;
        if let Some((epoch, title)) = title {
            self.meta_title_epoch = epoch;
            meta.title = title;
        }
        if let Some((epoch, cwd)) = cwd {
            self.meta_cwd_epoch = epoch;
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
            meta.attachments = self
                .subs
                .iter()
                .filter(|sub| sub.is_window)
                .map(|sub| Attachment {
                    id: sub.id.0,
                    attached_at: sub.attached_at,
                    input_owner: self.input_owner == Some(sub.id),
                })
                .collect();
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
        // The parse thread records a `DECSET 2048` set without writing
        // anything; the burst it rode in on is drained here, so the
        // reconcile owes the state a look. Judged under this guard: the
        // reconcile takes the lock again, a second wait per cycle.
        let drained = streaming::drain_effects(&mut self.session, core);
        if resize_notify_key(&core.grid) != self.reported_resize {
            self.resize_notify_dirty = true;
        }
        drained
    }

    fn drain_and_fan(&mut self) -> Result<(), ConnError> {
        // Re-arm before draining: a chunk parsed mid-drain must
        // re-notify, or its effects strand until the next unrelated
        // wake.
        self.session.signals.clear_dirty();
        let generation = self.session.signals.drain_generation();
        let drained = self.drive_cycle();
        self.session.signals.publish_drained(generation);
        drained
    }

    /// The drain's error is returned after the cycle ships rather than
    /// before: at EOF the child's final frame must still reach the
    /// windows.
    fn drive_cycle(&mut self) -> Result<(), ConnError> {
        let core = Arc::clone(&self.session.core);
        let mut guard = core.lock();
        let drained = self.drain_effects_under(&mut guard);
        let cycle = self.compose_cycle(&mut guard);
        drop(guard);
        self.finish_cycle(cycle);
        drained
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
    }

    fn push_image_events(&mut self) {
        if self.session.image_events.is_empty() {
            return;
        }
        // Taken unconditionally (the queue is otherwise unbounded for a
        // parked session), materialized only for a window: a parked mpv
        // would otherwise pay a frame read per video frame for nobody.
        let events = std::mem::take(&mut self.session.image_events);
        if self.subs.iter().any(|sub| sub.is_window) {
            let msgs = crate::graphics::materialize_image_events(&events, &self.session.images);
            self.broadcast(|sub| {
                if sub.is_window {
                    msgs.iter().cloned().map(OutEvent::Image).collect()
                } else {
                    Vec::new()
                }
            });
        }
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
    fn collect_facets_under(
        &self,
        core: &mut ParseCore,
    ) -> (Vec<GridMsg>, Option<felis_grid::ClipboardWrite>) {
        if self.subs.is_empty() || !core.grid.ready_to_present(Instant::now()) {
            return (Vec::new(), None);
        }
        (
            streaming::collect_facets(&mut core.grid),
            core.grid.take_pending_clipboard_set(),
        )
    }

    fn push_facets(&mut self, facets: &[GridMsg], clipboard: Option<felis_grid::ClipboardWrite>) {
        if !facets.is_empty() {
            self.broadcast(|sub| {
                if sub.is_window {
                    facets.iter().cloned().map(OutEvent::Grid).collect()
                } else {
                    Vec::new()
                }
            });
        }
        if let Some(write) = clipboard {
            self.route_clipboard_set(write);
        }
    }

    /// Per-client clipboard scope (security-model.md): with
    /// `clipboard.osc_52 = "system"` a broadcast would overwrite every
    /// mirror host's clipboard. The active subscriber is the best
    /// attribution; a sole window is unambiguous; otherwise the write
    /// is dropped rather than guessed.
    fn route_clipboard_set(&mut self, write: felis_grid::ClipboardWrite) {
        let target = self
            .active_sub
            .filter(|&id| self.subs.iter().any(|s| s.id == id && s.is_window))
            .or_else(|| {
                let mut windows = self.subs.iter().filter(|s| s.is_window);
                match (windows.next(), windows.next()) {
                    (Some(only), None) => Some(only.id),
                    _ => None,
                }
            });
        let Some(target) = target else {
            debug!(
                id = ?self.id,
                "dropping OSC 52 write: no attributable window subscriber"
            );
            return;
        };
        if let Some(i) = self.sub_index(target)
            && !self.push_to(target, i, OutEvent::Grid(GridMsg::ClipboardSet { write }))
        {
            debug!(id = ?self.id, sub = target.0, "OSC 52 target outbox died");
        }
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
        );
        Some((sub.id, composed.map(|()| out)))
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

    fn handle_admitted_input(
        &mut self,
        sub_id: SubscriberId,
        msg: InputMsg,
        reservation: Option<InputReservation>,
    ) -> Option<EndReason> {
        let i = self.sub_index(sub_id)?;
        // Active-owns-winsize (session-lifecycle.md "Same-user mirroring"): size
        // ownership follows input reaching the PTY. Mouse motion promotes only if it
        // encodes to bytes, preventing passive mirror hovers from stealing size.
        // Promoted before dispatch so new winsize applies before bytes reach shell.
        let promotes = match &msg {
            InputMsg::KeyBytes(_) | InputMsg::Paste(_) => true,
            InputMsg::Mouse(_) | InputMsg::Key(_) => {
                encode_input(&self.session.lock_core(), &msg).is_some()
            }
            _ => false,
        };
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
            InputMsg::Viewport { lines_from_bottom } => {
                let stream = &mut self.subs[i].stream;
                let new_viewport = session.lock_core().grid.clamp_viewport(lines_from_bottom);
                if new_viewport != stream.diff.viewport {
                    stream.diff.viewport = new_viewport;
                    stream.damage.mark_all();
                }
            }
            InputMsg::JumpPrompt { direction } => {
                let stream = &mut self.subs[i].stream;
                let target = session
                    .lock_core()
                    .grid
                    .prompt_jump_target(stream.diff.viewport, direction);
                if let Some(target) = target {
                    stream.diff.viewport = target;
                    stream.damage.mark_all();
                }
            }
            InputMsg::NextGridFrame => {
                self.subs[i].pull_pending = true;
                trace!(target: "felis_daemon::serve", "grid frame pull");
            }
        }
        // Ship after every input, not only on the PTY arm: a Viewport
        // request into a quiet prompt would otherwise sit unrendered
        // until the user types.
        pty_write_outcome(self.grid_cycle_for(sub_id))
    }

    fn handle_search(
        &mut self,
        sub_id: SubscriberId,
        msg: SearchToDaemonMsg,
        stream: StreamId,
    ) -> Option<EndReason> {
        let i = self.sub_index(sub_id)?;
        let SearchToDaemonMsg::Query { query, options } = msg;
        let compiled = match felis_grid::SearchQuery::new(&query, options) {
            Ok(q) => q,
            Err(err) => {
                // The stream is already open on the wire, so it owes
                // its one terminal even now.
                self.push_terminal(
                    sub_id,
                    i,
                    ConnToClientMsg::Error {
                        subject: Subject::Stream(stream),
                        reason: StreamErrorReason::InvalidRequest,
                        detail: err.to_string(),
                    },
                );
                return None;
            }
        };
        // `lock_core` is not reentrant; one guard for both reads.
        let total = {
            let core = self.session.lock_core();
            core.grid.scrollback().len() + usize::from(core.grid.rows())
        };
        let budget = SEARCH_CHUNK_LINES.max(total.div_ceil(MAX_SEARCH_CHUNKS));
        self.subs[i].producers.push(Producer {
            stream,
            canceled: false,
            emitted: 0,
            kind: ProducerKind::Search {
                query: compiled,
                cursor: felis_grid::SearchCursor::default(),
                budget,
            },
        });
        self.pending_chunks.push_back((sub_id, stream));
        None
    }

    /// An unresolved source (no closed `OSC 133` range yet) ends an
    /// empty stream rather than going silent.
    fn start_rows(
        &mut self,
        sub_id: SubscriberId,
        source: RegionSource,
        ansi: bool,
        max_rows: Option<u32>,
        stream: StreamId,
    ) {
        let Some(i) = self.sub_index(sub_id) else {
            return;
        };
        self.subs[i].producers.push(Producer {
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
        self.pending_chunks.push_back((sub_id, stream));
    }

    /// A canceled producer stays queued for one more turn: it still
    /// owes the stream its terminal.
    fn cancel_producer(&mut self, sub_id: SubscriberId, stream: StreamId) {
        let Some(i) = self.sub_index(sub_id) else {
            return;
        };
        let Some(producer) = self.subs[i]
            .producers
            .iter_mut()
            .find(|p| p.stream == stream)
        else {
            // The terminal was already on the wire; the connection
            // driver treats this cancel as idempotent.
            return;
        };
        producer.canceled = true;
        if !self.pending_chunks.contains(&(sub_id, stream)) {
            self.pending_chunks.push_back((sub_id, stream));
        }
    }

    /// One slice per run-loop turn, never looped: a cancel, a
    /// keystroke, and the PTY drain get a turn between slices.
    fn run_next_chunk(&mut self) {
        let Some((sub_id, stream)) = self.pending_chunks.pop_front() else {
            return;
        };
        self.run_chunk(sub_id, stream);
    }

    fn has_pending_chunks(&self) -> bool {
        !self.pending_chunks.is_empty()
    }

    fn run_chunk(&mut self, sub_id: SubscriberId, stream: StreamId) {
        let Some(i) = self.sub_index(sub_id) else {
            return;
        };
        let Some(p) = self.subs[i]
            .producers
            .iter()
            .position(|p| p.stream == stream)
        else {
            return;
        };
        if self.subs[i].producers[p].canceled {
            self.close_stream(sub_id, i, p, None);
            return;
        }
        let (events, done, failure) = self.next_slice(i, p);
        for ev in events {
            if !self.push_to(sub_id, i, ev) {
                return;
            }
        }
        // `push_to` may have evicted the subscriber.
        let Some(i) = self.sub_index(sub_id) else {
            return;
        };
        let Some(p) = self.subs[i]
            .producers
            .iter()
            .position(|p| p.stream == stream)
        else {
            return;
        };
        if done || failure.is_some() {
            self.close_stream(sub_id, i, p, failure);
        } else {
            self.pending_chunks.push_back((sub_id, stream));
        }
    }

    /// Returns the events, whether the walk finished, and a failure
    /// that terminates the stream instead of a clean end.
    #[allow(clippy::type_complexity)]
    fn next_slice(
        &mut self,
        i: usize,
        p: usize,
    ) -> (Vec<OutEvent>, bool, Option<(StreamErrorReason, String)>) {
        let stream = self.subs[i].producers[p].stream;
        let correlation = Correlation::stream(stream);
        let viewport = self.subs[i].stream.diff.viewport;
        let mut out = Vec::new();
        let session = &self.session;
        let (done, results) = match &mut self.subs[i].producers[p].kind {
            ProducerKind::Search {
                query,
                cursor,
                budget,
            } => {
                let core = session.lock_core();
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
        self.subs[i].producers[p].emitted =
            self.subs[i].producers[p].emitted.saturating_add(results);
        (out, done, None)
    }

    fn close_stream(
        &mut self,
        sub_id: SubscriberId,
        i: usize,
        p: usize,
        failure: Option<(StreamErrorReason, String)>,
    ) {
        let producer = self.subs[i].producers.remove(p);
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
        self.push_terminal(sub_id, i, terminal);
    }

    fn promote_active(&mut self, sub_id: SubscriberId, i: usize) {
        if self.active_sub == Some(sub_id) {
            return;
        }
        self.active_sub = Some(sub_id);
        let differs = |size: PtySize| {
            let core = self.session.lock_core();
            size.rows != core.grid.rows() || size.cols != core.grid.cols()
        };
        if let Some(size) = self.subs[i].desired_size
            && differs(size)
        {
            debug!(
                sub = sub_id.0,
                rows = size.rows,
                cols = size.cols,
                "size ownership transferred on input"
            );
            self.apply_size(size);
        }
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
    fn apply_size(&mut self, size: PtySize) {
        // Floor division matches how producers derive cell size from
        // TIOCGWINSZ.
        let cell_px = (
            size.pixel_width.checked_div(size.cols).unwrap_or(0),
            size.pixel_height.checked_div(size.rows).unwrap_or(0),
        );
        let cell_px_changed = cell_px != (self.session.cell_pixel_w, self.session.cell_pixel_h);
        (self.session.cell_pixel_w, self.session.cell_pixel_h) = cell_px;
        {
            // `refresh_meta` samples the grid, which holds no pixels.
            let mut meta = self
                .meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            meta.pixel_w = size.pixel_width;
            meta.pixel_h = size.pixel_height;
        }
        let shared = Arc::clone(&self.session.core);
        let mut core = shared.lock();
        // The `DECSET 2048` report carries cells only, so a pixels-only
        // resize moves nothing it would state.
        if (core.grid.rows(), core.grid.cols()) != (size.rows, size.cols) {
            self.resize_notify_dirty = true;
        }
        // The primary screen reflows (REQ-604); the alternate screen
        // trims/pads, its rows being a distinct surface rather than a
        // continuation of the shared scrollback.
        let remap = if core.grid.on_alternate_screen() {
            core.grid.resize(size.rows, size.cols);
            None
        } else {
            core.grid.reflow(size.rows, size.cols)
        };
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
                self.session.saved_primary_placements.as_mut(),
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
        self.pty_steps.push(PtyStep::Resize);
        if let Err(err) = self.session.resizer.resize(size) {
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
        sub_id: SubscriberId,
        source: RegionSource,
        ansi: bool,
        request: RequestId,
    ) {
        let Some(i) = self.sub_index(sub_id) else {
            return;
        };
        let viewport = self.subs[i].stream.diff.viewport;
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
        let _evicted = self.push_to(
            sub_id,
            i,
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
    fn for_tests(session: Session) -> Self {
        let (rows, cols) = {
            let core = session.lock_core();
            (core.grid.rows(), core.grid.cols())
        };
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
            sequence: std::num::NonZeroU64::MIN,
        }));
        Self {
            id: SessionId::new(),
            session,
            input_budget: crate::pool::new_input_budget(),
            subs: Vec::new(),
            attachment_ids: AttachmentIds::detached(),
            meta,
            pool: Weak::new(),
            hub: tokio::sync::broadcast::channel(8).0,
            policy: IdlePolicy::default(),
            active_sub: None,
            input_owner: None,
            attachments_dirty: false,
            pty_eof: false,
            child_exited: false,
            row_cache: streaming::RowEncodeCache::default(),
            reported_focus: false,
            focus_dirty: false,
            reported_os_dark: false,
            scheme_dirty: false,
            resize_notify_dirty: false,
            reported_resize: None,
            pty_steps: Vec::new(),
            pending_chunks: std::collections::VecDeque::new(),
            meta_title_epoch: 0,
            meta_cwd_epoch: 0,
        }
    }

    fn animation_tick(&mut self) {
        let now = crate::graphics::anim_now_ms();
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
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use felis_protocol::messages::{
        KeyEventKind, PromptJump, RequestedDims, RetargetCarrier, RetargetLanding, RetargetTarget,
        SpawnArgs,
    };
    use felis_pty::Command;
    use proptest::prelude::*;

    use super::*;

    fn test_request() -> RequestId {
        RequestId::new(1).unwrap_or_else(|| unreachable!("1 is non-zero"))
    }

    fn test_stream(n: u64) -> StreamId {
        StreamId::new(n).unwrap_or_else(|| unreachable!("callers pass non-zero"))
    }

    /// Builtin-only bodies; one that runs an external program wants
    /// [`crate::fixture_path`], or it silently runs nothing on a host
    /// with no FHS `/bin`.
    fn session_with(body: &str) -> Session {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", body]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        Session::from_spawned(crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"))
    }

    /// Asserted on the grid rather than the reader half, which is
    /// EOF-only in sink mode.
    async fn wait_for_grid_text(task: &SessionTask, needle: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let text: String = {
                let core = task.session.lock_core();
                let clusters = core.grid.cluster_table();
                core.grid
                    .rows_with_wrap(felis_grid::AltScreenRows::Include)
                    .map(|(row, _)| felis_grid::row_text_trim(row, clusters) + "\n")
                    .collect()
            };
            if text.contains(needle) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "grid never showed {needle:?}; grid text:\n{text}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn attach_sub(
        task: &mut SessionTask,
        pull_paced: bool,
    ) -> (SubscriberId, mpsc::UnboundedReceiver<OutEvent>) {
        attach_sub_as(task, ConnectionMode::Window, pull_paced)
    }

    fn attach_ops_sub(task: &mut SessionTask) -> (SubscriberId, mpsc::UnboundedReceiver<OutEvent>) {
        attach_sub_as(task, ConnectionMode::Ops, false)
    }

    fn attach_sub_as(
        task: &mut SessionTask,
        mode: ConnectionMode,
        pull_paced: bool,
    ) -> (SubscriberId, mpsc::UnboundedReceiver<OutEvent>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        task.subscribe(SubscribeReq {
            mode,
            pull_paced,
            live_only: false,
            tx,
            buffered: Arc::new(AtomicUsize::new(0)),
            reply: reply_tx,
        });
        let ok = reply_rx
            .try_recv()
            .expect("subscribe reply")
            .expect("the subscribe was admitted");
        while !matches!(
            rx.try_recv().expect("rehydrate event"),
            OutEvent::Grid(GridMsg::RehydrateEnd)
        ) {}
        (ok.sub, rx)
    }

    fn attach_burst(task: &mut SessionTask, mode: ConnectionMode) -> Vec<OutEvent> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        task.subscribe(SubscribeReq {
            mode,
            pull_paced: false,
            live_only: false,
            tx,
            buffered: Arc::new(AtomicUsize::new(0)),
            reply: reply_tx,
        });
        reply_rx
            .try_recv()
            .expect("subscribe reply")
            .expect("the subscribe was admitted");
        let mut burst = Vec::new();
        loop {
            let ev = rx.try_recv().expect("rehydrate event");
            let end = matches!(ev, OutEvent::Grid(GridMsg::RehydrateEnd));
            burst.push(ev);
            if end {
                return burst;
            }
        }
    }

    /// An `Ops` attach burst carries only the markers and the keyboard
    /// state; a window burst carries rows, meta, and the image store.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ops_attach_burst_holds_no_session_content_a_window_burst_holds_it_all() {
        use felis_grid::images::ImageEntry;
        use felis_protocol::ImageId;
        use felis_protocol::messages::ImageFormat;

        let mut task = SessionTask::for_tests(session_with("read _x"));
        {
            let mut core = task.session.lock_core();
            let mut parser = felis_vt::Parser::new();
            parser.advance(
                &mut core.grid,
                b"\x1b]0;fixture\x07\x1b]133;A\x07hello world\r\n",
            );
        }
        task.session
            .images
            .insert(
                ImageId(7),
                ImageEntry::new(1, 1, ImageFormat::Rgba32, vec![0xAA, 0xBB, 0xCC, 0xDD]),
            )
            .expect("insert fixture image");

        let ops = attach_burst(&mut task, ConnectionMode::Ops);
        let shapes: Vec<&str> = ops
            .iter()
            .map(|ev| match ev {
                OutEvent::Grid(GridMsg::RehydrateBegin) => "begin",
                OutEvent::Grid(GridMsg::RehydrateEnd) => "end",
                OutEvent::Grid(GridMsg::KittyKbdFlags { .. }) => "kbd",
                OutEvent::Grid(GridMsg::ModeFlags { .. }) => "modes",
                OutEvent::Grid(GridMsg::RowDelta { .. }) => "rows",
                OutEvent::Image(_) => "image",
                _ => "other",
            })
            .collect();
        assert_eq!(
            shapes,
            ["begin", "kbd", "modes", "end"],
            "an Ops attach burst must carry nothing but the markers and the keyboard modes"
        );

        let window = attach_burst(&mut task, ConnectionMode::Window);
        assert!(
            window
                .iter()
                .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. }))),
            "a window burst must still replay the grid rows"
        );
        assert!(
            window
                .iter()
                .any(|ev| matches!(ev, OutEvent::Image(ImageMsg::Complete { .. }))),
            "a window burst must still replay the image store"
        );
        assert!(
            window
                .iter()
                .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Title { .. }))),
            "a window burst must still replay the session meta"
        );
    }

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<OutEvent>) -> Vec<OutEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }

    /// `InputMsg::Viewport` clamps the requesting subscriber's viewport
    /// to retained scrollback depth and composes the browse view for
    /// that subscriber alone (session-lifecycle.md "Same-user
    /// mirroring").
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_viewport_clamps_to_retained_scrollback() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let rows = task.session.lock_core().grid.rows();
        let pad = "x\r\n".repeat(usize::from(rows) + 3);
        let mut parser = felis_vt::Parser::new();
        parser.advance(&mut task.session.lock_core().grid, pad.as_bytes());
        let depth = task.session.lock_core().grid.scrollback().len() as u32;
        assert!(depth >= 3, "fixture needs ≥ 3 scrollback rows, got {depth}");

        let (sub_a, mut rx_a) = attach_sub(&mut task, false);
        let (_sub_b, mut rx_b) = attach_sub(&mut task, false);

        let end = task.handle_input(
            sub_a,
            InputMsg::Viewport {
                lines_from_bottom: 9_999,
            },
        );
        assert!(end.is_none(), "Viewport is a continuing input");
        assert_eq!(
            task.subs[0].stream.diff.viewport, depth,
            "viewport must clamp to retained scrollback depth",
        );

        let a_rows: usize = drain_events(&mut rx_a)
            .iter()
            .filter_map(|ev| match ev {
                OutEvent::Grid(GridMsg::RowDelta { rows }) => Some(rows.len()),
                _ => None,
            })
            .sum();
        assert_eq!(a_rows, usize::from(rows), "composed view for the requester");
        assert!(
            drain_events(&mut rx_b).is_empty(),
            "a viewport change must not disturb other subscribers",
        );
    }

    fn report_theme(task: &mut SessionTask, sub: SubscriberId, bg: Option<(u8, u8, u8)>) {
        let end = task.handle_cmd(SessionCmd::ConfigureTheme {
            sub,
            fg: Some((0xE5, 0xE5, 0xE5)),
            bg,
            cursor: None,
        });
        assert!(end.is_none(), "ConfigureTheme is a continuing command");
    }

    fn report_scheme(task: &mut SessionTask, sub: SubscriberId, dark: bool) {
        let end = task.handle_input(sub, InputMsg::ColorScheme { dark });
        assert!(end.is_none(), "ColorScheme is a continuing input");
    }

    /// The race `InputMsg::Key` exists for: the daemon parses a mode
    /// change, the subscriber has not been handed the `ModeFlags` frame
    /// that mirrors it, and the very next key must still encode under
    /// the new mode.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_key_encodes_against_a_mode_the_subscriber_has_not_been_told_about() {
        let tmp = tempfile::TempDir::new().unwrap();
        let received = tmp.path().join("keys");
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            &format!("stty raw -echo; exec cat > {}", received.display()),
        ]);
        cmd.env_clear();
        cmd.env("PATH", crate::fixture_path());
        let mut task = SessionTask::for_tests(Session::from_spawned(
            crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"),
        ));

        let (sub, mut rx) = attach_sub(&mut task, false);
        drain_events(&mut rx);

        // Straight into the grid, so nothing is fanned out: the
        // subscriber's shadow still has DECCKM off when the key arrives.
        let mut parser = felis_vt::Parser::new();
        parser.advance(&mut task.session.lock_core().grid, b"\x1b[?1h");
        assert!(
            task.session
                .lock_core()
                .grid
                .mode_snapshot()
                .application_cursor,
            "fixture must leave DECCKM set daemon-side",
        );

        let end = task.handle_input(
            sub,
            InputMsg::Key(KeyEvent {
                key: felis_protocol::messages::Key::Named(
                    felis_protocol::messages::NamedKey::ArrowUp,
                ),
                text: None,
                mods: felis_protocol::messages::KeyMods::empty(),
                kind: KeyEventKind::Press,
                location: felis_protocol::messages::KeyLocation::Standard,
            }),
        );
        assert!(end.is_none(), "Key is a continuing input");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if std::fs::read(&received).is_ok_and(|bytes| bytes.ends_with(b"\x1bOA")) {
                break;
            }
            let seen = std::fs::read(&received).unwrap_or_default();
            assert!(
                tokio::time::Instant::now() < deadline,
                "the arrow reached the child as {seen:?}, not the DECCKM form ESC O A",
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let told_after = drain_events(&mut rx).iter().any(|ev| {
            matches!(
                ev,
                OutEvent::Grid(GridMsg::ModeFlags {
                    application_cursor: true,
                    ..
                })
            )
        });
        assert!(
            told_after,
            "the mode mirror must still have been undelivered when the key was encoded",
        );
    }

    /// Repeat is the one event kind with no `KeyBytes` equivalent: it
    /// reports Kitty event type 2 while event types are on, and as a
    /// press otherwise.
    #[test]
    fn a_repeat_reports_kitty_event_type_two_only_while_event_types_are_on() {
        use crate::serve::key_encode::{KittyKbdFlags, encode};
        use felis_protocol::messages::{Key, KeyLocation, KeyMods, ModifyOtherKeys};

        let at = |flags: KittyKbdFlags, kind: KeyEventKind| {
            encode(
                &Key::Named(felis_protocol::messages::NamedKey::Escape),
                None,
                KeyMods::empty(),
                kind,
                flags,
                ModifyOtherKeys::Off,
                false,
                false,
                false,
                KeyLocation::Standard,
            )
        };
        let with_events = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
        assert_eq!(
            at(with_events, KeyEventKind::Repeat).as_deref(),
            Some(&b"\x1b[27;1:2u"[..]),
        );
        assert_eq!(
            at(KittyKbdFlags::DISAMBIGUATE, KeyEventKind::Repeat),
            at(KittyKbdFlags::DISAMBIGUATE, KeyEventKind::Press),
            "without event types a repeat has no distinct report",
        );
        assert_eq!(
            at(KittyKbdFlags::empty(), KeyEventKind::Repeat).as_deref(),
            Some(&b"\x1b"[..]),
            "legacy encodings have no event field at all",
        );
    }

    fn press_key(task: &mut SessionTask, sub: SubscriberId, key: &[u8]) {
        let end = task.handle_input(sub, InputMsg::KeyBytes(key.to_vec()));
        assert!(end.is_none(), "KeyBytes is a continuing input");
    }

    fn take_input_ownership(task: &mut SessionTask, sub: SubscriberId) {
        press_key(task, sub, b"k");
    }

    fn effective_bg(task: &SessionTask) -> Option<(u8, u8, u8)> {
        task.session
            .lock_core()
            .grid
            .theme_config_default(ThemeChannel::Background)
    }

    fn effective_dark(task: &SessionTask) -> bool {
        task.session.lock_core().grid.os_dark() == Some(true)
    }

    fn stored_of(task: &SessionTask, sub: SubscriberId) -> (Option<(u8, u8, u8)>, Option<bool>) {
        let s = task
            .subs
            .iter()
            .find(|s| s.id == sub)
            .expect("subscriber still attached");
        (
            s.presentation
                .theme
                .and_then(|trio| trio[ThemeChannel::Background as usize]),
            s.presentation.os_dark,
        )
    }

    /// Client-reported theme and OS scheme die with the last window; an
    /// `Ops` reader still attached does not keep them alive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn last_window_detach_clears_client_reported_theme_and_scheme() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (win, _rx) = attach_sub(&mut task, false);
        let (ops, _rx_ops) = attach_ops_sub(&mut task);
        report_theme(&mut task, win, Some((0x0D, 0x0D, 0x12)));
        report_scheme(&mut task, win, true);
        report_theme(&mut task, ops, Some((0xFF, 0xFF, 0xFF)));
        report_scheme(&mut task, ops, false);
        assert_eq!(effective_bg(&task), Some((0x0D, 0x0D, 0x12)));
        assert!(effective_dark(&task));

        task.remove_sub(win);

        let core = task.session.lock_core();
        for channel in [
            ThemeChannel::Foreground,
            ThemeChannel::Background,
            ThemeChannel::Cursor,
        ] {
            assert_eq!(
                core.grid.theme_config_default(channel),
                None,
                "theme must not outlive the last window",
            );
        }
        assert_eq!(
            core.grid.color_scheme_report_bytes(),
            b"\x1b[?997;2n",
            "a cleared OS preference answers the no-report default (light)",
        );
    }

    /// A mirror's later report is stored but changes nothing a query
    /// can see while the owner has reported.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_mirrors_report_is_stored_but_the_owner_still_answers() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (a, mut rx_a) = attach_sub(&mut task, false);
        let (b, mut rx_b) = attach_sub(&mut task, false);
        take_input_ownership(&mut task, a);
        report_theme(&mut task, a, Some((0x11, 0x11, 0x11)));
        report_scheme(&mut task, a, false);
        report_theme(&mut task, a, Some((0x11, 0x11, 0x11)));
        report_scheme(&mut task, a, false);

        report_theme(&mut task, b, Some((0x22, 0x22, 0x22)));
        report_scheme(&mut task, b, true);

        assert_eq!(
            effective_bg(&task),
            Some((0x11, 0x11, 0x11)),
            "the owner keeps the trio",
        );
        assert!(!effective_dark(&task), "the owner keeps the OS scheme");
        assert_eq!(
            stored_of(&task, b),
            (Some((0x22, 0x22, 0x22)), Some(true)),
            "the mirror's report is kept for when it wins the chain",
        );
        assert!(
            drain_events(&mut rx_a).is_empty() && drain_events(&mut rx_b).is_empty(),
            "presentation reports are query-answering state: nothing is pushed to any window",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ownership_transfer_flips_both_facets_with_no_new_report() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (a, _rx_a) = attach_sub(&mut task, false);
        let (b, _rx_b) = attach_sub(&mut task, false);
        report_theme(&mut task, a, Some((0x11, 0x11, 0x11)));
        report_scheme(&mut task, a, false);
        report_theme(&mut task, b, Some((0x22, 0x22, 0x22)));
        report_scheme(&mut task, b, true);
        take_input_ownership(&mut task, a);
        assert_eq!(effective_bg(&task), Some((0x11, 0x11, 0x11)));
        assert!(!effective_dark(&task));

        take_input_ownership(&mut task, b);

        assert_eq!(effective_bg(&task), Some((0x22, 0x22, 0x22)));
        assert!(effective_dark(&task));
    }

    /// The two facets resolve apart: the trio and the OS scheme may come
    /// from different windows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_trio_and_the_os_scheme_can_come_from_different_windows() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (a, _rx_a) = attach_sub(&mut task, false);
        let (b, _rx_b) = attach_sub(&mut task, false);
        take_input_ownership(&mut task, a);
        report_scheme(&mut task, a, true);
        report_theme(&mut task, b, Some((0x22, 0x22, 0x22)));

        assert!(effective_dark(&task), "the owner supplies the scheme");
        assert_eq!(
            effective_bg(&task),
            Some((0x22, 0x22, 0x22)),
            "the only window that reported a trio supplies it",
        );
    }

    /// An `Ops` attach's reports never enter the chain, even as the sole
    /// subscriber or size owner.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scripted_subscriber_is_never_a_presentation_candidate() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (ops, _rx_ops) = attach_ops_sub(&mut task);
        report_theme(&mut task, ops, Some((0x22, 0x22, 0x22)));
        report_scheme(&mut task, ops, true);

        assert_eq!(effective_bg(&task), None, "no window has reported");
        assert!(!effective_dark(&task));

        take_input_ownership(&mut task, ops);
        assert_eq!(effective_bg(&task), None);
        assert!(!effective_dark(&task));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_owner_detach_falls_back_to_the_newest_remaining_window() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (a, _rx_a) = attach_sub(&mut task, false);
        let (b, _rx_b) = attach_sub(&mut task, false);
        let (c, _rx_c) = attach_sub(&mut task, false);
        report_theme(&mut task, b, Some((0x22, 0x22, 0x22)));
        report_scheme(&mut task, b, false);
        report_theme(&mut task, c, Some((0x33, 0x33, 0x33)));
        report_scheme(&mut task, c, true);
        take_input_ownership(&mut task, a);
        report_theme(&mut task, a, Some((0x11, 0x11, 0x11)));
        assert_eq!(effective_bg(&task), Some((0x11, 0x11, 0x11)));

        task.remove_sub(a);

        assert_eq!(
            effective_bg(&task),
            Some((0x33, 0x33, 0x33)),
            "the newest surviving window, not the oldest",
        );
        assert!(effective_dark(&task), "and its scheme with it");
    }

    const CAPTURE_READY: &str = "CAPTURE-READY";

    /// A child that saves its own stdin to `path`.
    ///
    /// Uses `stty raw -echo` so line discipline does not hold bytes awaiting newline
    /// and echo does not feed stdin back into the parser. `emit` runs after `stty`.
    fn capture_session(path: &std::path::Path, emit: &str) -> Session {
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            &format!(
                "stty raw -echo; printf '{emit}'; printf {CAPTURE_READY}; exec cat > {}",
                path.display()
            ),
        ]);
        cmd.env_clear();
        cmd.env("PATH", crate::fixture_path());
        Session::from_spawned(crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"))
    }

    /// Waits for the child's `stty` to land; bytes written before it
    /// would sit in the line discipline and miss the capture.
    async fn capture_task(path: &std::path::Path) -> SessionTask {
        let task = SessionTask::for_tests(capture_session(path, ""));
        wait_for_grid_text(&task, CAPTURE_READY).await;
        task
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    async fn capture_through(path: &std::path::Path, sentinel: &[u8]) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let seen = std::fs::read(path).unwrap_or_default();
            if find_bytes(&seen, sentinel).is_some() {
                return seen;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the PTY never received {sentinel:?}; captured: {seen:?}",
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The mode-2031 notification tracks the effective scheme, not each
    /// report, and a transfer writes scheme report, then resize, then
    /// keystroke, in that order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mode_2031_reports_effective_scheme_changes_before_the_keystroke() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pty-in");
        let mut task = capture_task(&path).await;
        {
            let mut core = task.session.lock_core();
            let mut parser = felis_vt::Parser::new();
            parser.advance(&mut core.grid, b"\x1b[?2031h");
            assert!(core.grid.color_scheme_notify(), "the program opted in");
        }
        let (a, _rx_a) = attach_sub(&mut task, false);
        let (b, _rx_b) = attach_sub(&mut task, false);

        press_key(&mut task, a, b"A");
        report_scheme(&mut task, a, false);
        report_scheme(&mut task, b, true);
        let end = task.handle_input(
            b,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: 30,
                    cols: 120,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        );
        assert!(end.is_none(), "resize is a continuing input");
        assert_eq!(
            task.session.lock_core().grid.rows(),
            crate::DEFAULT_ROWS,
            "a non-active mirror's resize is stored, not applied",
        );
        assert!(
            task.pty_steps.is_empty(),
            "nothing PTY-visible has happened yet: {:?}",
            task.pty_steps,
        );

        press_key(&mut task, b, b"B");

        assert_eq!(
            task.pty_steps,
            [PtyStep::SchemeReport, PtyStep::Resize],
            "the transfer's scheme report must precede the resize it also triggers",
        );
        assert_eq!(task.session.lock_core().grid.rows(), 30);

        report_scheme(&mut task, b, true);
        let (_c, _rx_c) = attach_sub(&mut task, false);
        press_key(&mut task, b, b"Z");

        let seen = capture_through(&path, b"Z").await;
        let report = find_bytes(&seen, b"\x1b[?997;1n").expect("the transfer notified");
        let keystroke = find_bytes(&seen, b"B").expect("the keystroke reached the PTY");
        assert!(
            report < keystroke,
            "the new owner's scheme must land before the bytes of the event that transferred it: {seen:?}",
        );
        assert_eq!(
            count_bytes(&seen, b"\x1b[?997;1n"),
            1,
            "a same-value re-report and an attach notify nobody: {seen:?}",
        );
        assert_eq!(
            count_bytes(&seen, b"\x1b[?997;2n"),
            0,
            "nothing moved the answer back to light: {seen:?}",
        );
    }

    /// A detach that moves the effective scheme owes a report, written
    /// by the run loop's reconcile.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_detach_that_moves_the_effective_scheme_notifies_mode_2031() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pty-in");
        let mut task = capture_task(&path).await;
        {
            let mut core = task.session.lock_core();
            let mut parser = felis_vt::Parser::new();
            parser.advance(&mut core.grid, b"\x1b[?2031h");
        }
        let (a, _rx_a) = attach_sub(&mut task, false);
        let (b, _rx_b) = attach_sub(&mut task, false);
        report_scheme(&mut task, a, false);
        press_key(&mut task, b, b"B");
        report_scheme(&mut task, b, true);
        assert!(effective_dark(&task));

        task.remove_sub(b);

        assert!(!effective_dark(&task), "the survivor's light report wins");
        assert!(
            task.scheme_dirty,
            "the sync detach path leaves the reconcile to the run loop",
        );
        let end = task.sync_color_scheme_to_pty();
        assert!(end.is_none());
        let seen = capture_through(&path, b"\x1b[?997;2n").await;
        assert_eq!(
            count_bytes(&seen, b"\x1b[?997;2n"),
            1,
            "one report per effective change: {seen:?}",
        );

        task.remove_sub(a);
        let end = task.sync_color_scheme_to_pty();
        assert!(end.is_none());
        assert_eq!(
            task.pty_steps,
            [PtyStep::SchemeReport, PtyStep::SchemeReport],
            "clearing the last light report still answers light, which the program was already told: {:?}",
            task.pty_steps,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pass_that_returns_to_the_reported_scheme_notifies_nobody() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        {
            let mut core = task.session.lock_core();
            let mut parser = felis_vt::Parser::new();
            parser.advance(&mut core.grid, b"\x1b[?2031h");
        }
        let (a, _rx_a) = attach_sub(&mut task, false);
        let (b, _rx_b) = attach_sub(&mut task, false);
        take_input_ownership(&mut task, a);
        report_scheme(&mut task, a, false);
        report_scheme(&mut task, b, true);
        assert!(!effective_dark(&task), "the owner answers light");
        assert!(task.pty_steps.is_empty(), "light was already the answer");

        task.remove_sub(a);
        task.remove_sub(b);

        let end = task.sync_color_scheme_to_pty();
        assert!(end.is_none());
        assert!(
            task.pty_steps.is_empty(),
            "the program was never told dark, so it is owed no return to light: {:?}",
            task.pty_steps,
        );
    }

    /// No daemon-initiated report (`DECSET 2031` scheme, `DECSET 1004`
    /// focus) reaches a PTY at EOF. Both the silence and the session
    /// staying alive are asserted: on the BSDs the write would fail
    /// with `EIO` and the next one tear the session down; on Linux only
    /// the silence is observable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_report_after_pty_eof_never_reaches_the_dead_pty() {
        let mut task = SessionTask::for_tests(session_with("exit 0"));
        {
            let mut core = task.session.lock_core();
            let mut parser = felis_vt::Parser::new();
            parser.advance(&mut core.grid, b"\x1b[?2031h\x1b[?1004h");
        }
        let mut eof_buf = [0u8; 1];
        let read = tokio::time::timeout(
            Duration::from_secs(10),
            task.session.reader.read(&mut eof_buf),
        )
        .await
        .expect("the shell exited within the deadline")
        .expect("the lifecycle reader reports EOF, not an error");
        assert_eq!(read, 0, "the sink-mode reader carries no data");
        task.pty_eof = true;
        task.evict_all(None);

        let (win, _rx) = attach_sub(&mut task, false);
        // Looped over a sleep: the writer thread fails asynchronously,
        // and it is the write after the failed one that reports dead.
        for _ in 0..5 {
            for on in [true, false] {
                assert!(
                    task.handle_input(win, InputMsg::ColorScheme { dark: on })
                        .is_none(),
                    "a presentation report must not end a session whose PTY is at EOF",
                );
                assert!(
                    task.handle_input(win, InputMsg::FocusChange { focused: on })
                        .is_none(),
                    "a focus report must not end a session whose PTY is at EOF",
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.subs[0].focused = true;
        task.focus_dirty = true;
        assert!(task.sync_focus_to_pty().is_none());
        assert!(
            task.pty_steps.is_empty(),
            "no daemon-initiated report reaches a PTY at EOF: {:?}",
            task.pty_steps,
        );
    }

    /// Session focus is the OR over window subscribers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_background_window_blur_does_not_unfocus_the_session() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (a, _rx_a) = attach_sub(&mut task, false);
        let (b, _rx_b) = attach_sub(&mut task, false);

        let end = task.handle_input(a, InputMsg::FocusChange { focused: true });
        assert!(end.is_none());
        assert!(task.reported_focus, "first focus is a session edge");

        let end = task.handle_input(b, InputMsg::FocusChange { focused: false });
        assert!(end.is_none());
        assert!(
            task.reported_focus,
            "a background blur must not un-focus the session",
        );

        task.remove_sub(a);
        assert!(task.focus_dirty);
        let end = task.sync_focus_to_pty();
        assert!(end.is_none());
        assert!(!task.reported_focus, "the focused window left");
    }

    /// `InputMsg::JumpPrompt` (docs/explanation/data-model/scrollback.md)
    /// walks the per-subscriber viewport prompt-to-prompt and
    /// hard-clamps at the oldest.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_jump_prompt_walks_the_viewport_through_marks() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let mut parser = felis_vt::Parser::new();
        for _ in 0..3 {
            parser.advance(&mut task.session.lock_core().grid, b"\x1b]133;A\x07");
            parser.advance(
                &mut task.session.lock_core().grid,
                "out\r\n".repeat(40).as_bytes(),
            );
        }
        let (sub, mut rx) = attach_sub(&mut task, false);

        let mut up = Vec::new();
        for _ in 0..4 {
            task.handle_input(
                sub,
                InputMsg::JumpPrompt {
                    direction: PromptJump::Previous,
                },
            );
            up.push(task.subs[0].stream.diff.viewport);
            drain_events(&mut rx);
        }
        assert!(
            up[0] > 0 && up[1] > up[0] && up[2] > up[1],
            "each Previous jumps to an older prompt (strictly increasing viewport): {up:?}",
        );
        assert_eq!(
            up[3], up[2],
            "Previous at the oldest prompt is a no-op (hard clamp)"
        );

        task.handle_input(
            sub,
            InputMsg::JumpPrompt {
                direction: PromptJump::Next,
            },
        );
        let down1 = task.subs[0].stream.diff.viewport;
        assert!(down1 < up[2], "Next jumps toward the live bottom");
    }

    /// The shared meta follows every `OSC 2` / `OSC 7`, not just the
    /// first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn meta_follows_every_title_and_cwd_change() {
        let mut task = SessionTask::for_tests(session_with(
            "printf '\\033]2;first\\007\\033]7;file:///first\\007'; read _x; \
             printf '\\033]2;second\\007\\033]7;file:///second\\007'; read _y",
        ));
        let (sub, _rx) = attach_sub(&mut task, false);

        wait_for_grid_title(&task, "first").await;
        task.after_drive();
        {
            let meta = task.meta.lock().unwrap();
            assert_eq!(meta.title.as_deref(), Some("first"));
            assert_eq!(meta.cwd.as_deref(), Some("file:///first"));
        }

        let end = task.handle_input(sub, InputMsg::KeyBytes(b"\n".to_vec()));
        assert!(end.is_none());
        wait_for_grid_title(&task, "second").await;
        task.after_drive();
        let meta = task.meta.lock().unwrap();
        assert_eq!(meta.title.as_deref(), Some("second"));
        assert_eq!(meta.cwd.as_deref(), Some("file:///second"));
    }

    async fn wait_for_grid_title(task: &SessionTask, expect: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let seen = task.session.lock_core().grid.title().map(str::to_owned);
            if seen.as_deref() == Some(expect) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "grid title never became {expect:?} (last: {seen:?})"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// `InputMsg::Resize` resizes the daemon-side grid and the shared
    /// meta, not just the PTY.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_resize_resizes_pty_grid_and_meta() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        assert_eq!(task.session.lock_core().grid.rows(), crate::DEFAULT_ROWS);
        assert_eq!(task.session.lock_core().grid.cols(), crate::DEFAULT_COLS);
        let (sub, _rx) = attach_sub(&mut task, false);

        let end = task.handle_input(
            sub,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: 30,
                    cols: 120,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        );
        assert!(end.is_none(), "resize is a continuing input");
        assert_eq!(task.session.lock_core().grid.rows(), 30);
        assert_eq!(task.session.lock_core().grid.cols(), 120);
        let meta = task.meta.lock().unwrap().clone();
        assert_eq!((meta.rows, meta.cols), (30, 120));
    }

    /// A geometry change under `DECSET 2048` writes exactly one
    /// `CSI 48 ; rows ; cols ; h ; w t` to the PTY; the same resize
    /// with the mode unset writes nothing, and a same-dims one repeats
    /// nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mode_2048_reports_a_resize_only_while_the_program_opted_in() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pty-in");
        let mut task = capture_task(&path).await;
        let (sub, _rx) = attach_sub(&mut task, false);

        assert!(resize_to(&mut task, sub, 30, 120).is_none());
        assert!(task.sync_resize_notify_to_pty().is_none());
        assert!(
            !task.pty_steps.contains(&PtyStep::ResizeReport),
            "the program never opted in, so the resize owes no report: {:?}",
            task.pty_steps,
        );

        set_mode_2048(&task, true);
        assert!(task.sync_resize_notify_to_pty().is_none());
        assert!(resize_to(&mut task, sub, 31, 121).is_none());
        assert!(task.resize_notify_dirty, "the run loop owes the report");
        assert!(task.sync_resize_notify_to_pty().is_none());
        assert!(resize_to(&mut task, sub, 31, 121).is_none());
        assert!(task.sync_resize_notify_to_pty().is_none());

        let sentinel = b"\x1b[48;31;121;0;0t";
        let seen = capture_through(&path, sentinel).await;
        assert_eq!(
            count_bytes(&seen, b"\x1b[48;30;120;0;0t"),
            1,
            "the set answers the geometry it landed on: {seen:?}",
        );
        assert_eq!(
            count_bytes(&seen, sentinel),
            1,
            "one report per geometry change, and none for a same-dims resize: {seen:?}",
        );
    }

    /// A `DECSET 2048` that lands on a resize the reconcile has not
    /// drained yet answers once, with the geometry that is live when
    /// the report is written, never the pre-set geometry after the
    /// post-set one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mode_2048_collapses_a_pending_resize_and_the_set_into_one_report() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pty-in");
        let mut task = capture_task(&path).await;
        let (sub, _rx) = attach_sub(&mut task, false);

        assert!(resize_to(&mut task, sub, 30, 120).is_none());
        assert!(task.resize_notify_dirty, "the resize left the edge");
        set_mode_2048(&task, true);
        assert!(resize_to(&mut task, sub, 31, 121).is_none());

        assert!(task.sync_resize_notify_to_pty().is_none());
        assert!(task.sync_resize_notify_to_pty().is_none());
        assert_eq!(
            task.pty_steps
                .iter()
                .filter(|step| **step == PtyStep::ResizeReport)
                .count(),
            1,
            "one report for one geometry: {:?}",
            task.pty_steps,
        );

        let sentinel = b"\x1b[48;31;121;0;0t";
        let seen = capture_through(&path, sentinel).await;
        assert_eq!(
            count_bytes(&seen, b"\x1b[48;30;120;0;0t"),
            0,
            "the geometry the set left behind is never reported: {seen:?}",
        );
    }

    /// A `DECRST 2048` coalesced with a re-`DECSET` in one parse burst
    /// still answers the fresh set.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mode_2048_answers_a_set_that_follows_a_coalesced_reset() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pty-in");
        let mut task = capture_task(&path).await;
        let (sub, _rx) = attach_sub(&mut task, false);

        assert!(resize_to(&mut task, sub, 30, 120).is_none());
        set_mode_2048(&task, true);
        assert!(task.sync_resize_notify_to_pty().is_none());

        set_mode_2048(&task, false);
        set_mode_2048(&task, true);
        assert!(task.sync_resize_notify_to_pty().is_none());
        press_key(&mut task, sub, b"Z");

        let seen = capture_through(&path, b"Z").await;
        assert_eq!(
            count_bytes(&seen, b"\x1b[48;30;120;0;0t"),
            2,
            "each set answers, even at an unchanged geometry: {seen:?}",
        );
    }

    /// A resize triggered by size-ownership transfer reports before the
    /// bytes of the input that transferred it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mode_2048_reports_a_transfer_before_the_triggering_keystroke() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pty-in");
        let mut task = capture_task(&path).await;
        set_mode_2048(&task, true);
        let (a, _rx_a) = attach_sub(&mut task, false);
        let (b, _rx_b) = attach_sub(&mut task, false);

        press_key(&mut task, a, b"A");
        assert!(resize_to(&mut task, b, 30, 120).is_none());
        task.pty_steps.clear();

        press_key(&mut task, b, b"B");

        assert_eq!(
            task.pty_steps,
            [PtyStep::Resize, PtyStep::ResizeReport],
            "the transfer's resize report must precede the keystroke that triggered it",
        );
        let seen = capture_through(&path, b"B").await;
        let report = find_bytes(&seen, b"\x1b[48;30;120;0;0t").expect("the transfer notified");
        let keystroke = find_bytes(&seen, b"B").expect("the keystroke reached the PTY");
        assert!(
            report < keystroke,
            "the new geometry must land before the bytes of the event that transferred it: {seen:?}",
        );
    }

    /// The production trigger, with no helper standing in for the
    /// wiring: a `DECSET 2048` written by the child as PTY output is
    /// parsed by the real sink, drained by the run loop's own
    /// `drain_effects`, and answered exactly once by the reconcile at
    /// the bottom of that loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mode_2048_set_over_the_pty_is_answered_by_the_run_loop() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pty-in");
        let task = SessionTask::for_tests(capture_session(&path, "\\033[?2048h"));
        let (rows, cols) = {
            let core = task.session.lock_core();
            (core.grid.rows(), core.grid.cols())
        };
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let loop_done = tokio::spawn(run_session(task, cmd_rx));

        let report = format!("\x1b[48;{rows};{cols};0;0t");
        let seen = capture_through(&path, report.as_bytes()).await;
        assert_eq!(
            count_bytes(&seen, report.as_bytes()),
            1,
            "the set owes one report, and the loop's repeated reconciles owe no more: {seen:?}",
        );

        drop(cmd_tx);
        loop_done
            .await
            .expect("the run loop ends on a dropped pool");
    }

    /// Real time, counted in flood chunks: under chunks that hold the
    /// lock past `DRAIN_COALESCE`, the run loop still yields between
    /// drains, so a pull arriving through the runtime is answered. On
    /// this single-threaded runtime the test body's timer fires only
    /// when the loop yields.
    #[tokio::test(flavor = "current_thread")]
    async fn a_pull_sent_mid_flood_is_answered_within_a_few_chunks_real_time() {
        use std::sync::atomic::{AtomicBool, AtomicU32};

        const HOLD: Duration = Duration::from_millis(6);
        const FLOOD_CAP: Duration = Duration::from_secs(5);
        const MAX_LAG_CHUNKS: u32 = 20;
        assert!(HOLD > DRAIN_COALESCE, "each drain must outwait the window");

        let mut task = SessionTask::for_tests(session_with("read -r _"));
        let (sub, mut rx) = attach_sub(&mut task, true);
        let core = Arc::clone(&task.session.core);
        let signals = Arc::clone(&task.session.signals);
        let chunks = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        // Stands in for the parse sink with chunks queued behind the
        // one in hand: it retakes the lock the moment the task drops
        // it, and the edge for the next chunk is already raised when
        // the lock changes hands, so every drain waits out a whole
        // chunk and finds more to drain behind it.
        let flood = std::thread::spawn({
            let chunks = Arc::clone(&chunks);
            let stop = Arc::clone(&stop);
            move || {
                let started = Instant::now();
                while !stop.load(Ordering::Relaxed) && started.elapsed() < FLOOD_CAP {
                    let n = chunks.load(Ordering::Relaxed);
                    {
                        let mut guard = loop {
                            if let Some(guard) = core.try_lock() {
                                break guard;
                            }
                            std::hint::spin_loop();
                        };
                        let core = &mut *guard;
                        core.parser
                            .advance(&mut core.grid, format!("\r{n}").as_bytes());
                        std::thread::sleep(HOLD);
                        signals.mark_dirty();
                    }
                    chunks.store(n.wrapping_add(1), Ordering::Relaxed);
                }
                stop.store(true, Ordering::Relaxed);
            }
        });

        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
        let loop_done = tokio::spawn(run_session(task, cmd_rx));
        let mut lags = Vec::new();
        for _ in 0..3 {
            let before = chunks.load(Ordering::Relaxed);
            // About five chunks' worth when the loop yields.
            tokio::time::sleep(HOLD * 5).await;
            cmd_tx
                .send(SessionCmd::Input {
                    sub,
                    msg: InputMsg::NextGridFrame,
                    reservation: None,
                })
                .await
                .map_err(|_| ())
                .expect("the session task accepts the pull");
            // A pull that lands after the flood finds nothing to ship
            // and stays armed, so the cycle it waits for never comes.
            let answered = tokio::time::timeout(FLOOD_CAP, async {
                while !matches!(
                    rx.recv().await.expect("the outbox stays open"),
                    OutEvent::Grid(GridMsg::CycleEnd)
                ) {}
            })
            .await
            .is_ok();
            if !answered {
                lags.push(u32::MAX);
                break;
            }
            lags.push(chunks.load(Ordering::Relaxed).wrapping_sub(before));
        }
        let flood_outlived_the_pulls = !stop.swap(true, Ordering::Relaxed);

        flood.join().expect("the flood thread ends");
        drop(cmd_tx);
        loop_done
            .await
            .expect("the run loop ends on a dropped pool");
        assert!(
            lags.iter().all(|&lag| lag <= MAX_LAG_CHUNKS),
            "a pull waited out {lags:?} flood chunks: the run loop drained back to back without yielding",
        );
        assert!(
            flood_outlived_the_pulls,
            "the pulls must be measured under the flood"
        );
    }

    fn set_mode_2048(task: &SessionTask, on: bool) {
        let mut core = task.session.lock_core();
        let mut parser = felis_vt::Parser::new();
        parser.advance(
            &mut core.grid,
            if on { b"\x1b[?2048h" } else { b"\x1b[?2048l" },
        );
        assert_eq!(core.grid.in_band_resize_notify(), on);
    }

    fn resize_to(
        task: &mut SessionTask,
        sub: SubscriberId,
        rows: u32,
        cols: u32,
    ) -> Option<EndReason> {
        task.handle_input(
            sub,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows,
                    cols,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        )
    }

    /// Input fans in: every subscriber's `KeyBytes` reaches the one PTY
    /// writer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn key_bytes_from_any_subscriber_reach_the_pty() {
        let mut task = SessionTask::for_tests(session_with("read x; printf 'got=%s' \"$x\""));
        let (_sub_a, _rx_a) = attach_sub(&mut task, false);
        let (sub_b, _rx_b) = attach_sub(&mut task, false);

        let end = task.handle_input(sub_b, InputMsg::KeyBytes(b"HELLO\n".to_vec()));
        assert!(end.is_none());
        wait_for_grid_text(&task, "got=HELLO").await;
    }

    /// A search emits in bounded slices: the first slice reaches the
    /// subscriber while the walk still has scrollback left.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_search_ships_its_first_slice_before_the_walk_completes() {
        let mut task = SessionTask::for_tests(session_with(
            "i=0; while [ $i -lt 400 ]; do echo needle-$i; i=$((i+1)); done; read _x",
        ));
        let (sub, mut rx) = attach_sub(&mut task, false);
        wait_for_grid_text(&task, "needle-399").await;
        drain_events(&mut rx);

        let stream = test_stream(1);
        task.handle_search(
            sub,
            SearchToDaemonMsg::Query {
                query: "needle-".to_owned(),
                options: felis_protocol::messages::SearchOptions::default(),
            },
            stream,
        );
        assert!(
            task.has_pending_chunks(),
            "the walk must be queued, not run"
        );
        task.run_next_chunk();

        let first = drain_events(&mut rx);
        assert!(
            first.iter().any(|ev| matches!(ev, OutEvent::Search { .. })),
            "the first slice must carry hits, got {} events",
            first.len(),
        );
        assert!(
            !first.iter().any(|ev| matches!(
                ev,
                OutEvent::Control(ConnToClientMsg::End { .. } | ConnToClientMsg::Error { .. })
            )),
            "a walk this deep cannot finish in one slice",
        );
        assert!(
            task.has_pending_chunks(),
            "the unfinished walk must re-queue itself",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_between_slices_stops_the_walk_with_one_terminal() {
        let mut task = SessionTask::for_tests(session_with(
            "i=0; while [ $i -lt 400 ]; do echo needle-$i; i=$((i+1)); done; read _x",
        ));
        let (sub, mut rx) = attach_sub(&mut task, false);
        wait_for_grid_text(&task, "needle-399").await;
        drain_events(&mut rx);

        let stream = test_stream(1);
        task.handle_search(
            sub,
            SearchToDaemonMsg::Query {
                query: "needle-".to_owned(),
                options: felis_protocol::messages::SearchOptions::default(),
            },
            stream,
        );
        task.run_next_chunk();
        drain_events(&mut rx);

        task.cancel_producer(sub, stream);
        let mut turns = 0;
        while task.has_pending_chunks() {
            task.run_next_chunk();
            turns += 1;
            assert!(turns < 8, "a canceled producer must stop promptly");
        }
        let after = drain_events(&mut rx);
        assert!(
            !after.iter().any(|ev| matches!(ev, OutEvent::Search { .. })),
            "no hit may be emitted after the cancel",
        );
        let terminals = after
            .iter()
            .filter(|ev| {
                matches!(
                    ev,
                    OutEvent::Control(ConnToClientMsg::End { .. } | ConnToClientMsg::Error { .. })
                )
            })
            .count();
        assert_eq!(terminals, 1, "exactly one terminal, even under cancel");
    }

    /// An unresolvable needle ends the stream with a typed error, not
    /// an empty success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rejected_needle_ends_the_stream_with_a_typed_error() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub, mut rx) = attach_sub(&mut task, false);
        drain_events(&mut rx);

        let stream = test_stream(1);
        task.handle_search(
            sub,
            SearchToDaemonMsg::Query {
                query: String::new(),
                options: felis_protocol::messages::SearchOptions::default(),
            },
            stream,
        );
        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                OutEvent::Control(ConnToClientMsg::Error {
                    subject: Subject::Stream(id),
                    reason: StreamErrorReason::InvalidRequest,
                    ..
                }) if *id == stream
            )),
            "an empty needle must terminate the stream typed, got {events:?}",
        );
        assert!(
            !task.has_pending_chunks(),
            "a refused query must queue no walk",
        );
    }

    /// A row stream's terminal follows `RowsDone` rather than replacing
    /// it, and its count is the rows emitted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_row_stream_ends_with_rows_done_then_its_terminal() {
        let mut task = SessionTask::for_tests(session_with("printf 'CAPTURE\\n'; read _x"));
        let (sub, mut rx) = attach_sub(&mut task, false);
        wait_for_grid_text(&task, "CAPTURE").await;
        drain_events(&mut rx);

        let stream = test_stream(1);
        task.start_rows(sub, RegionSource::Visible, false, None, stream);
        while task.has_pending_chunks() {
            task.run_next_chunk();
        }
        let events = drain_events(&mut rx);
        let done_at = events.iter().position(|ev| {
            matches!(
                ev,
                OutEvent::Region {
                    msg: RegionToClientMsg::RowsDone { .. },
                    ..
                }
            )
        });
        let end_at = events
            .iter()
            .position(|ev| matches!(ev, OutEvent::Control(ConnToClientMsg::End { .. })));
        assert!(
            matches!((done_at, end_at), (Some(d), Some(e)) if d < e),
            "RowsDone must precede the stream terminal, got {events:?}",
        );

        let rows = events
            .iter()
            .filter(|ev| {
                matches!(
                    ev,
                    OutEvent::Region {
                        msg: RegionToClientMsg::Row { .. },
                        ..
                    }
                )
            })
            .count();
        let count = events
            .iter()
            .find_map(|ev| match ev {
                OutEvent::Control(ConnToClientMsg::End { count, .. }) => Some(*count),
                _ => None,
            })
            .expect("the stream terminal");
        assert_eq!(
            usize::try_from(count).expect("a count fits usize"),
            rows,
            "the terminal's count is the rows emitted, got {events:?}",
        );
    }

    /// A region that resolves to nothing still ends with a terminal, at
    /// `count == 0`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_region_stream_ends_with_a_zero_count() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub, mut rx) = attach_sub(&mut task, false);
        drain_events(&mut rx);

        let stream = test_stream(1);
        task.start_rows(sub, RegionSource::LastCommand, false, None, stream);
        while task.has_pending_chunks() {
            task.run_next_chunk();
        }
        let events = drain_events(&mut rx);
        let count = events
            .iter()
            .find_map(|ev| match ev {
                OutEvent::Control(ConnToClientMsg::End { count, .. }) => Some(*count),
                _ => None,
            })
            .expect("the stream terminal");
        assert_eq!(
            count, 0,
            "an unresolved region emits no rows, got {events:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn eviction_terminates_a_subscribers_open_streams() {
        let mut task = SessionTask::for_tests(session_with(
            "i=0; while [ $i -lt 400 ]; do echo needle-$i; i=$((i+1)); done; read _x",
        ));
        let (sub, mut rx) = attach_sub(&mut task, false);
        wait_for_grid_text(&task, "needle-399").await;
        drain_events(&mut rx);

        let stream = test_stream(1);
        task.handle_search(
            sub,
            SearchToDaemonMsg::Query {
                query: "needle-".to_owned(),
                options: felis_protocol::messages::SearchOptions::default(),
            },
            stream,
        );
        task.run_next_chunk();
        drain_events(&mut rx);

        task.evict_all(Some("forced"));
        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                OutEvent::Control(ConnToClientMsg::Error {
                    subject: Subject::Stream(id),
                    reason: StreamErrorReason::Unavailable,
                    ..
                }) if *id == stream
            )),
            "the open stream must be failed typed on eviction, got {events:?}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_push_evicting_a_window_terminates_its_open_streams() {
        let mut task = SessionTask::for_tests(session_with(
            "i=0; while [ $i -lt 400 ]; do echo needle-$i; i=$((i+1)); done; read _x",
        ));
        let (sub, mut rx) = attach_sub(&mut task, false);
        wait_for_grid_text(&task, "needle-399").await;
        drain_events(&mut rx);

        let stream = test_stream(1);
        task.handle_search(
            sub,
            SearchToDaemonMsg::Query {
                query: "needle-".to_owned(),
                options: felis_protocol::messages::SearchOptions::default(),
            },
            stream,
        );
        task.run_next_chunk();
        drain_events(&mut rx);

        let idx = task.sub_index(sub).expect("subscriber present");
        task.subs[idx].buffered.store(
            SUBSCRIBER_BUFFER_CAP - WIRE_LEN_OVERHEAD + 1,
            Ordering::Relaxed,
        );

        let accepted =
            task.push_to_scope(&PushMsg::Reattach { id: task.id.0 }, SwitchScope::Default);
        assert_eq!(queued(accepted), 0, "an over-cap window accepts nothing");
        assert!(
            task.sub_index(sub).is_none(),
            "an over-cap window departs on the push",
        );
        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                OutEvent::Control(ConnToClientMsg::Error {
                    subject: Subject::Stream(id),
                    reason: StreamErrorReason::Unavailable,
                    ..
                }) if *id == stream
            )),
            "the open stream must be failed typed when a push evicts, got {events:?}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn region_reply_reaches_the_originator_only() {
        let mut task = SessionTask::for_tests(session_with("printf 'CLIPME'; read _x"));
        let (sub_a, mut rx_a) = attach_sub(&mut task, false);
        let (_sub_b, mut rx_b) = attach_sub(&mut task, false);
        wait_for_grid_text(&task, "CLIPME").await;

        task.handle_region(sub_a, RegionSource::Visible, false, test_request());

        let a = drain_events(&mut rx_a);
        let data = a
            .iter()
            .find_map(|ev| match ev {
                OutEvent::Region {
                    msg: RegionToClientMsg::Reply { data, .. },
                    ..
                } => Some(data.clone()),
                _ => None,
            })
            .expect("originating subscriber must receive a region reply");
        assert!(
            String::from_utf8_lossy(&data).contains("CLIPME"),
            "the reply must carry the serialized region, got {:?}",
            String::from_utf8_lossy(&data),
        );
        assert!(
            !drain_events(&mut rx_b).iter().any(|ev| matches!(
                ev,
                OutEvent::Region {
                    msg: RegionToClientMsg::Reply { .. },
                    ..
                }
            )),
            "a region read is the originator's action, not broadcast",
        );
    }

    fn fence_replies(events: &[OutEvent]) -> Vec<Correlation> {
        events
            .iter()
            .filter_map(|ev| match ev {
                OutEvent::Session {
                    msg: SessionToClientMsg::InputAccepted,
                    correlation,
                } => Some(*correlation),
                _ => None,
            })
            .collect()
    }

    fn fence(task: &mut SessionTask, sub: SubscriberId) -> Option<EndReason> {
        task.handle_cmd(SessionCmd::InputFence {
            sub,
            request: test_request(),
        })
    }

    /// The reply is the requester's alone, and carries the id of the
    /// fence it answers: a caller with two barriers outstanding pairs
    /// them by nothing else.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fence_answers_the_subscriber_that_asked_on_its_own_request_id() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_a, mut rx_a) = attach_sub(&mut task, false);
        let (_sub_b, mut rx_b) = attach_sub(&mut task, false);

        assert!(fence(&mut task, sub_a).is_none());

        assert_eq!(
            fence_replies(&drain_events(&mut rx_a)),
            vec![Correlation::request(test_request())],
        );
        assert!(
            fence_replies(&drain_events(&mut rx_b)).is_empty(),
            "a barrier is the asking connection's own, never a broadcast",
        );
    }

    /// A resize generates its in-band report only under DECSET 2048, so
    /// on a grid that asked for none the fence behind it answers with
    /// nothing written to the child.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resize_before_a_fence_writes_nothing_and_still_answers() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub, mut rx) = attach_sub(&mut task, false);

        assert!(
            task.handle_input(
                sub,
                InputMsg::Resize {
                    dims: RequestedDims {
                        rows: 30,
                        cols: 100,
                        pixel_w: 0,
                        pixel_h: 0,
                    },
                },
            )
            .is_none()
        );
        assert!(fence(&mut task, sub).is_none());

        assert_eq!(fence_replies(&drain_events(&mut rx)).len(), 1);
        assert!(
            !task.pty_steps.contains(&PtyStep::ResizeReport),
            "a grid that asked for no resize report gets none: {:?}",
            task.pty_steps,
        );
    }

    /// Order, not delivery: the report a focus change generates is
    /// attempted before the fence is answered, and on a writer with
    /// room it reaches the PTY. The reply still says only that the
    /// input was processed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_focus_report_is_written_before_the_fence_behind_it_answers() {
        let mut task = SessionTask::for_tests(session_with(
            "printf '\\033[?1004h'; while :; do sleep 1; done",
        ));
        let (sub, mut rx) = attach_sub(&mut task, false);
        wait_for_focus_reporting(&task).await;

        assert!(
            task.handle_input(sub, InputMsg::FocusChange { focused: true })
                .is_none()
        );
        assert!(
            task.pty_steps.contains(&PtyStep::FocusReport),
            "the report is written on the input, ahead of the fence: {:?}",
            task.pty_steps,
        );
        assert!(fence(&mut task, sub).is_none());

        assert_eq!(fence_replies(&drain_events(&mut rx)).len(), 1);
        wait_for_drained_writer(&task).await;
    }

    /// The same change on a saturated writer: the report is dropped,
    /// and the fence completes anyway. A barrier that waited on a
    /// best-effort report would hang on a child that stopped reading.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_focus_report_does_not_hold_back_the_fence() {
        let mut task = SessionTask::for_tests(session_with(
            "printf '\\033[?1004h'; while :; do sleep 1; done",
        ));
        let (sub, mut rx) = attach_sub(&mut task, false);
        wait_for_focus_reporting(&task).await;
        saturate_writer(&task);

        assert!(
            task.handle_input(sub, InputMsg::FocusChange { focused: true })
                .is_none()
        );
        assert!(fence(&mut task, sub).is_none());

        assert_eq!(
            fence_replies(&drain_events(&mut rx)).len(),
            1,
            "the fence answers on admission, not on the report landing",
        );
    }

    async fn wait_for_focus_reporting(task: &SessionTask) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !task.session.lock_core().grid.focus_reporting() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the child never enabled DECSET 1004",
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_for_drained_writer(task: &SessionTask) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while task.session.writer.pending_bytes() != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the focus report never reached the PTY",
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Fills the unreserved gauge, which is what makes the next
    /// generated report a dropped one. Written in chunks because the
    /// OS writer drains into the PTY buffer while this runs.
    fn saturate_writer(task: &SessionTask) {
        for _ in 0..64 {
            match task
                .session
                .writer
                .write_owned(vec![0u8; felis_pty::PTY_WRITE_PENDING_CAP / 4], None)
                .expect("the writer is live")
            {
                felis_pty::WriteOutcome::Queued => {}
                felis_pty::WriteOutcome::Dropped { .. } => return,
            }
        }
        panic!("the unreserved gauge never reached its cap");
    }

    /// One self-delimiting token: a scalar, an escape sequence, or a
    /// newline. The generator knows every safe cut offset without
    /// walking the bytes, which is what makes it an oracle for
    /// `resumable_cut`.
    fn arb_token() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            Just(b"x".to_vec()),
            Just(b"\n".to_vec()),
            Just("\u{3042}".as_bytes().to_vec()),
            Just("\u{1F600}".as_bytes().to_vec()),
            Just(b"\x1b[31m".to_vec()),
            Just(b"\x1b]0;t\x07".to_vec()),
            Just(b"\x1bP+q436f\x1b\\".to_vec()),
            Just(b"\x1bM".to_vec()),
        ]
    }

    proptest! {
        /// An oversize region ships its youngest lines: the cut is the
        /// first line start at or after the byte bound, and where the
        /// tail holds no newline (a blob, or one long `--ansi` line) it
        /// is still a token boundary, never mid-sequence or mid-scalar.
        #[test]
        fn an_oversize_region_reply_cuts_at_the_first_safe_offset(
            tokens in prop::collection::vec(arb_token(), 1..24),
            cap_ratio in 0.0f64..1.0,
        ) {
            let mut data = Vec::new();
            let mut starts = vec![0usize];
            for t in &tokens {
                data.extend_from_slice(t);
                starts.push(data.len());
            }
            let position = Some(RegionPosition {
                top_line: 1,
                cursor_line: 2,
                cursor_column: 3,
            });

            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "ratio of a bounded length")]
            let cap = (data.len() as f64 * cap_ratio) as usize;
            let (out, out_position) = trim_region_reply(data.clone(), position, cap);
            if data.len() <= cap {
                prop_assert_eq!(out, data);
                prop_assert_eq!(out_position, position);
                return Ok(());
            }

            prop_assert_eq!(out_position, None, "a trimmed reply drops its head-relative position");
            prop_assert!(data.ends_with(&out), "the reply keeps a suffix");
            prop_assert!(out.len() <= cap, "the suffix fits the bound");

            let tail = data.len() - cap;
            let cut = data.len() - out.len();
            // The end of the buffer is not a cut: it would answer empty.
            let safe: Vec<usize> = starts
                .iter()
                .copied()
                .filter(|&b| b >= tail && b < data.len())
                .collect();
            let want = safe
                .iter()
                .copied()
                .find(|&b| b > 0 && data[b - 1] == b'\n')
                .or_else(|| safe.first().copied())
                .unwrap_or(data.len());
            prop_assert_eq!(cut, want);
        }
    }

    /// A region the daemon cannot resolve (no `OSC 133` marks yet) still
    /// answers, with empty bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unresolvable_region_still_answers_with_empty_bytes() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub, mut rx) = attach_sub(&mut task, false);
        drain_events(&mut rx);

        task.handle_region(sub, RegionSource::LastCommand, false, test_request());

        let reply = drain_events(&mut rx)
            .into_iter()
            .find_map(|ev| match ev {
                OutEvent::Region {
                    msg: msg @ RegionToClientMsg::Reply { .. },
                    ..
                } => Some(msg),
                _ => None,
            })
            .expect("an unresolvable region must still produce a reply");
        assert_eq!(
            reply,
            RegionToClientMsg::Reply {
                data: Vec::new(),
                position: None,
                exit_code: None,
            },
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paste_feeds_bytes_back_as_input() {
        let mut task = SessionTask::for_tests(session_with("read x; printf 'got=%s' \"$x\""));
        let (sub, _rx) = attach_sub(&mut task, false);

        task.handle_input(sub, InputMsg::Paste(b"PASTED\n".to_vec()));

        wait_for_grid_text(&task, "got=PASTED").await;
    }

    /// Demand-driven pacing (docs/explanation/rendering/pipeline.md): a
    /// pull with nothing dirty ships nothing and leaves the pull armed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pull_with_nothing_dirty_emits_no_redundant_frame() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub, mut rx) = attach_sub(&mut task, true);
        drain_events(&mut rx);

        let end = task.handle_input(sub, InputMsg::NextGridFrame);
        assert!(end.is_none(), "NextGridFrame is a continuing input");

        assert!(
            task.subs[0].pull_pending,
            "a clean pull stays pending — nothing was emitted to consume it"
        );
        let events = drain_events(&mut rx);
        let grid_frames = events
            .iter()
            .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. })))
            .count();
        assert_eq!(
            grid_frames, 0,
            "a pull with nothing new must not force a redundant frame"
        );
        assert!(
            !events
                .iter()
                .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::CycleEnd))),
            "an empty cycle terminates nothing: there was no cycle"
        );
    }

    /// The cycle terminator closes a non-empty pull-paced cycle: one
    /// marker, after every message that cycle composed, whether the
    /// rows arrived in one `RowDelta` frame or several.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pull_cycle_ends_with_one_marker_after_its_last_frame() {
        let mut task = SessionTask::for_tests(session_with("printf 'cycle-end-probe'; read _x"));
        let (sub, mut rx) = attach_sub(&mut task, true);
        wait_for_grid_text(&task, "cycle-end-probe").await;
        drain_events(&mut rx);

        task.handle_input(sub, InputMsg::NextGridFrame);

        let grid: Vec<GridMsg> = drain_events(&mut rx)
            .into_iter()
            .filter_map(|ev| match ev {
                OutEvent::Grid(msg) => Some(msg),
                _ => None,
            })
            .collect();
        assert!(
            grid.iter().any(|m| matches!(m, GridMsg::RowDelta { .. })),
            "the probe must produce a cycle to terminate: {grid:?}"
        );
        assert_eq!(
            grid.iter()
                .filter(|m| matches!(m, GridMsg::CycleEnd))
                .count(),
            1,
            "exactly one marker per cycle: {grid:?}"
        );
        assert_eq!(
            grid.last(),
            Some(&GridMsg::CycleEnd),
            "the marker follows every message of its cycle: {grid:?}"
        );
    }

    /// A cycle whose rows outrun the frame budget splits across
    /// consecutive `RowDelta` frames (REQ-105); the marker closes the
    /// cycle, not each frame, so it lands once, after the last split.
    /// It drives the two halves `ship_sub_at` composes, since no
    /// admitted geometry encodes to the budget.
    #[test]
    fn the_marker_follows_the_last_frame_of_a_split_cycle() {
        let oversized = streaming::OUTBOUND_BODY_BUDGET_BYTES / 2 + 1;
        let mut out = Vec::new();
        streaming::push_row_deltas(
            (0..4)
                .map(|i| (i, felis_protocol::RowPayload(vec![0u8; oversized])))
                .collect(),
            &mut out,
        );
        push_cycle_end(&mut out, true);

        let frames = out
            .iter()
            .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. })))
            .count();
        assert!(frames >= 2, "the batch must have split: {frames} frame(s)");
        assert_eq!(
            out.iter()
                .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::CycleEnd)))
                .count(),
            1,
            "one marker per cycle, not per frame"
        );
        assert!(
            matches!(out.last(), Some(OutEvent::Grid(GridMsg::CycleEnd))),
            "the marker follows every split frame"
        );
    }

    /// An eager-push peer never pulls, so it has no cycle to close and
    /// a marker would be traffic it cannot act on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_eager_push_peer_gets_no_cycle_marker() {
        let mut task = SessionTask::for_tests(session_with("printf 'eager-probe'; read _x"));
        let (_sub, mut rx) = attach_sub(&mut task, false);
        wait_for_grid_text(&task, "eager-probe").await;
        task.ship_all();

        assert!(
            !drain_events(&mut rx)
                .iter()
                .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::CycleEnd))),
            "no pull gate, no cycle"
        );
    }

    /// Active-owns-winsize: a non-owner's resize is not applied; the
    /// requester gets a `Size` correction plus a full row replay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_active_mirror_resize_is_corrected_not_applied() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_a, mut rx_a) = attach_sub(&mut task, false);
        let (sub_b, mut rx_b) = attach_sub(&mut task, false);

        task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
        assert_eq!(task.active_sub, Some(sub_a));
        drain_events(&mut rx_a);

        task.handle_input(
            sub_b,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: 50,
                    cols: 200,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        );
        assert_eq!(
            {
                let core = task.session.lock_core();
                (core.grid.rows(), core.grid.cols())
            },
            (crate::DEFAULT_ROWS, crate::DEFAULT_COLS),
            "non-active resize must not change the authoritative grid",
        );
        let b_events = drain_events(&mut rx_b);
        assert!(
            b_events.iter().any(|ev| matches!(
                ev,
                OutEvent::Grid(GridMsg::Size {
                    dims: GridDims {
                        rows: crate::DEFAULT_ROWS,
                        cols: crate::DEFAULT_COLS,
                        ..
                    },
                })
            )),
            "the overruled requester must receive the authoritative dims",
        );
        let b_rows = b_events
            .iter()
            .filter_map(|ev| match ev {
                OutEvent::Grid(GridMsg::RowDelta { rows }) => Some(rows.len()),
                _ => None,
            })
            .sum::<usize>();
        assert_eq!(
            b_rows,
            usize::from(crate::DEFAULT_ROWS),
            "the correction must replay every row at the authoritative width",
        );
        assert!(
            drain_events(&mut rx_a).is_empty(),
            "the active client must not be disturbed by an ignored resize",
        );
    }

    /// Active-owns-winsize: typing transfers size ownership, the PTY
    /// reflows to the new owner's last requested size, and every mirror
    /// hears the new dims.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_transfers_size_ownership_and_reflows() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_a, mut rx_a) = attach_sub(&mut task, false);
        let (sub_b, mut rx_b) = attach_sub(&mut task, false);

        task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
        task.handle_input(
            sub_b,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: 30,
                    cols: 120,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        );
        assert_eq!(task.session.lock_core().grid.cols(), crate::DEFAULT_COLS);
        drain_events(&mut rx_a);
        drain_events(&mut rx_b);

        task.handle_input(sub_b, InputMsg::KeyBytes(b"b".to_vec()));
        assert_eq!(task.active_sub, Some(sub_b));
        assert_eq!(
            {
                let core = task.session.lock_core();
                (core.grid.rows(), core.grid.cols())
            },
            (30, 120),
            "the new owner's last requested size must apply on transfer",
        );
        for rx in [&mut rx_a, &mut rx_b] {
            assert!(
                drain_events(rx).iter().any(|ev| matches!(
                    ev,
                    OutEvent::Grid(GridMsg::Size {
                        dims: GridDims {
                            rows: 30,
                            cols: 120,
                            ..
                        }
                    })
                )),
                "every mirror must hear the new authoritative dims",
            );
        }
    }

    fn shipped_rows(events: &[OutEvent]) -> usize {
        events
            .iter()
            .filter_map(|ev| match ev {
                OutEvent::Grid(GridMsg::RowDelta { rows }) => Some(rows.len()),
                _ => None,
            })
            .sum()
    }

    /// An authoritative resize must answer every armed pull, not only
    /// the requester's: nothing else would, and the mirror would paint
    /// rows composed at dimensions the daemon has overruled. It owes
    /// each of them one replay, so the resize consumes its own damage
    /// rather than leaving the caller's fan-out to re-arm a second.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resize_answers_an_idle_mirrors_armed_pull_exactly_once() {
        const ROWS: u16 = 30;

        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_a, mut rx_a) = attach_sub(&mut task, false);
        let (sub_b, mut rx_b) = attach_sub(&mut task, true);

        task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
        // The echo has to be on the grid and shipped before the resize,
        // or it rides the replay and the row counts below measure it.
        wait_for_grid_text(&task, "a").await;
        task.after_drive();
        drain_events(&mut rx_a);
        // One pull hands the mirror that echo, the next finds it clean
        // and stays armed, which is the state the resize has to answer.
        task.handle_input(sub_b, InputMsg::NextGridFrame);
        drain_events(&mut rx_b);
        task.handle_input(sub_b, InputMsg::NextGridFrame);
        assert!(
            drain_events(&mut rx_b).is_empty(),
            "the mirror is caught up, so its pull stays armed",
        );

        task.handle_input(
            sub_a,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: u32::from(ROWS),
                    cols: 120,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        );

        let events = drain_events(&mut rx_b);
        let size_at = events
            .iter()
            .position(|ev| matches!(ev, OutEvent::Grid(GridMsg::Size { .. })))
            .expect("the mirror hears the authoritative dims");
        let rows_at = events
            .iter()
            .position(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. })))
            .expect("and the replay that fills them, with no further activity");
        assert!(size_at < rows_at, "the rows follow the dimensions");
        assert_eq!(
            shipped_rows(&events),
            usize::from(ROWS),
            "one replay of the new grid, not two",
        );
        assert_eq!(
            shipped_rows(&drain_events(&mut rx_a)),
            usize::from(ROWS),
            "the eager requester is owed one replay too",
        );

        task.handle_input(sub_b, InputMsg::NextGridFrame);
        assert!(
            shipped_rows(&drain_events(&mut rx_b)) == 0,
            "the mirror's next clean pull finds nothing left over",
        );
    }

    fn full_screen_scroll(task: &SessionTask) -> streaming::QueuedScroll {
        let core = task.session.lock_core();
        streaming::QueuedScroll {
            geometry_gen: core.grid.geometry_gen(),
            first_seq: core.grid.scroll_seq() + 1,
            last_seq: core.grid.scroll_seq() + 1,
            op: felis_grid::ScrollOp {
                region_top: 0,
                region_bottom: core.grid.rows() - 1,
                n_rows: 1,
                direction: felis_grid::ScrollDirection::Up,
            },
        }
    }

    const fn shrunk() -> PtySize {
        PtySize {
            rows: 10,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// A directive queued for a pull-paced subscriber names the geometry
    /// it was recorded under, so a resize must retire it: shipping it
    /// after the `Size` would name rows the new grid does not have.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resize_retires_a_pending_scroll_directive() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_sub, mut rx) = attach_sub(&mut task, true);
        drain_events(&mut rx);

        task.session.scroll_ops.push(full_screen_scroll(&task));
        task.fan_out_grid_state();
        assert_eq!(
            task.subs[0].stream.pending_scrolls.len(),
            1,
            "a clean subscriber accepts the directive",
        );

        task.apply_size(shrunk());
        assert!(
            task.subs[0].stream.pending_scrolls.is_empty(),
            "the pre-resize directive must not survive the new geometry",
        );
        task.ship_all();
        assert!(
            !drain_events(&mut rx)
                .iter()
                .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Scrolled { .. }))),
            "no directive may follow the Size announcement",
        );
    }

    /// The same directive drained after the resize instead of before it:
    /// its generation is stale, so it downgrades to a row replay rather
    /// than shifting rows the reflow already moved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scroll_drained_after_a_resize_downgrades_to_a_row_replay() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_sub, mut rx) = attach_sub(&mut task, true);
        drain_events(&mut rx);

        let queued = full_screen_scroll(&task);
        task.apply_size(shrunk());
        task.subs[0].stream.damage.clear();
        task.session.scroll_ops.push(queued);
        task.fan_out_grid_state();

        assert!(
            task.subs[0].stream.pending_scrolls.is_empty(),
            "a directive from the previous geometry is never admitted",
        );
        assert_eq!(
            task.subs[0].stream.damage.dirty_rows().count(),
            10,
            "it downgrades to a replay of every row of the new grid",
        );
    }

    fn mirror_rows(shadow: &felis_client_core::ShadowScreen) -> Vec<String> {
        let screen = shadow.screen();
        let clusters = screen.cluster_table();
        (0..screen.rows())
            .map(|r| felis_grid::row_text_trim(screen.row_cells(r).unwrap_or(&[]), clusters))
            .collect()
    }

    fn grid_rows(task: &SessionTask) -> Vec<String> {
        let core = task.session.lock_core();
        let clusters = core.grid.cluster_table();
        (0..core.grid.rows())
            .map(|r| felis_grid::row_text_trim(core.grid.row_cells(r).unwrap_or(&[]), clusters))
            .collect()
    }

    fn mirror_apply(shadow: &mut felis_client_core::ShadowScreen, events: &[OutEvent]) {
        for ev in events {
            if let OutEvent::Grid(msg) = ev {
                shadow.apply(msg).expect("the mirror admits every frame");
            }
        }
    }

    /// A scroll the parse thread produces between the resize fan-out and
    /// the replay is already in the rows that replay carries: shipping
    /// the directive as well would shift them a second time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scroll_raced_with_the_resize_replay_is_not_applied_twice() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_sub, mut rx) = attach_sub(&mut task, false);
        let mut shadow = {
            let core = task.session.lock_core();
            felis_client_core::ShadowScreen::new(core.grid.rows(), core.grid.cols())
        };
        {
            let mut core = task.session.lock_core();
            let core = &mut *core;
            core.parser
                .advance(&mut core.grid, b"\x1b[H\x1b[2Jone\r\ntwo\r\nthree");
        }
        task.drain_and_fan().expect("effects drain");
        task.ship_all();
        mirror_apply(&mut shadow, &drain_events(&mut rx));

        // The replay `apply_size` arms, fanned out before it is encoded.
        task.subs[0].stream.damage.mark_all();
        task.fan_out_grid_state();
        {
            let mut core = task.session.lock_core();
            let core = &mut *core;
            core.parser.advance(&mut core.grid, b"\x1b[S");
        }
        task.drain_effects().expect("effects drain");
        task.ship_all();
        task.fan_out_grid_state();
        assert!(
            task.subs[0].stream.pending_scrolls.is_empty(),
            "a directive the replay already carries must not be admitted",
        );
        task.ship_all();

        let events = drain_events(&mut rx);
        assert!(
            !events
                .iter()
                .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Scrolled { .. }))),
            "no directive may follow a replay of the shifted rows",
        );
        mirror_apply(&mut shadow, &events);
        assert_eq!(
            mirror_rows(&shadow),
            grid_rows(&task),
            "the mirror must hold exactly the daemon's rows",
        );
    }

    fn scroll_the_grid(task: &SessionTask) {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser.advance(&mut core.grid, b"\x1b[S");
    }

    fn write_to_grid(task: &SessionTask, bytes: &[u8]) {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser.advance(&mut core.grid, bytes);
    }

    fn new_mirror(task: &SessionTask) -> felis_client_core::ShadowScreen {
        let core = task.session.lock_core();
        felis_client_core::ShadowScreen::new(core.grid.rows(), core.grid.cols())
    }

    /// Two compositions under separate guards within one row-cache
    /// cycle: a scroll between them must not hand the second mirror rows
    /// from the grid the first one read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_mirrors_composed_across_a_scroll_both_hold_the_grids_rows() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_a, mut rx_a) = attach_sub(&mut task, false);
        let (_b, mut rx_b) = attach_sub(&mut task, false);
        let mut shadow_a = new_mirror(&task);
        let mut shadow_b = new_mirror(&task);

        write_to_grid(&task, b"\x1b[H\x1b[2Jone\r\ntwo\r\nthree");
        task.drain_and_fan().expect("effects drain");
        task.ship_all();
        mirror_apply(&mut shadow_a, &drain_events(&mut rx_a));
        mirror_apply(&mut shadow_b, &drain_events(&mut rx_b));

        write_to_grid(&task, b"\r\nfour");
        task.drain_effects().expect("effects drain");
        task.fan_out_grid_state();
        // One ship cycle, with the parse thread scrolling in the middle.
        task.row_cache.begin_cycle(task.subs.len());
        task.ship_sub_at(1);
        let mid = drain_events(&mut rx_b);
        assert!(
            shipped_rows(&mid) > 0,
            "the first mirror must fill the cache this cycle",
        );
        mirror_apply(&mut shadow_b, &mid);
        scroll_the_grid(&task);
        task.ship_sub_at(0);
        task.drain_effects().expect("effects drain");
        task.fan_out_grid_state();
        task.ship_all();

        mirror_apply(&mut shadow_a, &drain_events(&mut rx_a));
        mirror_apply(&mut shadow_b, &drain_events(&mut rx_b));
        let grid = grid_rows(&task);
        assert_eq!(mirror_rows(&shadow_a), grid, "the first mirror");
        assert_eq!(mirror_rows(&shadow_b), grid, "the second mirror");
    }

    const RIS: &[u8] = b"\x1bc";
    const SWITCH_PLACE_RIS: &[&[u8]] = &[
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b_Ga=T,f=24,s=1,v=1,c=2,r=1,q=2;AAAA\x1b\\",
        RIS,
    ];

    proptest! {
        /// The primary screen's stash exists exactly while the alternate
        /// screen is up, whatever mix of switches, placements and RIS the
        /// effects carry; RIS also leaves no placement behind.
        #[test]
        fn the_saved_primary_placements_exist_iff_on_the_alternate_screen(
            ops in prop::collection::vec(prop::sample::select(SWITCH_PLACE_RIS), 1..16),
            per_drain in 1usize..4,
        ) {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            let _guard = rt.enter();
            let mut task = SessionTask::for_tests(session_with("read _x"));
            for burst in ops.chunks(per_drain) {
                write_to_grid(&task, &burst.concat());
                task.drain_effects().expect("effects drain");
                let on_alt = task.session.lock_core().grid.on_alternate_screen();
                prop_assert_eq!(task.session.saved_primary_placements.is_some(), on_alt, "{:?}", burst);
                if burst.last() == Some(&RIS) {
                    prop_assert!(task.session.placements.is_empty(), "RIS leaves a placement");
                }
            }
        }
    }

    /// The task's grid size, at `cell_px` per cell.
    fn sized_cells(task: &SessionTask, cell_px: (u16, u16)) -> PtySize {
        let (rows, cols) = {
            let core = task.session.lock_core();
            (core.grid.rows(), core.grid.cols())
        };
        PtySize {
            rows,
            cols,
            pixel_width: cols * cell_px.0,
            pixel_height: rows * cell_px.1,
        }
    }

    /// A 30×40 px RGB image, placed with `extra` keys.
    fn place_30x40(task: &SessionTask, id: u32, extra: &str) {
        let payload = "AAAA".repeat(30 * 40);
        let apc = format!("\x1b_Ga=T,f=24,s=30,v=40,i={id},q=2{extra};{payload}\x1b\\");
        write_to_grid(task, apc.as_bytes());
    }

    fn placement_extent(placements: &felis_grid::images::Placements, id: u32) -> (u16, u16) {
        let p = placements
            .for_image(felis_protocol::ImageId(id))
            .next()
            .expect("placement recorded");
        (p.cols, p.rows)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cell_size_change_re_resolves_a_natural_extent_and_ships_it_while_idle() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_sub, mut rx) = attach_sub(&mut task, false);
        task.apply_size(sized_cells(&task, (10, 20)));
        place_30x40(&task, 1, "");
        place_30x40(&task, 2, ",c=5,r=5");
        task.drain_effects().expect("effects drain");
        assert_eq!(placement_extent(&task.session.placements, 1), (3, 2));
        drain_events(&mut rx);

        task.apply_size(sized_cells(&task, (8, 16)));
        assert_eq!(placement_extent(&task.session.placements, 1), (4, 3));
        assert_eq!(
            placement_extent(&task.session.placements, 2),
            (5, 5),
            "an explicit c=/r= is not re-resolved",
        );
        assert!(
            drain_events(&mut rx).iter().any(|ev| matches!(
                ev,
                OutEvent::Image(ImageMsg::Placement {
                    image_id: felis_protocol::ImageId(1),
                    cols: 4,
                    rows: 3,
                    ..
                })
            )),
            "the resize itself ships the new extent, with no PTY output to carry it",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resize_without_pixel_dimensions_keeps_a_natural_extent() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        task.apply_size(sized_cells(&task, (10, 20)));
        place_30x40(&task, 1, "");
        task.drain_effects().expect("effects drain");

        task.apply_size(sized_cells(&task, (0, 0)));
        assert_eq!(
            placement_extent(&task.session.placements, 1),
            (3, 2),
            "the 1 px rule would grow the image over text printed past it",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_re_put_with_an_explicit_size_is_not_re_resolved() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        task.apply_size(sized_cells(&task, (10, 20)));
        place_30x40(&task, 1, "");
        write_to_grid(&task, b"\x1b_Ga=p,i=1,c=5,r=5,q=2\x1b\\");
        task.drain_effects().expect("effects drain");
        assert_eq!(placement_extent(&task.session.placements, 1), (5, 5));

        task.apply_size(sized_cells(&task, (8, 16)));
        assert_eq!(placement_extent(&task.session.placements, 1), (5, 5));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cell_width_reported_late_re_resolves_only_the_width() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_sub, mut rx) = attach_sub(&mut task, false);
        task.apply_size(sized_cells(&task, (0, 20)));
        place_30x40(&task, 1, ",r=4");
        task.drain_effects().expect("effects drain");
        assert_eq!(placement_extent(&task.session.placements, 1), (30, 4));
        drain_events(&mut rx);

        task.apply_size(sized_cells(&task, (8, 20)));
        assert_eq!(placement_extent(&task.session.placements, 1), (4, 4));
        assert!(
            drain_events(&mut rx).iter().any(|ev| matches!(
                ev,
                OutEvent::Image(ImageMsg::Placement {
                    image_id: felis_protocol::ImageId(1),
                    cols: 4,
                    rows: 4,
                    ..
                })
            )),
            "the new width is shipped",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_cell_size_shrinks_an_extent_placed_without_one() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        place_30x40(&task, 1, "");
        task.drain_effects().expect("effects drain");
        assert_eq!(placement_extent(&task.session.placements, 1), (30, 40));

        task.apply_size(sized_cells(&task, (10, 20)));
        assert_eq!(placement_extent(&task.session.placements, 1), (3, 2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_saved_primary_screen_follows_a_cell_size_change_on_the_alternate_one() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_sub, mut rx) = attach_sub(&mut task, false);
        task.apply_size(sized_cells(&task, (10, 20)));
        place_30x40(&task, 1, "");
        write_to_grid(&task, b"\x1b[?1049h");
        task.drain_effects().expect("effects drain");

        task.apply_size(sized_cells(&task, (8, 16)));
        let saved = task
            .session
            .saved_primary_placements
            .as_ref()
            .expect("primary placements stashed");
        assert_eq!(placement_extent(saved, 1), (4, 3));

        drain_events(&mut rx);
        write_to_grid(&task, b"\x1b[?1049l");
        task.drain_and_fan().expect("effects drain");
        task.ship_all();
        assert!(
            drain_events(&mut rx).iter().any(|ev| matches!(
                ev,
                OutEvent::Image(ImageMsg::Placement {
                    image_id: felis_protocol::ImageId(1),
                    cols: 4,
                    rows: 3,
                    ..
                })
            )),
            "leaving the alternate screen re-states the rescaled extent",
        );
    }

    /// A scroll between the fan-out and the composition leaves the
    /// partial replay carrying post-scroll cells; the directive drained
    /// afterwards would shift them a second time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scroll_racing_a_partial_replay_is_not_applied_twice() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_sub, mut rx) = attach_sub(&mut task, false);
        let mut shadow = new_mirror(&task);

        write_to_grid(&task, b"\x1b[H\x1b[2Jone\r\ntwo\r\nthree");
        task.drain_and_fan().expect("effects drain");
        task.ship_all();
        mirror_apply(&mut shadow, &drain_events(&mut rx));

        // One dirty row inside the region, fanned out before the scroll.
        write_to_grid(&task, b"\x1b[2;1Htwo!");
        task.drain_effects().expect("effects drain");
        task.fan_out_grid_state();
        scroll_the_grid(&task);
        task.ship_all();
        task.drain_effects().expect("effects drain");
        task.fan_out_grid_state();
        task.ship_all();

        mirror_apply(&mut shadow, &drain_events(&mut rx));
        assert_eq!(
            mirror_rows(&shadow),
            grid_rows(&task),
            "the mirror must hold exactly the daemon's rows",
        );
    }

    /// A row edited in place (no scroll, no resize) after a drain cycle
    /// cached its encoding must reach a mirror whose pull the command
    /// arm answers, not the row as the earlier cycle encoded it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pull_after_an_in_place_edit_ships_the_edited_row_to_the_second_mirror() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_a, mut rx_a) = attach_sub(&mut task, true);
        let (b, mut rx_b) = attach_sub(&mut task, true);
        let mut shadow_b = new_mirror(&task);

        write_to_grid(&task, b"\x1b[H\x1b[2Jone");
        for sub in &mut task.subs {
            sub.pull_pending = true;
        }
        task.drain_and_fan().expect("effects drain");
        drain_events(&mut rx_a);
        mirror_apply(&mut shadow_b, &drain_events(&mut rx_b));
        assert_eq!(mirror_rows(&shadow_b)[0], "one");

        write_to_grid(&task, b"\x1b[Htwo");
        task.handle_input(b, InputMsg::NextGridFrame);

        mirror_apply(&mut shadow_b, &drain_events(&mut rx_b));
        assert_eq!(
            mirror_rows(&shadow_b),
            grid_rows(&task),
            "the second mirror must hold the edited row",
        );
    }

    /// An outbox that still takes the cycle's facets but overflows on its
    /// composed rows is evicted in the push after the guard drops, and
    /// the healthy peer still receives its whole cycle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_outbox_overflowing_on_its_rows_is_evicted_and_its_peer_still_gets_the_cycle() {
        const HEADROOM: usize = 8 * WIRE_LEN_OVERHEAD;
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (dying, mut rx_dying) = attach_sub(&mut task, true);
        let (healthy, mut rx_healthy) = attach_sub(&mut task, true);
        let dying_idx = task.sub_index(dying).expect("dying subscriber present");
        task.subs[dying_idx]
            .buffered
            .store(SUBSCRIBER_BUFFER_CAP - HEADROOM, Ordering::Relaxed);

        let mut screen = String::from("\x1b]0;renamed\x07\x1b[H\x1b[2J");
        for row in 0..20_u32 {
            for col in 0..60_u32 {
                screen.push(char::from(
                    b'!' + u8::try_from((row * 7 + col) % 90).unwrap(),
                ));
            }
            screen.push_str("\r\n");
        }
        write_to_grid(&task, screen.as_bytes());
        for sub in &mut task.subs {
            sub.pull_pending = true;
        }
        task.drain_and_fan().expect("effects drain");

        let dying_events = drain_events(&mut rx_dying);
        assert!(
            dying_events
                .iter()
                .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Title { .. }))),
            "the facet fits the dying outbox, so the failure lands on the rows: {dying_events:?}",
        );
        assert_eq!(
            shipped_rows(&dying_events),
            0,
            "the rows overflowed the cap"
        );
        assert!(
            task.sub_index(dying).is_none(),
            "the overflowing outbox is evicted"
        );
        assert_eq!(task.sub_index(healthy), Some(0));

        let events = drain_events(&mut rx_healthy);
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Title { .. }))),
            "the facet reaches the healthy peer: {events:?}",
        );
        assert!(shipped_rows(&events) > 0, "the rows reach the healthy peer");
        assert!(
            matches!(events.last(), Some(OutEvent::Grid(GridMsg::CycleEnd))),
            "the healthy peer's cycle is closed: {events:?}",
        );
        let meta = task.meta.lock().unwrap();
        assert_eq!(meta.subscribers, 1);
        assert_eq!(meta.title.as_deref(), Some("renamed"));
    }

    /// A client that refuses a grid frame closes only its own
    /// attachment (`docs/reference/ipc.md` "Corruption"): the session
    /// and every other subscriber must outlive it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_attachment_that_closes_leaves_the_session_and_its_peers_running() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_a, rx_a) = attach_sub(&mut task, false);
        let (sub_b, mut rx_b) = attach_sub(&mut task, false);

        task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
        task.handle_input(
            sub_b,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: 30,
                    cols: 120,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        );
        drain_events(&mut rx_b);
        // The refusing client drops its connection.
        drop(rx_a);

        let end = task.handle_input(sub_b, InputMsg::KeyBytes(b"b".to_vec()));
        assert!(
            end.is_none(),
            "one lost attachment must not end the session"
        );
        assert!(
            !task.subs.iter().any(|sub| sub.id == sub_a),
            "the closed attachment must be dropped from the roster",
        );
        assert_eq!(
            {
                let core = task.session.lock_core();
                (core.grid.rows(), core.grid.cols())
            },
            (30, 120),
            "the surviving subscriber still drives the authoritative grid",
        );
        assert!(
            drain_events(&mut rx_b).iter().any(|ev| matches!(
                ev,
                OutEvent::Grid(GridMsg::Size {
                    dims: GridDims {
                        rows: 30,
                        cols: 120,
                        ..
                    }
                })
            )),
            "the surviving mirror must keep receiving frames",
        );
    }

    /// Pointer motion over a mirror does not transfer size ownership
    /// while the running program has no mouse protocol.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mouse_motion_without_mouse_protocol_does_not_transfer_ownership() {
        use felis_protocol::messages::{InputMods, MouseAction, MouseEvent};

        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_a, _rx_a) = attach_sub(&mut task, false);
        let (sub_b, _rx_b) = attach_sub(&mut task, false);

        task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
        assert_eq!(task.active_sub, Some(sub_a));

        task.handle_input(
            sub_b,
            InputMsg::Mouse(MouseEvent {
                button: None,
                action: MouseAction::Motion,
                mods: InputMods::empty(),
                x: 3,
                y: 3,
                px: 3,
                py: 3,
            }),
        );
        assert_eq!(
            task.active_sub,
            Some(sub_a),
            "hover must not steal the PTY size while no mouse protocol is active",
        );
    }

    /// A key release from a mirror does not transfer size ownership while
    /// the running program has not asked for release events.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreported_key_release_does_not_transfer_ownership() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_a, _rx_a) = attach_sub(&mut task, false);
        let (sub_b, _rx_b) = attach_sub(&mut task, false);

        task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
        assert_eq!(task.active_sub, Some(sub_a));

        task.handle_input(
            sub_b,
            InputMsg::Key(KeyEvent {
                key: felis_protocol::messages::Key::Character("b".into()),
                text: None,
                mods: felis_protocol::messages::KeyMods::empty(),
                kind: KeyEventKind::Release,
                location: felis_protocol::messages::KeyLocation::Standard,
            }),
        );
        assert_eq!(
            task.active_sub,
            Some(sub_a),
            "a release that encodes to no bytes must not steal the PTY size",
        );
    }

    /// `EvictAll`: every subscriber receives `Evicted` and its outbox
    /// closes; the task keeps running parked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn evict_all_ships_evicted_to_every_subscriber() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_a, mut rx_a) = attach_sub(&mut task, false);
        let (_b, mut rx_b) = attach_sub(&mut task, false);

        let (reply_tx, mut reply_rx) = oneshot::channel();
        let end = task.handle_cmd(SessionCmd::EvictAll { reply: reply_tx });
        assert!(end.is_none(), "EvictAll must not end the task");
        assert_eq!(reply_rx.try_recv().expect("reply"), 2);
        for rx in [&mut rx_a, &mut rx_b] {
            let evicted = drain_events(rx)
                .iter()
                .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::Evicted { .. })))
                .count();
            assert_eq!(evicted, 1, "each subscriber hears exactly one Evicted");
            assert!(
                matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)),
                "outbox must close after eviction",
            );
        }
        assert!(task.subs.is_empty());
        assert_eq!(task.meta.lock().unwrap().subscribers, 0);
    }

    /// Shell exit ships `SessionExited`, carrying the session's own id,
    /// to window subscribers only.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn notify_shell_exit_ships_session_exited_only_to_window_subscribers() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_window, mut rx_window) = attach_sub(&mut task, false);
        let (_ops, mut rx_ops) = attach_ops_sub(&mut task);

        task.notify_shell_exit();

        let pushed: Vec<_> = drain_events(&mut rx_window)
            .into_iter()
            .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::SessionExited { .. })))
            .collect();
        assert_eq!(pushed.len(), 1, "the window hears one SessionExited");
        assert!(
            matches!(
                pushed[0],
                OutEvent::Push(PushMsg::SessionExited { id }) if id == task.id.0
            ),
            "SessionExited carries this session's id",
        );
        assert!(
            drain_events(&mut rx_ops).is_empty(),
            "a scripted Ops attach has no window to keep open, so it gets no SessionExited",
        );
    }

    /// Both authoritative-dims paths (applied resize, overruled-mirror
    /// correction) announce to every subscriber.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resize_announces_authoritative_dims_to_every_subscriber() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (owner, mut rx_owner) = attach_sub(&mut task, false);
        let (mirror, mut rx_mirror) = attach_sub(&mut task, false);

        task.handle_input(owner, InputMsg::KeyBytes(b"a".to_vec()));
        assert_eq!(task.active_sub, Some(owner));
        drain_events(&mut rx_owner);
        drain_events(&mut rx_mirror);

        task.handle_input(
            owner,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: 30,
                    cols: 120,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        );
        let hears_new_dims = |evs: &[OutEvent]| {
            evs.iter().any(|ev| {
                matches!(
                    ev,
                    OutEvent::Grid(GridMsg::Size {
                        dims: GridDims {
                            rows: 30,
                            cols: 120,
                            ..
                        }
                    })
                )
            })
        };
        assert!(
            hears_new_dims(&drain_events(&mut rx_owner)),
            "the owner must hear the new authoritative dims",
        );
        assert!(
            hears_new_dims(&drain_events(&mut rx_mirror)),
            "a mirror must hear the new authoritative dims too",
        );

        task.handle_input(
            mirror,
            InputMsg::Resize {
                dims: RequestedDims {
                    rows: 50,
                    cols: 200,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
        );
        assert!(
            hears_new_dims(&drain_events(&mut rx_mirror)),
            "an overruled mirror gets the authoritative dims as a correction",
        );
    }

    /// A subscriber whose backlog crosses `SUBSCRIBER_BUFFER_CAP` is
    /// evicted on its next ship while a healthy peer keeps receiving.
    /// The gauge is pre-loaded to just under the cap rather than
    /// materializing the backlog.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_subscriber_is_evicted_while_healthy_peer_keeps_streaming() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_slow, mut rx_slow) = attach_sub(&mut task, false);
        let (sub_healthy, mut rx_healthy) = attach_sub(&mut task, false);

        let mut parser = felis_vt::Parser::new();
        parser.advance(&mut task.session.lock_core().grid, b"hello world\r\n");

        let slow_idx = task.sub_index(sub_slow).expect("slow subscriber present");
        task.subs[slow_idx].buffered.store(
            SUBSCRIBER_BUFFER_CAP - WIRE_LEN_OVERHEAD + 1,
            Ordering::Relaxed,
        );

        task.fan_out_grid_state();
        task.ship_all();

        assert!(
            task.sub_index(sub_slow).is_none(),
            "over-cap subscriber must be evicted",
        );
        assert!(
            matches!(
                rx_slow.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected),
            ),
            "evicted subscriber's outbox must close",
        );

        assert_eq!(task.subs.len(), 1, "only the healthy peer remains");
        assert_eq!(
            task.sub_index(sub_healthy),
            Some(0),
            "the surviving subscriber is the healthy peer",
        );
        let healthy_rows = drain_events(&mut rx_healthy)
            .iter()
            .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. })))
            .count();
        assert!(
            healthy_rows >= 1,
            "the healthy peer keeps receiving frames despite its peer's eviction",
        );
        assert_eq!(task.meta.lock().unwrap().subscribers, 1);
    }

    fn switch_push(task: &mut SessionTask, scope: SwitchScope) -> PushOutcome {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let end = task.handle_cmd(SessionCmd::PushToTarget {
            msg: PushMsg::Reattach { id: 0xBEEF },
            scope,
            reply: reply_tx,
        });
        assert!(end.is_none(), "a switch push must not end the task");
        reply_rx.try_recv().expect("reply")
    }

    fn queued(outcome: PushOutcome) -> u32 {
        match outcome {
            PushOutcome::Accepted(queued) => queued,
            PushOutcome::Denied(denied) => panic!("unexpected denial: {denied:?}"),
        }
    }

    fn reattach_count(rx: &mut mpsc::UnboundedReceiver<OutEvent>) -> usize {
        drain_events(rx)
            .iter()
            .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::Reattach { .. })))
            .count()
    }

    fn roster(task: &SessionTask) -> Vec<Attachment> {
        task.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .attachments
            .clone()
    }

    /// The default scope resolves to the window whose input last
    /// reached the PTY, and to that window alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_default_scope_targets_the_last_window_input_owner() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_mirror, mut rx_mirror) = attach_sub(&mut task, false);
        let (typed_in, mut rx_typed_in) = attach_sub(&mut task, false);

        task.handle_input(typed_in, InputMsg::KeyBytes(b"b".to_vec()));
        assert_eq!(task.input_owner, Some(typed_in));
        drain_events(&mut rx_mirror);
        drain_events(&mut rx_typed_in);

        assert_eq!(queued(switch_push(&mut task, SwitchScope::Default)), 1,);
        assert_eq!(
            reattach_count(&mut rx_typed_in),
            1,
            "the owner hears the push",
        );
        assert_eq!(
            reattach_count(&mut rx_mirror),
            0,
            "a mirror the user did not type in stays where it is",
        );
    }

    /// With the marker clear, a session with exactly one window
    /// resolves to it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_default_scope_falls_back_to_a_sole_window() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_sole, mut rx) = attach_sub(&mut task, false);
        drain_events(&mut rx);
        assert_eq!(task.input_owner, None, "nothing has typed yet");

        assert_eq!(queued(switch_push(&mut task, SwitchScope::Default)), 1,);
        assert_eq!(reattach_count(&mut rx), 1);
    }

    /// With the marker clear and two windows attached, the default
    /// scope is refused by name.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_default_scope_refuses_two_windows_with_no_input_owner() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_a, mut rx_a) = attach_sub(&mut task, false);
        let (_b, mut rx_b) = attach_sub(&mut task, false);

        assert_eq!(
            switch_push(&mut task, SwitchScope::Default),
            PushOutcome::Denied(SwitchDenied::NoInputOwner),
        );
        assert_eq!(reattach_count(&mut rx_a) + reattach_count(&mut rx_b), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ops_input_takes_size_ownership_but_not_the_switch_target() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (window, mut rx_window) = attach_sub(&mut task, false);
        let (ops, _rx_ops) = attach_ops_sub(&mut task);

        task.handle_input(window, InputMsg::KeyBytes(b"w".to_vec()));
        task.handle_input(ops, InputMsg::KeyBytes(b"o".to_vec()));

        assert_eq!(
            task.active_sub,
            Some(ops),
            "size ownership follows any input that reaches the PTY",
        );
        assert_eq!(
            task.input_owner,
            Some(window),
            "the switch target stays with the window that typed",
        );
        drain_events(&mut rx_window);
        assert_eq!(queued(switch_push(&mut task, SwitchScope::Default)), 1,);
        assert_eq!(reattach_count(&mut rx_window), 1);
    }

    /// The owning window detaching clears the marker, so the sole
    /// survivor becomes the target under the fallback rule.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_owner_detaching_clears_the_marker_and_the_survivor_takes_over() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (owner, _rx_owner) = attach_sub(&mut task, false);
        let (_survivor, mut rx_survivor) = attach_sub(&mut task, false);

        task.handle_input(owner, InputMsg::KeyBytes(b"o".to_vec()));
        assert_eq!(task.input_owner, Some(owner));

        task.remove_sub(owner);
        assert_eq!(task.input_owner, None, "the marker clears with its owner");

        drain_events(&mut rx_survivor);
        assert_eq!(queued(switch_push(&mut task, SwitchScope::Default)), 1,);
        assert_eq!(reattach_count(&mut rx_survivor), 1);
    }

    /// An explicit attachment overrules the marker; a stale id is
    /// refused by name, never redirected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_explicit_attachment_targets_that_window_and_a_stale_id_is_refused() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (owner, mut rx_owner) = attach_sub(&mut task, false);
        let (other, mut rx_other) = attach_sub(&mut task, false);
        task.handle_input(owner, InputMsg::KeyBytes(b"o".to_vec()));
        drain_events(&mut rx_owner);
        drain_events(&mut rx_other);

        assert_eq!(
            queued(switch_push(&mut task, SwitchScope::Attachment(other.0))),
            1,
        );
        assert_eq!(reattach_count(&mut rx_other), 1, "the named window moves");
        assert_eq!(reattach_count(&mut rx_owner), 0, "the marker is overruled");

        let gone = other.0;
        task.remove_sub(other);
        assert_eq!(
            switch_push(&mut task, SwitchScope::Attachment(gone)),
            PushOutcome::Denied(SwitchDenied::NoSuchAttachment { attachment: gone }),
        );
        assert_eq!(
            reattach_count(&mut rx_owner),
            0,
            "a stale id must not fall back to a surviving window",
        );
    }

    /// The roster carries one stamped, flagged row per window
    /// attachment; a marker transfer flips `input_owner` without
    /// changing the subscriber count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_roster_reports_attachments_and_flips_the_owner_on_transfer() {
        let before = SystemTime::now();
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_a, _rx_a) = attach_sub(&mut task, false);
        let (sub_b, _rx_b) = attach_sub(&mut task, false);
        let (_ops, _rx_ops) = attach_ops_sub(&mut task);

        let rows = roster(&task);
        assert_eq!(
            rows.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![sub_a.0, sub_b.0],
            "window attachments only, in attach order",
        );
        assert!(
            rows.iter().all(|a| a.attached_at >= before),
            "every attachment carries the instant its attach landed",
        );
        assert!(
            rows.iter().all(|a| !a.input_owner),
            "nothing has typed, so no attachment claims the marker",
        );

        task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
        let owners: Vec<u64> = roster(&task)
            .iter()
            .filter(|a| a.input_owner)
            .map(|a| a.id)
            .collect();
        assert_eq!(owners, vec![sub_a.0]);

        let count_before = task.subs.len();
        task.handle_input(sub_b, InputMsg::KeyBytes(b"b".to_vec()));
        assert_eq!(
            task.subs.len(),
            count_before,
            "a transfer changes no subscriber count",
        );
        let owners: Vec<u64> = roster(&task)
            .iter()
            .filter(|a| a.input_owner)
            .map(|a| a.id)
            .collect();
        assert_eq!(owners, vec![sub_b.0], "the marker moved with the input");
    }

    /// Attachment ids are daemon-global and never reused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attachment_ids_are_never_reused_across_sessions() {
        let ids = AttachmentIds::detached();
        let mut first = SessionTask::for_tests(session_with("read _x"));
        first.attachment_ids = ids.clone();
        let mut second = SessionTask::for_tests(session_with("read _x"));
        second.attachment_ids = ids;

        let (a, _rx_a) = attach_sub(&mut first, false);
        let (b, _rx_b) = attach_sub(&mut second, false);
        first.remove_sub(a);
        let (c, _rx_c) = attach_sub(&mut second, false);

        assert_ne!(a.0, b.0, "two sessions' windows carry distinct ids");
        assert_ne!(a.0, c.0, "a departed window's id is never handed out again");
        assert_ne!(b.0, c.0);
    }

    /// A `Reattach` push reaches the window subscriber and leaves it
    /// subscribed; an `Ops` attach is skipped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reattach_push_reaches_only_window_subscribers() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_window, mut rx_capable) = attach_sub(&mut task, false);
        let (_ops, mut rx_legacy) = attach_ops_sub(&mut task);

        let (reply_tx, mut reply_rx) = oneshot::channel();
        let end = task.handle_cmd(SessionCmd::PushToTarget {
            msg: PushMsg::Reattach { id: 0xBEEF },
            scope: SwitchScope::Default,
            reply: reply_tx,
        });
        assert!(end.is_none(), "the switch push must not end the task");
        assert_eq!(
            queued(reply_rx.try_recv().expect("reply")),
            1,
            "only the window subscriber counts as queued",
        );
        let reattaches = drain_events(&mut rx_capable)
            .iter()
            .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::Reattach { id: 0xBEEF, .. })))
            .count();
        assert_eq!(reattaches, 1, "the window hears one Reattach");
        assert!(
            drain_events(&mut rx_legacy)
                .iter()
                .all(|ev| !matches!(ev, OutEvent::Push(PushMsg::Reattach { .. }))),
            "a scripted Ops attach must not be moved",
        );
        assert_eq!(
            task.subs.len(),
            2,
            "both subscribers stay attached — the push does not evict",
        );
    }

    /// A `RetargetHost` push reaches the window subscriber and leaves
    /// it subscribed; an `Ops` attach is skipped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retarget_push_reaches_only_window_subscribers() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_window, mut rx_capable) = attach_sub(&mut task, false);
        let (_ops, mut rx_switch_only) = attach_ops_sub(&mut task);

        let target = RetargetTarget {
            carrier: RetargetCarrier::Ssh {
                destination: "user@devbox".into(),
                ssh_args: vec!["-p".into(), "2222".into()],
            },
            landing: RetargetLanding::Create(SpawnArgs::default()),
        };
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let end = task.handle_cmd(SessionCmd::PushToTarget {
            msg: PushMsg::RetargetHost {
                target: target.clone(),
            },
            scope: SwitchScope::Default,
            reply: reply_tx,
        });
        assert!(end.is_none(), "the retarget push must not end the task");
        assert_eq!(
            queued(reply_rx.try_recv().expect("reply")),
            1,
            "only the window subscriber counts as queued",
        );
        let retargets: Vec<_> = drain_events(&mut rx_capable)
            .into_iter()
            .filter(|ev| matches!(ev, OutEvent::Push(PushMsg::RetargetHost { .. })))
            .collect();
        assert_eq!(retargets.len(), 1, "the window hears one RetargetHost");
        assert!(
            matches!(
                &retargets[0],
                OutEvent::Push(PushMsg::RetargetHost { target: t, .. }) if *t == target
            ),
            "RetargetHost relays the target verbatim",
        );
        assert!(
            drain_events(&mut rx_switch_only)
                .iter()
                .all(|ev| !matches!(ev, OutEvent::Push(PushMsg::RetargetHost { .. }))),
            "a scripted Ops attach must not be re-dialed",
        );
        assert_eq!(task.subs.len(), 2, "the push does not evict");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn notification_attached_bit_ignores_ops_subscribers() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let mut hub_rx = task.hub.subscribe();
        let (_ops, _rx_ops) = attach_ops_sub(&mut task);

        {
            let mut core = task.session.lock_core();
            let core = &mut *core;
            core.parser
                .advance(&mut core.grid, b"\x1b]99;u=1;ops only\x1b\\");
        }
        task.after_drive();
        match hub_rx.try_recv().expect("event published") {
            NotifyToClientMsg::Event { attached, .. } => {
                assert!(!attached, "an Ops reader is not an attached window");
            }
            other => panic!("expected NotifyToClientMsg::Event, got {other:?}"),
        }

        let (_win, _rx_win) = attach_sub(&mut task, false);
        {
            let mut core = task.session.lock_core();
            let core = &mut *core;
            core.parser
                .advance(&mut core.grid, b"\x1b]99;u=1;window now\x1b\\");
        }
        task.after_drive();
        loop {
            match hub_rx.try_recv().expect("event published") {
                NotifyToClientMsg::Event {
                    attached,
                    notification,
                    ..
                } if notification.title.as_deref() == Some("window now") => {
                    assert!(attached, "a window subscriber sets the bit");
                    break;
                }
                _ => {}
            }
        }
    }

    /// Per-client clipboard scope (security-model.md): an OSC 52 write
    /// reaches only the active subscriber, never a mirror.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn osc52_write_reaches_only_the_active_subscriber() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (sub_a, mut rx_a) = attach_sub(&mut task, false);
        let (_sub_b, mut rx_b) = attach_sub(&mut task, false);
        task.active_sub = Some(sub_a);

        {
            let mut core = task.session.lock_core();
            let core = &mut *core;
            core.parser.advance(&mut core.grid, b"\x1b]52;c;Zm9v\x1b\\");
        }
        task.broadcast_facets();

        let writes_a = drain_events(&mut rx_a)
            .iter()
            .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. })))
            .count();
        assert_eq!(writes_a, 1, "the active subscriber hears the write");
        assert!(
            drain_events(&mut rx_b)
                .iter()
                .all(|ev| !matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. }))),
            "a mirror must not hear another client's clipboard write",
        );
    }

    /// With no active subscriber and several mirrors an OSC 52 write is
    /// dropped; a sole window subscriber still receives it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unattributable_osc52_write_is_dropped_not_broadcast() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let (_sub_a, mut rx_a) = attach_sub(&mut task, false);
        let (_sub_b, mut rx_b) = attach_sub(&mut task, false);

        {
            let mut core = task.session.lock_core();
            let core = &mut *core;
            core.parser.advance(&mut core.grid, b"\x1b]52;c;Zm9v\x1b\\");
        }
        task.broadcast_facets();

        for rx in [&mut rx_a, &mut rx_b] {
            assert!(
                drain_events(rx)
                    .iter()
                    .all(|ev| !matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. }))),
                "an unattributable write must reach no mirror",
            );
        }

        let mut solo = SessionTask::for_tests(session_with("read _x"));
        let (_only, mut rx_only) = attach_sub(&mut solo, false);
        {
            let mut core = solo.session.lock_core();
            let core = &mut *core;
            core.parser.advance(&mut core.grid, b"\x1b]52;c;Zm9v\x1b\\");
        }
        solo.broadcast_facets();
        let writes = drain_events(&mut rx_only)
            .iter()
            .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::ClipboardSet { .. })))
            .count();
        assert_eq!(writes, 1, "a sole window subscriber is unambiguous");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervise_removes_pool_entry_when_the_task_panics() {
        let id = SessionId::new();
        let (cmd, _cmd_rx) = mpsc::channel(1);
        let handle = SessionHandle {
            cmd,
            input_budget: crate::pool::new_input_budget(),
            resizer: None,
            meta: Arc::new(StdMutex::new(SessionMeta {
                rows: crate::DEFAULT_ROWS,
                cols: crate::DEFAULT_COLS,
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
                sequence: std::num::NonZeroU64::MIN,
            })),
        };
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        pool.lock()
            .await
            .register(id, handle, None, Listing::Public);

        let task = tokio::spawn(async { panic!("boom") });
        supervise(
            task,
            Arc::downgrade(&pool),
            id,
            exited_child(),
            watch::channel(false).0,
        )
        .await;

        assert!(
            pool.lock().await.handle_cloned(id).is_none(),
            "the panicked task's entry must be gone from the pool"
        );
    }

    fn exited_child() -> Arc<ChildHandle> {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        let spawned = crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh");
        Arc::new(spawned.child)
    }

    async fn pid_from(path: &std::path::Path) -> i32 {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the shell never reported its pid to {}", path.display());
    }

    /// Reaped counts as gone: the daemon stays the parent, and a zombie
    /// would still answer a signal.
    async fn waits_for_exit(pid: i32) -> bool {
        let Some(pid) = rustix::process::Pid::from_raw(pid) else {
            return true;
        };
        for _ in 0..200 {
            if matches!(
                rustix::process::test_kill_process(pid),
                Err(rustix::io::Errno::SRCH)
            ) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn destroying_a_session_ends_its_shell() {
        let dir = tempfile::TempDir::new().unwrap();
        let pidfile = dir.path().join("pid");
        let mut cmd = Command::new("/bin/sh");
        // External `sleep`, not a builtin blocking on the PTY: it does
        // not end when the master closes, so only the signal can end it.
        cmd.args(["-c", &format!("echo $$ > {}; sleep 60", pidfile.display())]);
        cmd.env_clear();
        cmd.env("PATH", crate::fixture_path());
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let SessionLifecycle { id, .. } = spawn_session(
            &pool,
            crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"),
            IdlePolicy::default(),
            SessionId::new(),
            GridDims {
                rows: crate::DEFAULT_ROWS,
                cols: crate::DEFAULT_COLS,
                pixel_w: 0,
                pixel_h: 0,
            },
            Vec::new(),
            None,
            Listing::Public,
        )
        .await;
        let pid = pid_from(&pidfile).await;

        let handle = pool.lock().await.handle_cloned(id).expect("registered");
        handle.cmd.send(SessionCmd::Shutdown).await.expect("send");

        assert!(
            waits_for_exit(pid).await,
            "the destroyed session's shell is still running",
        );
    }

    /// Panicking tasks terminate their child process even if it ignores `SIGHUP`.
    ///
    /// `trap '' HUP` survives `exec` to verify the hangup, grace, and `SIGKILL` sequence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervise_ends_the_child_when_the_task_panics() {
        let dir = tempfile::TempDir::new().unwrap();
        let pidfile = dir.path().join("pid");
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            &format!(
                "trap '' HUP; echo $$ > {}; exec sleep 60",
                pidfile.display()
            ),
        ]);
        cmd.env_clear();
        cmd.env("PATH", crate::fixture_path());
        let session = Session::from_spawned(crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"));
        let pid = pid_from(&pidfile).await;
        let child = Arc::clone(&session.child);

        // The real panic path drops the session before the watchdog
        // sees the `JoinError`; same order here, so the drop-side
        // hangup is not what the test proves.
        drop(session);

        let pool = Arc::new(Mutex::new(SessionPool::new()));
        let task = tokio::spawn(async { panic!("boom") });
        supervise(
            task,
            Arc::downgrade(&pool),
            SessionId::new(),
            child,
            watch::channel(false).0,
        )
        .await;

        assert!(
            waits_for_exit(pid).await,
            "the panicked task's shell is still running",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervise_leaves_the_pool_alone_on_clean_exit() {
        let id = SessionId::new();
        let (cmd, _cmd_rx) = mpsc::channel(1);
        let handle = SessionHandle {
            cmd,
            input_budget: crate::pool::new_input_budget(),
            resizer: None,
            meta: Arc::new(StdMutex::new(SessionMeta {
                rows: crate::DEFAULT_ROWS,
                cols: crate::DEFAULT_COLS,
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
                sequence: std::num::NonZeroU64::MIN,
            })),
        };
        let pool = Arc::new(Mutex::new(SessionPool::new()));
        pool.lock()
            .await
            .register(id, handle, None, Listing::Public);

        let task = tokio::spawn(async {});
        supervise(
            task,
            Arc::downgrade(&pool),
            id,
            exited_child(),
            watch::channel(false).0,
        )
        .await;

        assert!(
            pool.lock().await.handle_cloned(id).is_some(),
            "a clean exit must not touch the pool entry"
        );
    }

    /// REQ-307 / REQ-1012: animation frames advance daemon-side
    /// regardless of client attachment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn animation_advances_with_no_subscriber_attached() {
        use felis_grid::images::{Frame, ImageEntry};
        use felis_protocol::ImageId;
        use felis_protocol::messages::ImageFormat;

        let mut task = SessionTask::for_tests(session_with("read _x"));
        let id = ImageId(7);
        {
            let images = &mut task.session.images;
            images
                .insert(
                    id,
                    ImageEntry::new(1, 1, ImageFormat::Rgba32, vec![0xFF; 4]),
                )
                .unwrap();
            for _ in 0..2 {
                images
                    .push_frame(
                        id,
                        Frame {
                            pixels: vec![0xFF; 4].into(),
                            gap_ms: 20,
                        },
                    )
                    .unwrap();
            }
            // `advance_animations` skips unplaced images.
            assert!(images.retain(id), "fixture image must be placed");
        }

        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
        let loop_task = tokio::spawn(run_session(task, cmd_rx));

        // Real time, not tokio's paused clock: `graphics::anim_now_ms`
        // is `Instant`-based.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(SessionCmd::Subscribe(SubscribeReq {
                mode: ConnectionMode::Window,
                pull_paced: false,
                live_only: false,
                tx,
                buffered: Arc::new(AtomicUsize::new(0)),
                reply: reply_tx,
            }))
            .await
            .map_err(|_| ())
            .expect("session task accepts the subscribe");
        reply_rx
            .await
            .expect("subscribe reply")
            .expect("the subscribe was admitted");

        let mut shown = None;
        loop {
            match rx.recv().await.expect("rehydrate event") {
                OutEvent::Image(ImageMsg::ShowFrame { id: img, number }) if img == id => {
                    shown = Some(number.get());
                }
                OutEvent::Grid(GridMsg::RehydrateEnd) => break,
                _ => {}
            }
        }
        assert!(
            matches!(shown, Some(number) if number > 1),
            "a reattach must land on the live frame; got {shown:?}",
        );

        cmd_tx.send(SessionCmd::Shutdown).await.ok();
        loop_task.await.expect("session loop ends cleanly");
    }

    /// The `subscriber_queue_bytes` subject sample is the deepest outbox
    /// (a max, not a sum) and moves with real pushes, while the sibling
    /// the daemon-wide total is built from is every outbox summed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stats_report_the_deepest_subscriber_backlog() {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        assert_eq!(
            task.stats().max_subscriber_backlog,
            0,
            "no subscriber, no backlog"
        );

        let (shallow, _rx_shallow) = attach_sub(&mut task, false);
        let (deep, _rx_deep) = attach_sub(&mut task, false);
        let after_attach = task.stats().max_subscriber_backlog;
        assert!(
            after_attach > 0,
            "an undrained attach burst is queued bytes: {after_attach}"
        );

        let deep_idx = task.sub_index(deep).expect("deep subscriber present");
        task.subs[deep_idx]
            .buffered
            .store(after_attach + 4096, Ordering::Relaxed);
        let shallow_idx = task.sub_index(shallow).expect("shallow subscriber present");
        let shallow_depth = task.subs[shallow_idx].buffered.load(Ordering::Relaxed);

        let mut parser = felis_vt::Parser::new();
        parser.advance(&mut task.session.lock_core().grid, b"hello world\r\n");
        task.fan_out_grid_state();
        task.ship_all();

        let sample = task.stats().max_subscriber_backlog;
        assert!(
            sample > after_attach + 4096,
            "the row follows the deepest outbox and the push grew it: {sample}"
        );
        assert!(
            task.subs[shallow_idx].buffered.load(Ordering::Relaxed) > shallow_depth,
            "the shallow peer also grew, so the sample is a max and not a sum"
        );

        let stats = task.stats();
        let shallow_now = task.subs[shallow_idx].buffered.load(Ordering::Relaxed);
        let deep_now = task.subs[deep_idx].buffered.load(Ordering::Relaxed);
        assert_eq!(
            stats.total_subscriber_backlog,
            shallow_now + deep_now,
            "the sibling field is every outbox summed, not the deepest one"
        );
        assert_eq!(stats.max_subscriber_backlog, deep_now);
    }
}

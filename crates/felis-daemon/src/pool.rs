//! Session pool: id to handle registry for per-session owner tasks.
//!
//! State lives inside each session's owner task (`serve::session_task`),
//! reached through the command channel in [`SessionHandle`].

use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap, HashSet},
    num::NonZeroU64,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering},
    },
    time::Instant,
};

use felis_grid::{
    Grid, TableGc,
    images::{ImageStore, Placements},
};
use felis_protocol::messages::{Attachment, MAX_SESSION_TAGS, MAX_TAG_BYTES, NotifyToClientMsg};
use felis_pty::{ChildHandle, PtyReader, PtyWriter, Resizer};
use felis_vt::{Parser, kitty_graphics::Reassembler};
use tokio::sync::{Notify, Semaphore, broadcast, mpsc};

use crate::{
    graphics::{ImageEvent, ShmDeferral},
    serve::session_task::SessionCmd,
};

/// Initial PTY size before the client reports its viewport.
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;
/// Per-session byte cap for the Kitty graphics image store (a 4K RGBA
/// frame is ≈33 MiB). Caps are per-session, never shared
/// (`security-model.md` "Kitty graphics"). Shared with the client's
/// mirror through `felis-protocol`, which enforces the same total
/// against the headers it receives.
pub const DEFAULT_IMAGE_BYTE_CAP: usize = felis_protocol::messages::MAX_SESSION_IMAGE_BYTES;

/// Most sessions one daemon admits; creates beyond this are refused.
///
/// Compiled in rather than configured; reported by `felis daemon status`
/// (`docs/reference/cli.md`).
pub const MAX_SESSIONS: usize = 256;

/// Most connections one daemon serves concurrently.
///
/// Bounds tasks and descriptors across sessions (`docs/reference/spec.md`).
/// Descriptor exhaustion is handled in the accept loop by backing off
/// on `EMFILE`.
pub const MAX_CONNECTIONS: usize = 1024;

/// Over-cap peers the daemon will answer at once. A refusal costs a
/// task, an fd, and a read buffer for as long as the handshake
/// deadlines allow, so it is admitted against its own small ceiling;
/// past it a connection is dropped unanswered.
pub const MAX_REFUSALS_IN_FLIGHT: usize = 16;

/// Random `u128` session id; not user-visible
/// (`architecture/session-lifecycle.md` "Creation").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u128);

impl SessionId {
    #[must_use]
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        let filled = getrandom::fill(&mut bytes);
        assert!(filled.is_ok(), "OS RNG failed: {filled:?}");
        Self(u128::from_be_bytes(bytes))
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// The terminal state the parse pipeline mutates as one unit, behind
/// a shared lock: the PTY parse thread owns the parse
/// (`crate::parse_sink`) while the session task keeps compose/fan-out.
pub struct ParseCore {
    pub parser: Parser,
    pub grid: Grid,
    /// Sweep policy for the grid's style and sizing registries. The same
    /// type the client's shadow owns, so the two drivers of the same
    /// registries cannot drift.
    pub table_gc: TableGc,
}

impl ParseCore {
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: Parser::new(),
            grid: Grid::new(rows, cols),
            table_gc: TableGc::new(),
        }
    }

    /// Runs on the PTY parse thread right after `Parser::advance`, where
    /// no external handle into either table is held (the encoders run on
    /// the session task under the same lock).
    pub fn maybe_gc_tables(&mut self) {
        self.table_gc.maybe_sweep(&mut self.grid);
    }
}

/// One owned session, held by its owner task (`serve::session_task`).
pub struct Session {
    /// Shared with the PTY parse thread's sink, which owns
    /// `Parser::advance`; lock via [`Session::lock_core`]. Not
    /// `std::sync::Mutex`: it never hands off to a waiter, so the sink's
    /// back-to-back relocks starve the pull answer under a flood
    /// (`docs/explanation/rendering/pipeline.md`).
    pub core: Arc<parking_lot::Mutex<ParseCore>>,
    pub signals: Arc<crate::parse_sink::ParseSignals>,
    /// Persists across detach for the rehydrate burst.
    pub images: ImageStore,
    /// Direct placements (`a=T` / `a=p`); persists across detach like
    /// `images`.
    pub placements: Placements,
    /// The primary screen's placements while the program is on the
    /// alternate screen (`?1049h`), restored on `?1049l`; `None` on the
    /// primary. Stashed entries still pin their images.
    pub saved_primary_placements: Option<Placements>,
    /// Chunked-transmission reassembler
    /// (docs/explanation/protocols/kitty-graphics.md "Dispatcher
    /// architecture").
    pub graphics_reassembler: Reassembler,
    /// Image-state changes the dispatcher produced this drain cycle,
    /// fanned to every subscriber. [`ImageEvent`] markers rather than
    /// `ImageMsg`: the pixels are read back out of `images` when shipped,
    /// so no second copy of a decoded image sits here.
    pub image_events: Vec<ImageEvent>,
    /// Scroll directives the effect replay parked this drain cycle for
    /// `serve::session_task::fan_out_grid_state`, each carrying the grid
    /// generation it describes. They ride the one ordered
    /// [`felis_grid::PtyEffect`] queue with the placement effects (a
    /// scroll before an `a=T` shifts a placement the same burst creates).
    pub scroll_ops: Vec<crate::serve::streaming::QueuedScroll>,
    /// Cell pixel size from `InputMsg::Resize`; the graphics dispatcher
    /// resolves auto placement extents (`c=0` / `r=0`) with it so the
    /// cursor advances past the image. Zero before any client attaches
    /// (see [`felis_grid::images::effective_extent`]).
    pub cell_pixel_w: u16,
    /// See [`Session::cell_pixel_w`].
    pub cell_pixel_h: u16,
    pub reader: PtyReader,
    pub writer: PtyWriter,
    /// Shared: the panic path needs it after this `Session` is gone, so
    /// the watchdog (`serve::session_task::supervise`) reaches it through
    /// an `Arc` taken before the task started.
    pub child: Arc<ChildHandle>,
    /// Shared: the roster build samples the foreground process group
    /// through it ([`SessionHandle::foreground_pgrp`]) without going
    /// through the owner task.
    pub resizer: Arc<Resizer>,
    /// POSIX shm object names this session's `t=s` transmissions opened,
    /// unlinked at session teardown rather than per read.
    ///
    /// Deferral prevents tearing down segments that producers like mpv reuse
    /// across frames ([`ShmDeferral`]).
    pub shm_segments: ShmDeferral,
}

impl Session {
    #[must_use]
    pub fn from_spawned(spawned: crate::SpawnedPty) -> Self {
        let crate::SpawnedPty {
            reader,
            writer,
            child,
            resizer,
            core,
            signals,
        } = spawned;
        Self {
            core,
            signals,
            images: ImageStore::new(DEFAULT_IMAGE_BYTE_CAP),
            placements: Placements::new(),
            saved_primary_placements: None,
            graphics_reassembler: Reassembler::new(),
            image_events: Vec::new(),
            scroll_ops: Vec::new(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            reader,
            writer,
            child: Arc::new(child),
            resizer: Arc::new(resizer),
            shm_segments: ShmDeferral::default(),
        }
    }

    pub fn lock_core(&self) -> parking_lot::MutexGuard<'_, ParseCore> {
        self.core.lock()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The producer is gone at teardown, so the deferred `t=s` unlinks
        // run now (see `shm_segments`); ENOENT from a producer that cleaned
        // up after itself is expected.
        for name in self.shm_segments.names() {
            crate::graphics::unlink_shm_segment(name);
        }
        // Closing the PTY master would not end the child: the reader thread
        // holds its own dup and lets go only at EOF, which is the child
        // exiting. Sessions the daemon runs are ended by
        // `session_task::end_child`; this bare hangup covers a `Session` that
        // never reached a task (the ones tests build directly).
        match self.child.hangup() {
            Ok(_) => {}
            Err(err) => tracing::warn!(?err, "session teardown: hangup"),
        }
    }
}

/// Metadata snapshot one session task mirrors for the pool, so
/// `OpsToClientMsg::Listed` needs nothing from the task. A `std::sync::Mutex`:
/// never held across an await.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub rows: u16,
    pub cols: u16,
    /// Window pixel width the size-owning client last reported, `0` when
    /// none has (the TIOCGWINSZ convention). Not grid-derived, so
    /// `refresh_meta` leaves it to the resize path like `tags`.
    pub pixel_w: u16,
    /// See [`SessionMeta::pixel_w`]: the vertical axis.
    pub pixel_h: u16,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// When the session last went parked (subscriber count hit 0), or its
    /// creation; the reap grace and the recency listing measure from here.
    pub idle_since: Instant,
    pub subscribers: usize,
    /// Opaque user labels (`OpsToDaemonMsg::Tag`). Not grid-derived: `refresh_meta`
    /// leaves them alone so an OSC-driven refresh never clobbers a user
    /// label.
    pub tags: BTreeSet<String>,
    /// Most recent decoded desktop notification (OSC 9/99/777).
    /// Event-sourced like `tags`, so `refresh_meta` leaves it alone.
    pub last_notification: Option<StoredNotification>,
    /// Shell exited; the session is lingering in the post-exit grace.
    /// Event-sourced like `tags`; shipped as `SessionInfo::exited`.
    pub exited: bool,
    /// Exit code of the youngest retained OSC 133 `D` mark, grid-derived;
    /// shipped as `SessionInfo::last_exit_code`.
    pub last_exit_code: Option<u32>,
    /// Live window attachments, oldest first (`SessionInfo::attachments`).
    /// Mirrored on attach, detach, and input-owner transfer; the last
    /// leaves the subscriber count unchanged, so this cannot ride the
    /// count comparison `idle_since` does.
    pub attachments: Vec<Attachment>,
    /// Creation sequence (`SessionInfo::sequence`), minted once at
    /// registration and never rewritten: a window holding a prior value
    /// would step through a ring that reordered under it.
    pub sequence: NonZeroU64,
}

/// A session's most recent notification plus the `Instant` it arrived,
/// so the `OpsToClientMsg::Listed` build stamps
/// `SessionNotification::age_seconds` itself.
#[derive(Debug, Clone)]
pub struct StoredNotification {
    /// Title, if the program set one. `None` for a bare-body OSC 9.
    pub title: Option<String>,
    pub body: String,
    /// Urgency; `Normal` for protocols carrying none.
    pub urgency: felis_protocol::messages::Urgency,
    pub at: Instant,
}

/// Apply a tag delta in place. Adds land before removes, so a tag named
/// in both ends up absent. A delta that would breach `MAX_SESSION_TAGS` /
/// `MAX_TAG_BYTES` returns `Err` with `tags` untouched, so the
/// `TagsUpdated` reply's snapshot stays honest.
pub fn apply_tag_delta(
    tags: &mut BTreeSet<String>,
    add: &[String],
    remove: &[String],
) -> Result<(), TagDeltaError> {
    if let Some(tag) = add.iter().find(|t| t.len() > MAX_TAG_BYTES) {
        return Err(TagDeltaError::TagTooLong {
            bytes: tag.len(),
            max: MAX_TAG_BYTES,
        });
    }
    let mut next = tags.clone();
    next.extend(add.iter().cloned());
    next.retain(|t| !remove.iter().any(|r| r == t));
    if next.len() > MAX_SESSION_TAGS {
        return Err(TagDeltaError::TooManyTags {
            would_be: next.len(),
            max: MAX_SESSION_TAGS,
        });
    }
    *tags = next;
    Ok(())
}

/// Why a tag delta was refused whole; rendered into
/// `TagsUpdated.denied`.
#[derive(Debug, PartialEq, Eq)]
pub enum TagDeltaError {
    TooManyTags { would_be: usize, max: usize },
    TagTooLong { bytes: usize, max: usize },
}

impl std::fmt::Display for TagDeltaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyTags { would_be, max } => {
                write!(
                    f,
                    "a session carries at most {max} tags (this delta would leave {would_be})"
                )
            }
            Self::TagTooLong { bytes, max } => {
                write!(f, "a tag is at most {max} bytes (got {bytes})")
            }
        }
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    pub cmd: mpsc::Sender<SessionCmd>,
    /// Permits are bytes of client input admitted but not yet written to
    /// the child. A connection acquires here before it hands the bytes
    /// to the session actor, so a child that stopped reading its stdin
    /// stops the *typist's* pump rather than growing daemon memory
    /// (`docs/reference/ipc.md` "Backpressure").
    pub input_budget: Arc<Semaphore>,
    pub meta: Arc<StdMutex<SessionMeta>>,
    /// `None` for a handle built without a PTY (test fixtures), which
    /// reports no foreground program.
    pub resizer: Option<Arc<Resizer>>,
}

/// One connection's admission against a session's input budget.
///
/// Type-erased so `OwnedSemaphorePermit` does not leak into [`SessionCmd`],
/// preventing command matches from holding reservations across whole arms.
pub type InputReservation = Box<dyn Send + 'static>;

/// A session's whole input budget, minted at registration.
#[must_use]
pub fn new_input_budget() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(felis_protocol::limits::PTY_INPUT_BUDGET))
}

impl SessionHandle {
    #[must_use]
    pub fn meta_snapshot(&self) -> SessionMeta {
        self.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The PTY's foreground process group (`tcgetpgrp`), sampled at the
    /// read rather than mirrored into [`SessionMeta`]: nothing the daemon
    /// parses announces a `fork`/`exec` behind the PTY.
    #[must_use]
    pub fn foreground_pgrp(&self) -> Option<i32> {
        self.resizer.as_ref().and_then(|r| r.foreground_pgrp())
    }

    #[must_use]
    pub fn idle_since(&self) -> Instant {
        self.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .idle_since
    }
}

/// Most recently parked first, ties on the raw id so a quiescent pool
/// orders the same each run.
fn by_recency(a: &(SessionId, Instant), b: &(SessionId, Instant)) -> Ordering {
    b.1.cmp(&a.1).then_with(|| a.0.0.cmp(&b.0.0))
}

/// The daemon-global attachment-id allocator (see
/// [`SessionPool::attachment_ids`]).
#[derive(Clone)]
pub struct AttachmentIds(Arc<AtomicU64>);

impl AttachmentIds {
    pub fn mint(&self) -> u64 {
        self.0.fetch_add(1, AtomicOrdering::Relaxed)
    }

    /// A standalone allocator for a session task built without a pool.
    #[cfg(test)]
    #[must_use]
    pub fn detached() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }
}

/// Session slot admitted against [`MAX_SESSIONS`] but not yet registered.
///
/// Held across PTY fork/exec outside the pool lock to prevent concurrent
/// bursts from exceeding the session cap (REQ-915). Released on `Drop`.
#[derive(Debug)]
pub struct SessionSlot {
    reserved: Arc<AtomicUsize>,
    settled: Arc<Notify>,
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        // `Release` pairs with the `Acquire` in `try_reserve`: a rollback
        // drop runs outside the pool lock.
        self.reserved.fetch_sub(1, AtomicOrdering::Release);
        // Registration drops the slot too, so a drain that woke on this
        // re-reads the count rather than concluding anything from the
        // wake itself.
        self.settled.notify_waiters();
    }
}

/// A session already out of the pool whose child is still being hung
/// up and reaped. Removal happens before that teardown, so a drain
/// trusting [`SessionPool::admitted`] alone could fire the shutdown
/// while a child this daemon owns is still running. Released on
/// `Drop`: an unwind inside the teardown must not strand the count.
#[derive(Debug)]
pub struct TeardownGuard {
    tearing_down: Arc<AtomicUsize>,
    settled: Arc<Notify>,
}

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        self.tearing_down.fetch_sub(1, AtomicOrdering::Release);
        self.settled.notify_waiters();
    }
}

/// Why [`SessionPool::try_reserve`] admitted nothing. The two are
/// different refusals to the caller: a cap clears when a session is
/// reaped, a drain never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveRefusal {
    /// Registered plus reserved is at the cap; the count as read.
    AtCapacity { admitted: usize },
    /// The daemon is on its way out.
    Draining,
}

/// Visibility of a registered session in by-name operations.
///
/// [`Listing::Held`] keeps newly spawned sessions unnameable until their
/// creator publishes them via [`SessionPool::publish`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listing {
    /// Reachable as soon as it is registered.
    Public,
    /// Registered but unnameable until published.
    Held,
}

/// Registry of live sessions.
pub struct SessionPool {
    sessions: HashMap<SessionId, SessionHandle>,
    /// Rows registered [`Listing::Held`] and not yet published. They
    /// count against the cap, their child being already running, but
    /// answer no lookup.
    held: HashSet<SessionId>,
    /// Slots admitted by [`SessionPool::try_reserve`] whose sessions have
    /// not reached [`SessionPool::register`]. An `Arc` because
    /// [`SessionSlot`] releases on drop, off the pool lock.
    reserved: Arc<AtomicUsize>,
    /// Set by [`OpsToDaemonMsg::Stop`](felis_protocol::messages::OpsToDaemonMsg::Stop):
    /// every admission is refused from here on, so the count this
    /// pool reports can only fall.
    draining: bool,
    /// Woken whenever [`Self::admitted`] may have reached zero (a slot
    /// released, a session removed). The count is the truth; this only
    /// says when to re-read it.
    settled: Arc<Notify>,
    /// Sessions removed from [`Self::sessions`] whose children are still
    /// being reaped. An `Arc` because [`TeardownGuard`] releases on
    /// drop, off the pool lock.
    tearing_down: Arc<AtomicUsize>,
    /// Creation-sequence allocator, monotonic and never reused. Separate
    /// from [`Self::attachment_ids`]: one shared counter would make a
    /// session's sequence jump by however many windows attached in
    /// between.
    sequences: AtomicU64,
    /// Attachment-id allocator, daemon-global so an id names one window
    /// across the whole daemon; monotonic and never reused, so a stale id
    /// resolves to "gone" rather than to whoever came next.
    attachment_ids: AttachmentIds,
    /// Daemon-global notification hub
    /// (docs/reference/protocols/notifications.md): session tasks publish,
    /// `felis notifications subscribe` observers receive.
    notify_hub: broadcast::Sender<NotifyToClientMsg>,
}

impl Default for SessionPool {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionPool {
    #[must_use]
    pub fn new() -> Self {
        // A slow observer that falls 256 behind sees a typed
        // `NotifyToClientMsg::Lagged` marker for the gap, never a silent drop.
        let (notify_hub, _) = broadcast::channel(256);
        Self {
            sessions: HashMap::new(),
            held: HashSet::new(),
            reserved: Arc::new(AtomicUsize::new(0)),
            draining: false,
            settled: Arc::new(Notify::new()),
            tearing_down: Arc::new(AtomicUsize::new(0)),
            sequences: AtomicU64::new(0),
            attachment_ids: AttachmentIds(Arc::new(AtomicU64::new(0))),
            notify_hub,
        }
    }

    /// Mint the next creation sequence, under the pool lock the
    /// registration takes, so pool order is sequence order. Starts at 1
    /// because the wire spells `0` as a missing field, which the reader
    /// refuses.
    pub fn next_sequence(&self) -> NonZeroU64 {
        NonZeroU64::MIN.saturating_add(self.sequences.fetch_add(1, AtomicOrdering::Relaxed))
    }

    /// Grabbed once per task so a subscribe never takes the pool lock to
    /// number itself.
    #[must_use]
    pub fn attachment_ids(&self) -> AttachmentIds {
        self.attachment_ids.clone()
    }

    #[must_use]
    pub fn notify_hub(&self) -> broadcast::Sender<NotifyToClientMsg> {
        self.notify_hub.clone()
    }

    #[must_use]
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<NotifyToClientMsg> {
        self.notify_hub.subscribe()
    }

    /// Take one slot if the pool is not draining and is below `max`,
    /// counting slots admitted and not yet registered. Called with the
    /// pool lock held, which makes the drain check, the count and the
    /// increment one critical section: that is what stops a create from
    /// landing after a stop decided the pool was empty.
    pub fn try_reserve(&self, max: usize) -> Result<SessionSlot, ReserveRefusal> {
        if self.draining {
            return Err(ReserveRefusal::Draining);
        }
        let admitted = self.sessions.len() + self.reserved.load(AtomicOrdering::Acquire);
        if admitted >= max {
            return Err(ReserveRefusal::AtCapacity { admitted });
        }
        self.reserved.fetch_add(1, AtomicOrdering::Release);
        Ok(SessionSlot {
            reserved: Arc::clone(&self.reserved),
            settled: Arc::clone(&self.settled),
        })
    }

    /// Refuse every further admission. Idempotent, and one-way: nothing
    /// resumes a daemon that has begun to exit.
    pub const fn start_draining(&mut self) {
        self.draining = true;
    }

    #[must_use]
    pub const fn draining(&self) -> bool {
        self.draining
    }

    /// The wake a drain waits on. Held as an `Arc` so the waiter does
    /// not keep the pool locked between re-reads.
    #[must_use]
    pub fn settled(&self) -> Arc<Notify> {
        Arc::clone(&self.settled)
    }

    /// Count a session out of the pool but not yet reaped. Taken before
    /// [`Self::remove`] and held until the child is gone.
    #[must_use]
    pub fn begin_teardown(&self) -> TeardownGuard {
        self.tearing_down.fetch_add(1, AtomicOrdering::Release);
        TeardownGuard {
            tearing_down: Arc::clone(&self.tearing_down),
            settled: Arc::clone(&self.settled),
        }
    }

    /// Nothing admitted and nothing left to reap: what a stop waits for
    /// before the shutdown fires. [`Self::admitted`] alone would let the
    /// process exit during the last child's hangup.
    #[must_use]
    pub fn quiescent(&self) -> bool {
        self.admitted() == 0 && self.tearing_down.load(AtomicOrdering::Acquire) == 0
    }

    /// Take every registered session out of the pool, held rows
    /// included: a forced stop must reach the sessions a create has
    /// registered but not yet named to its caller.
    pub fn drain_sessions(&mut self) -> Vec<SessionHandle> {
        self.held.clear();
        let handles: Vec<_> = self.sessions.drain().map(|(_, handle)| handle).collect();
        self.settled.notify_waiters();
        handles
    }

    /// Every registered session's handle, held rows included, left in
    /// the pool. A drain that only waits for the count to fall would
    /// miss the teardown a session task runs *after* it removes itself,
    /// so the waiter needs the handles of the sessions it must outlive.
    #[must_use]
    pub fn live_handles(&self) -> Vec<SessionHandle> {
        self.sessions.values().cloned().collect()
    }

    /// Register a spawned session task's handle, releasing its admission
    /// slot in the same critical section: released a statement earlier the
    /// cap over-admits, a statement later it double-counts this session.
    pub fn register(
        &mut self,
        id: SessionId,
        handle: SessionHandle,
        slot: Option<SessionSlot>,
        listing: Listing,
    ) {
        self.sessions.insert(id, handle);
        if listing == Listing::Held {
            self.held.insert(id);
        }
        drop(slot);
    }

    /// Make a [`Listing::Held`] row nameable. `false`: the row is gone
    /// (an instantly exiting child reaps itself), so the caller is
    /// publishing nothing.
    pub fn publish(&mut self, id: SessionId) -> bool {
        self.held.remove(&id);
        self.sessions.contains_key(&id)
    }

    #[must_use]
    pub fn get(&self, id: SessionId) -> Option<&SessionHandle> {
        if self.held.contains(&id) {
            return None;
        }
        self.sessions.get(&id)
    }

    #[must_use]
    pub fn handle_cloned(&self, id: SessionId) -> Option<SessionHandle> {
        self.get(id).cloned()
    }

    /// Dropping the handle kills nothing (the task owns the `Session`),
    /// which is why `DestroySession` follows up with
    /// `SessionCmd::Shutdown`.
    pub fn remove(&mut self, id: SessionId) -> Option<SessionHandle> {
        self.held.remove(&id);
        let removed = self.sessions.remove(&id);
        self.settled.notify_waiters();
        removed
    }

    /// Every registered session, held rows included: a held session
    /// holds a PTY child and an admission slot, so a count that skipped
    /// it would disagree with [`Self::admitted`], which is what the cap
    /// is checked against and what `felis daemon status` reports. The
    /// by-name paths (`get`, `ids`, the rosters) skip held rows instead.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// What [`Self::try_reserve`] checks against the cap: registered plus
    /// reserved. A cap refusal and the `sessions` status row report this
    /// rather than [`Self::len`] so a burst never reads "at 1 of 3" while
    /// the cap is full of reservations.
    #[must_use]
    pub fn admitted(&self) -> usize {
        self.sessions.len() + self.reserved.load(AtomicOrdering::Acquire)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    fn published(&self) -> impl Iterator<Item = (&SessionId, &SessionHandle)> {
        self.sessions
            .iter()
            .filter(|(id, _)| !self.held.contains(id))
    }

    /// Arbitrary `HashMap` order; `OpsToDaemonMsg::List` uses
    /// [`Self::roster_by_recency`]. Prefix resolution runs off this, so
    /// held rows are skipped.
    pub fn ids(&self) -> impl Iterator<Item = SessionId> + '_ {
        self.sessions
            .keys()
            .copied()
            .filter(|id| !self.held.contains(id))
    }

    /// Most recently parked first.
    #[must_use]
    pub fn ids_by_recency(&self) -> Vec<SessionId> {
        let mut ids: Vec<_> = self
            .published()
            .map(|(id, handle)| (*id, handle.idle_since()))
            .collect();
        ids.sort_by(by_recency);
        ids.into_iter().map(|(id, _)| id).collect()
    }

    /// [`Self::ids_by_recency`]'s order with each row's metadata and a
    /// fresh foreground-pgrp sample, one clone per session.
    #[must_use]
    pub fn roster_by_recency(&self) -> Vec<(SessionId, SessionMeta, Option<i32>)> {
        let mut rows: Vec<_> = self
            .published()
            .map(|(id, handle)| (*id, handle.meta_snapshot(), handle.foreground_pgrp()))
            .collect();
        rows.sort_by(|a, b| by_recency(&(a.0, a.1.idle_since), &(b.0, b.1.idle_since)));
        rows
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use super::*;

    /// The parse path must drive the sweep: a producer that repaints sized
    /// text mints an entry per repaint, and the table would walk to the
    /// `u16` handle cap.
    #[test]
    fn repainted_sizing_runs_are_reclaimed_before_the_handle_space_runs_out() {
        let mut core = ParseCore::new(4, 20);
        let trigger = core.table_gc.sizing_threshold();
        for _ in 0..(trigger * 3) {
            core.parser
                .advance(&mut core.grid, b"\x1b[H\x1b]66;s=2;A\x1b\\");
            core.maybe_gc_tables();
        }
        assert!(
            core.grid.sizing_count() <= trigger,
            "the sweep must hold the table near its trigger point, got {}",
            core.grid.sizing_count(),
        );
        assert_eq!(
            core.table_gc.sizing_threshold(),
            trigger,
            "a sweep that reclaims almost everything must not raise the bar",
        );
    }

    #[test]
    fn text_printed_after_a_parse_path_sweep_keeps_its_truecolor_pen() {
        let mut core = ParseCore::new(4, 20);
        for i in 0..=core.table_gc.style_threshold() {
            let (g, b) = (i / 256 % 256, i % 256);
            core.parser
                .advance(&mut core.grid, format!("\x1b[48;2;0;{g};{b}m").as_bytes());
        }
        core.parser
            .advance(&mut core.grid, b"\x1b[0m\x1b[38;2;0;16;0m");
        let before = core.grid.style_table_len();
        core.maybe_gc_tables();
        assert!(
            core.grid.style_table_len() < before,
            "the sweep must compact"
        );

        core.parser.advance(&mut core.grid, b"A");

        let cell = core.grid.cell(0, 0).expect("in bounds");
        assert_eq!(
            core.grid.style(cell.style).fg,
            felis_grid::Color::Rgb(0, 16, 0)
        );
    }

    /// Slash-prefixed, the way a producer creates it (mpv's
    /// `/mpv-kitty-<ptr>`).
    #[cfg(unix)]
    fn create_shm(name: &str) {
        let fd = rustix::shm::open(
            name,
            rustix::shm::OFlags::CREATE | rustix::shm::OFlags::EXCL | rustix::shm::OFlags::RDWR,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .expect("create test shm");
        rustix::fs::ftruncate(&fd, 4).expect("size test shm");
    }

    #[cfg(unix)]
    fn shm_exists(name: &str) -> bool {
        rustix::shm::open(name, rustix::shm::OFlags::RDONLY, rustix::fs::Mode::empty()).is_ok()
    }

    /// `Drop` is the only thing that honors the spec's "the terminal must
    /// delete the object" for deferred segments. Names are recorded
    /// slash-less, as mpv transmits them, so the sweep's macOS slash
    /// restore is on the hook too.
    #[test]
    #[cfg(unix)]
    fn dropping_a_session_unlinks_its_deferred_shm_segments() {
        let mut cmd = felis_pty::Command::new("/bin/sh");
        cmd.args(["-c", "read _x"]);
        cmd.env_clear();
        cmd.env("PATH", "/bin:/usr/bin");
        let mut session = Session::from_spawned(crate::SpawnedPty::spawn(cmd).expect("spawn sh"));

        let pid = std::process::id();
        let names = [
            format!("/felis-teardown-a-{pid}"),
            format!("/felis-teardown-b-{pid}"),
        ];
        for name in &names {
            create_shm(name);
            assert!(
                session
                    .shm_segments
                    .record(name.trim_start_matches('/').to_owned())
                    .is_none(),
                "two names fit well under the deferral cap",
            );
        }

        drop(session);

        let leaked: Vec<String> = names
            .iter()
            .filter(|name| shm_exists(name))
            .cloned()
            .collect();
        // Unlink before asserting, or a regression makes the next run's
        // `EXCL` create fail for the wrong reason.
        for name in &leaked {
            let _ = rustix::shm::unlink(name.as_str());
        }
        assert!(
            leaked.is_empty(),
            "session teardown left shm segments behind: {leaked:?}",
        );
    }

    fn tag_set<const N: usize>(tags: [&str; N]) -> BTreeSet<String> {
        tags.into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn apply_tag_delta_removes_named_tags() {
        let mut tags = tag_set(["agent", "work"]);
        apply_tag_delta(&mut tags, &[], &["work".to_owned()]).unwrap();
        assert_eq!(tags, tag_set(["agent"]));
    }

    #[test]
    fn apply_tag_delta_remove_wins_over_add_in_one_call() {
        let mut tags = BTreeSet::new();
        apply_tag_delta(&mut tags, &["x".to_owned()], &["x".to_owned()]).unwrap();
        assert_eq!(tags, BTreeSet::new());
    }

    #[test]
    fn apply_tag_delta_refuses_a_cap_breach_without_partial_apply() {
        let mut tags: BTreeSet<String> =
            (0..MAX_SESSION_TAGS).map(|i| format!("t{i:02}")).collect();
        let before = tags.clone();
        let err = apply_tag_delta(&mut tags, &["one-more".to_owned()], &[]).unwrap_err();
        assert_eq!(
            err,
            TagDeltaError::TooManyTags {
                would_be: MAX_SESSION_TAGS + 1,
                max: MAX_SESSION_TAGS,
            }
        );
        assert_eq!(tags, before, "a refused delta must not partially apply");

        // The cap judges the resulting set, not the add list.
        let first = before.iter().next().expect("the cap is not zero").clone();
        apply_tag_delta(&mut tags, &["one-more".to_owned()], &[first]).unwrap();
        assert_eq!(tags.len(), MAX_SESSION_TAGS);
    }

    #[test]
    fn apply_tag_delta_refuses_an_oversized_tag() {
        let mut tags = BTreeSet::new();
        let big = "x".repeat(MAX_TAG_BYTES + 1);
        let err = apply_tag_delta(&mut tags, &[big], &[]).unwrap_err();
        assert_eq!(
            err,
            TagDeltaError::TagTooLong {
                bytes: MAX_TAG_BYTES + 1,
                max: MAX_TAG_BYTES,
            }
        );
        assert_eq!(tags, BTreeSet::new());
        apply_tag_delta(&mut tags, &["y".repeat(MAX_TAG_BYTES)], &[]).unwrap();
        assert_eq!(tags.len(), 1);
    }

    #[test]
    fn session_ids_do_not_collide_across_a_burst() {
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(SessionId::new()), "id collision in burst");
        }
    }

    fn dummy_handle(idle_since: Instant) -> SessionHandle {
        let (cmd, _rx) = mpsc::channel(1);
        SessionHandle {
            cmd,
            input_budget: new_input_budget(),
            resizer: None,
            meta: Arc::new(StdMutex::new(SessionMeta {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLS,
                pixel_w: 0,
                pixel_h: 0,
                title: None,
                cwd: None,
                idle_since,
                subscribers: 0,
                tags: BTreeSet::new(),
                last_notification: None,
                last_exit_code: None,
                exited: false,
                attachments: Vec::new(),
                sequence: NonZeroU64::MIN,
            })),
        }
    }

    #[test]
    fn register_and_lookup_round_trips() {
        let mut pool = SessionPool::new();
        let id = SessionId::new();
        pool.register(id, dummy_handle(Instant::now()), None, Listing::Public);
        assert_eq!(pool.len(), 1);
        assert!(pool.get(id).is_some());
        assert!(pool.handle_cloned(id).is_some());
        assert!(pool.remove(id).is_some());
        assert!(pool.is_empty());
    }

    /// A held row answers no lookup and appears in no roster, yet still
    /// counts against the session cap.
    #[test]
    fn a_held_registration_is_nameless_until_it_is_published() {
        let mut pool = SessionPool::new();
        let id = SessionId::new();
        pool.register(id, dummy_handle(Instant::now()), None, Listing::Held);

        assert!(pool.get(id).is_none());
        assert!(pool.handle_cloned(id).is_none());
        assert_eq!(pool.ids().count(), 0);
        assert_eq!(pool.ids_by_recency(), Vec::<SessionId>::new());
        assert!(pool.roster_by_recency().is_empty());
        assert_eq!(pool.len(), 1, "the held child still holds a cap slot");

        assert!(pool.publish(id));
        assert!(pool.get(id).is_some());
        assert_eq!(pool.ids_by_recency(), vec![id]);
    }

    /// A child that reaps itself before the create publishes it leaves
    /// `publish` with nothing to name.
    #[test]
    fn publishing_a_session_that_already_left_the_pool_reports_nothing() {
        let mut pool = SessionPool::new();
        let id = SessionId::new();
        pool.register(id, dummy_handle(Instant::now()), None, Listing::Held);
        assert!(pool.remove(id).is_some());

        assert!(!pool.publish(id));
        assert!(pool.is_empty());
    }

    #[test]
    fn ids_by_recency_orders_most_recently_idle_first() {
        let mut pool = SessionPool::new();
        let now = Instant::now();
        let first = SessionId::new();
        let second = SessionId::new();
        let third = SessionId::new();
        pool.register(
            first,
            dummy_handle(now.checked_sub(Duration::from_secs(30)).unwrap()),
            None,
            Listing::Public,
        );
        pool.register(
            second,
            dummy_handle(now.checked_sub(Duration::from_secs(20)).unwrap()),
            None,
            Listing::Public,
        );
        pool.register(
            third,
            dummy_handle(now.checked_sub(Duration::from_secs(10)).unwrap()),
            None,
            Listing::Public,
        );

        let ordered = pool.ids_by_recency();
        assert_eq!(
            ordered,
            vec![third, second, first],
            "newest idle must come first; got {ordered:?}",
        );
    }

    #[test]
    fn ids_by_recency_breaks_idle_since_ties_by_id_for_determinism() {
        let mut pool = SessionPool::new();
        let now = Instant::now();
        let a = SessionId::new();
        let b = SessionId::new();
        pool.register(a, dummy_handle(now), None, Listing::Public);
        pool.register(b, dummy_handle(now), None, Listing::Public);

        let ordered = pool.ids_by_recency();
        let mut expected = [a, b];
        expected.sort_by_key(|id| id.0);
        assert_eq!(
            ordered,
            expected.to_vec(),
            "tie-broken order must match raw-id ascending; got {ordered:?}",
        );
    }
}

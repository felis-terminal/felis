//! Per-session stream composition and parsed-effect replay.
//!
//! Composition is synchronous into typed [`OutEvent`] buffers without awaiting sockets;
//! the session task never blocks on subscriber wires (session-lifecycle.md).

use std::num::{NonZeroU16, NonZeroU32};

use super::registry_sync::{RowHandles, SentRegistries};
use super::session_task::OutEvent;
use super::{
    ConnError, ConnectionMode, Grid, GridMsg, ImageMsg, Instant, PtyEffect, SessionId, ThemeChannel,
};
use felis_grid::{Damage, RowEncode, ScrollOp, ViewportRowView, encode_row};
use felis_protocol::messages::{AttentionSource, Notification, PaletteAction, ThemeAction};
use felis_protocol::{RowPayload, messages::NotifyToClientMsg};
use felis_transport::{CheckedFrame, TransportError};

use crate::graphics::ImageEvent;

/// Replay the side-effects the parse queued into the grid: query and
/// graphics responses back to the PTY, placement shifts and evictions,
/// alt-screen save/restore. Never touches a subscriber: it leaves the
/// grid and `session.image_events` / `session.scroll_ops` for the
/// session task to fan out.
pub(crate) fn drain_effects(
    session: &mut crate::pool::Session,
    core: &mut crate::pool::ParseCore,
) -> Result<(), ConnError> {
    // The replay's one grid mutation, the cursor advance past an image,
    // can scroll and queue effects of its own; they replay in this drain,
    // and anything a later pass would still queue waits for the next.
    for _ in 0..3 {
        let effects = core.grid.take_pty_effects();
        if effects.is_empty() {
            break;
        }
        replay_effects(session, core, effects)?;
    }
    Ok(())
}

fn shift_placements(session: &mut crate::pool::Session, n: u32, retain: u32) {
    let had_placements = !session.placements.is_empty();
    let evicted = session.placements.shift_up(n, retain);
    if had_placements {
        session
            .image_events
            .push(ImageEvent::PlacementsShifted { lines: n });
    }
    for p in evicted {
        session.images.release(p.image_id);
        session.image_events.push(ImageEvent::PlacementRemoved {
            image_id: p.image_id,
            placement_id: p.placement_id,
        });
    }
}

fn replay_effects(
    session: &mut crate::pool::Session,
    core: &mut crate::pool::ParseCore,
    effects: Vec<PtyEffect>,
) -> Result<(), ConnError> {
    // Replay in byte-stream order so interleaved effects within a burst do not
    // invert. Query responses flow even with zero subscribers so programs do not hang.
    for effect in effects {
        match effect {
            // Kitty's erase-evicts-placements semantics.
            PtyEffect::Erased(range) => {
                let removed =
                    session
                        .placements
                        .remove_intersecting(range.top, range.bottom, range.force);
                for p in removed {
                    session.images.release(p.image_id);
                    session.image_events.push(ImageEvent::PlacementRemoved {
                        image_id: p.image_id,
                        placement_id: p.placement_id,
                    });
                }
            }
            // docs/explanation/protocols/kitty-graphics.md "Dispatcher
            // architecture".
            PtyEffect::Apc(apc) => {
                let responses =
                    crate::graphics::dispatch_apc_bodies(session, &mut core.grid, vec![apc]);
                for response in responses {
                    write_pty(&session.writer, &response)?;
                }
            }
            PtyEffect::Response(bytes) => {
                write_pty(&session.writer, &bytes)?;
            }
            // Anchors that stay within retained scrollback survive with a
            // non-positive row (docs/reference/protocols/kitty-graphics.md
            // "Scrollback-anchored placements"), mirrored as one
            // `PlacementsShifted` rather than re-sending each survivor; anchors
            // past the retention horizon evict.
            PtyEffect::ScrolledIntoScrollback(n) => {
                let retain = u32::try_from(core.grid.scrollback().len()).unwrap_or(u32::MAX);
                shift_placements(session, n, retain);
            }
            // The alternate screen keeps no history, so a placement that
            // leaves its top leaves for good.
            PtyEffect::AltScreenScrolled(n) => {
                shift_placements(session, n, 0);
            }
            // Parked for `session_task::fan_out_grid_state`, the only consumer
            // that knows each shadow's damage.
            PtyEffect::Scrolled {
                op,
                geometry_gen,
                first_seq,
                last_seq,
            } => {
                session.scroll_ops.push(QueuedScroll {
                    geometry_gen,
                    first_seq,
                    last_seq,
                    op,
                });
            }
            // Replayed at stream position, after the switch restored the saved
            // primary placements (REQ-604).
            PtyEffect::PrimaryReflowed(remap) => {
                crate::graphics::apply_reflow_remap(
                    &remap,
                    &mut session.images,
                    &mut session.placements,
                    &mut session.image_events,
                );
            }
            // A placement created after the toggle in the same burst stays on
            // the screen it was created on.
            PtyEffect::ScreenSwitch(switch) => {
                crate::graphics::apply_screen_switch(
                    switch,
                    &mut session.images,
                    &mut session.placements,
                    &mut session.saved_primary_placements,
                    &mut session.image_events,
                );
            }
            // REQ-902: disabling the paste guard must not be silent.
            PtyEffect::BracketedPasteDisabled => {
                tracing::warn!(
                    "program disabled bracketed paste (?2004l); pastes to this session are unframed"
                );
            }
        }
    }
    Ok(())
}

/// Retained scrollback rows + live rows; mirrors the renderer's
/// scrollbar-thumb math.
pub(crate) fn viewport_max_for(grid: &Grid) -> u32 {
    let sb = u32::try_from(grid.scrollback().len()).unwrap_or(u32::MAX);
    sb.saturating_add(u32::from(grid.rows()))
}

/// Absolute ordinal one past the last prompt mark, in the never-pruned
/// numbering the diff loop counts in.
pub(crate) fn prompt_marks_ordinal_end(grid: &Grid) -> usize {
    usize::try_from(grid.prompt_marks_pruned())
        .unwrap_or(usize::MAX)
        .saturating_add(grid.prompt_marks().len())
}

/// Per-subscriber mirrors of the last value `compose_diffs` shipped,
/// plus the subscriber's viewport: wire dedup state, so two clients on
/// one session each track their own.
pub(crate) struct DiffStreamState {
    pub(crate) last_cursor: felis_grid::Cursor,
    pub(crate) last_cursor_style: felis_protocol::messages::CursorStyle,
    pub(crate) last_cursor_blink: bool,
    /// Absolute ordinal in the never-pruned mark numbering, not a live
    /// index: the grid front-prunes evicted marks, so subtract
    /// [`Grid::prompt_marks_pruned`] to index the retained slice.
    pub(crate) prompt_marks_sent: usize,
    pub(crate) last_mode_flags: felis_grid::ModeSnapshot,
    /// Scrollback offset this subscriber is browsing at (0 = live).
    pub(crate) viewport: u32,
    /// Last `(lines_from_bottom, max)` shipped as `ViewportState`.
    pub(crate) last_viewport_state: (u32, u32),
}

impl DiffStreamState {
    /// Seeded from the snapshot `compose_rehydrate` just shipped, so the
    /// first diff cycle emits only on drift. Rehydrate always presents the
    /// live bottom, so the viewport starts at 0.
    pub(crate) fn seeded_from(grid: &Grid, prompt_marks_sent: usize) -> Self {
        Self {
            last_cursor: grid.cursor(),
            last_cursor_style: grid.cursor_style(),
            last_cursor_blink: grid.cursor_blink(),
            prompt_marks_sent,
            last_mode_flags: grid.mode_snapshot(),
            viewport: 0,
            last_viewport_state: (0, viewport_max_for(grid)),
        }
    }
}

/// One parked scroll effect: the directive, the geometry it belongs to,
/// and the span of the grid's scroll order it covers.
pub struct QueuedScroll {
    pub geometry_gen: u64,
    pub first_seq: u64,
    pub last_seq: u64,
    pub op: ScrollOp,
}

/// Past this many queued directives a subscriber takes the band as rows
/// instead: a mirror that stopped pulling must not grow the queue
/// without bound.
const PENDING_SCROLLS_CAP: usize = 1024;

/// Everything one subscriber's diff stream accumulates between ships
/// (docs/explanation/architecture/session-lifecycle.md "Same-user
/// mirroring"): subscribers at different pull cadences each hold
/// exactly what they have not shipped yet.
pub(crate) struct SubscriberStream {
    pub(crate) diff: DiffStreamState,
    pub(crate) damage: Damage,
    /// Scroll directives accepted for this subscriber; see
    /// [`SubscriberStream::accept_scroll`].
    pub(crate) pending_scrolls: Vec<ScrollOp>,
    /// Rows the pending directives leave vacated on the mirror, where
    /// the shadow keeps the rotated-out cells as unread storage. A
    /// released client skips a row write that matches that storage, so
    /// compose restates these rows through a blank first.
    pub(crate) vacated: Damage,
    /// Registry entries this subscriber has received; meaningful only
    /// while [`Self::mirrors_grid`].
    pub(crate) sent: SentRegistries,
    /// The grid's scroll order as of this subscriber's last compose. A
    /// directive at or below it is already in the rows it holds, and
    /// applying it again would shift them twice.
    pub(crate) composed_through_scroll: u64,
    /// The last scroll sequence a fan-out offered this subscriber.
    /// A compose that finds the grid past it is reading cells no
    /// directive it holds accounts for.
    pub(crate) admitted_through_scroll: u64,
    /// Only a window mirrors the grid. A scripted `Ops` attach reads the
    /// session through `Region` / `Search` replies, so rows shipped to it
    /// would be decoded and dropped, and it would pay the per-cell handle
    /// walk on every dirty row for the life of the connection.
    mirrors_grid: bool,
}

impl SubscriberStream {
    /// Compose the attach burst for `mode` into `out` and seed stream state.
    ///
    /// Mode chooses burst contents before composition so non-window modes avoid
    /// traversing the image store only to discard it.
    pub(crate) fn rehydrated(
        mode: ConnectionMode,
        grid: &Grid,
        images: &felis_grid::images::ImageStore,
        placements: &felis_grid::images::Placements,
        out: &mut Vec<OutEvent>,
    ) -> Result<Self, ConnError> {
        let mut sent = SentRegistries::default();
        if mode.is_window() {
            compose_rehydrate(grid, images, placements, &mut sent, out)?;
        } else {
            compose_ops_rehydrate(grid, out);
        }
        Ok(Self {
            diff: DiffStreamState::seeded_from(grid, prompt_marks_ordinal_end(grid)),
            damage: Damage::new(usize::from(grid.rows())),
            pending_scrolls: Vec::new(),
            vacated: Damage::new(usize::from(grid.rows())),
            sent,
            composed_through_scroll: grid.scroll_seq(),
            admitted_through_scroll: grid.scroll_seq(),
            mirrors_grid: mode.is_window(),
        })
    }

    /// The guard that keeps the per-cell handle walk off every cycle a
    /// window is already caught up on. Only asked about a subscriber that
    /// mirrors the grid.
    const fn owes_registry_entries(&self, grid: &Grid) -> bool {
        !self
            .sent
            .caught_up(grid.hyperlink_count(), grid.cluster_count())
    }

    /// A resize retires every queued directive: the region it names
    /// belongs to the geometry it was recorded under, and the replay
    /// that follows the resize restates those rows anyway.
    pub(crate) fn retire_scrolls(&mut self, rows: u16) {
        self.damage.resize(usize::from(rows));
        self.clear_scrolls();
    }

    fn clear_scrolls(&mut self) {
        self.pending_scrolls.clear();
        self.vacated = Damage::new(self.damage.len());
    }

    /// Admit one parked scroll against this subscriber's own accounting.
    /// `current_gen` / `rows` are the grid's geometry as of the fan-out.
    pub(crate) fn offer_scroll(&mut self, queued: &QueuedScroll, current_gen: u64, rows: u16) {
        if queued.geometry_gen != current_gen {
            // Produced before a resize this drain has already applied:
            // the rows it would shift are not the rows it named.
            self.retire_scrolls(rows);
        } else if queued.last_seq <= self.composed_through_scroll {
            // This subscriber's last compose read the grid past the
            // shift, so its rows already hold it.
        } else if queued.first_seq <= self.composed_through_scroll {
            // Coalesced across that compose: part of the shift is in
            // the rows already, and the entry cannot say how much.
            self.retire_scrolls(rows);
        } else {
            self.accept_scroll(queued.op);
        }
        self.admitted_through_scroll = self.admitted_through_scroll.max(queued.last_seq);
    }

    /// Called after the parked scrolls were offered and before the grid's
    /// damage merges in. That damage is in the coordinates of every shift
    /// the grid has made, so a shift still queued in the grid, offered
    /// later, would move rows already marked at their new place.
    pub(crate) fn catch_up_to(&mut self, grid: &Grid) {
        if grid.scroll_seq() > self.admitted_through_scroll {
            self.retire_scrolls(grid.rows());
            self.admitted_through_scroll = grid.scroll_seq();
        }
    }

    /// The rows this subscriber still owes move with the shift, as the
    /// grid's own damage did, so the directive is sound however far
    /// behind the subscriber is
    /// (docs/explanation/rendering/damage-tracking.md).
    pub(crate) fn accept_scroll(&mut self, op: ScrollOp) {
        let (top, bottom) = (usize::from(op.region_top), usize::from(op.region_bottom));
        self.damage
            .shift_band(top, bottom, usize::from(op.n_rows), op.direction);
        self.vacated
            .shift_band(top, bottom, usize::from(op.n_rows), op.direction);
        let band_owed = self.damage.all_dirty_in_range(top, bottom + 1);
        let folds = self.pending_scrolls.last().is_some_and(|tail| {
            tail.region_top == op.region_top
                && tail.region_bottom == op.region_bottom
                && tail.direction == op.direction
        });
        if folds && band_owed {
            self.pending_scrolls.pop();
        } else if let (true, Some(tail)) = (folds, self.pending_scrolls.last_mut()) {
            // Each vacated row stays owed until the next compose, so two
            // shifts that did not leave the band owed whole sum to less
            // than its height; the cap only guards that reasoning.
            let height = op.region_bottom - op.region_top + 1;
            tail.n_rows = tail.n_rows.saturating_add(op.n_rows).min(height);
        } else if band_owed {
            // Nothing to ship: the replay restates the band.
        } else if self.pending_scrolls.len() < PENDING_SCROLLS_CAP {
            self.pending_scrolls.push(op);
        } else {
            self.damage.mark_range(top, bottom + 1);
        }
    }

    /// A trailing directive whose band is owed whole moves nothing the
    /// replay does not restate. Only the tail can be judged here: an
    /// earlier directive's band was owed at a different offset.
    fn drop_replayed_scrolls(&mut self) {
        while let Some(tail) = self.pending_scrolls.last()
            && self.damage.all_dirty_in_range(
                usize::from(tail.region_top),
                usize::from(tail.region_bottom) + 1,
            )
        {
            self.pending_scrolls.pop();
        }
    }
}

/// A row a shadow reads back as default cells, whatever its storage
/// held.
fn row_is_blank(grid: &Grid, r: u16) -> bool {
    let view = grid.row_view(r);
    view.sized_cells.is_empty() && view.cells.iter().all(|c| *c == felis_grid::Cell::default())
}

fn blank_row_payload(grid: &Grid) -> Result<RowPayload, ConnError> {
    Ok(RowPayload(encode_row(
        RowEncode {
            cells: &[],
            pad_to: usize::from(grid.cols()),
            sized_cells: &[],
            soft_wrap_continued: false,
        },
        grid.style_table(),
    )?))
}

/// `pad_to` is the grid width: the view's cells are the
/// occupancy-clipped prefix, and the client expects a `cols`-wide row.
fn encode_row_view(grid: &Grid, view: &ViewportRowView<'_>) -> Result<Vec<u8>, ConnError> {
    Ok(encode_row(
        RowEncode {
            cells: view.cells,
            pad_to: usize::from(grid.cols()),
            sized_cells: &view.sized_cells,
            soft_wrap_continued: view.soft_wrap_continued,
        },
        grid.style_table(),
    )?)
}

/// Live row payloads encoded once per fan-out cycle and shared across
/// subscribers. Only live rows (viewport 0) are cached: a browsing
/// subscriber's composed row depends on its own viewport. With one
/// subscriber the cache stays disabled; clones would cost a copy per
/// row for no reuse.
#[derive(Default)]
pub(crate) struct RowEncodeCache {
    rows: Vec<Option<CachedRow>>,
    enabled: bool,
    /// The `(geometry_gen, scroll_seq)` the rows were encoded at: a row
    /// from an older grid would put a mirror one shift behind the
    /// sequence it is accounted at. A cycle composes under one core
    /// guard after `begin_cycle`, so this only catches a caller that
    /// composes across two guards without one.
    snapshot: Option<(u64, u64)>,
}

/// The handles are a property of the row, not the reader; only which
/// of them a subscriber has been sent differs.
#[derive(Clone)]
struct CachedRow {
    payload: RowPayload,
    handles: RowHandles,
}

impl RowEncodeCache {
    pub(crate) fn begin_cycle(&mut self, subscribers: usize) {
        self.rows.clear();
        self.snapshot = None;
        self.enabled = subscribers > 1;
    }

    /// Drop rows encoded from a grid this one has moved past.
    fn sync(&mut self, grid: &Grid) {
        let snapshot = (grid.geometry_gen(), grid.scroll_seq());
        if self.snapshot != Some(snapshot) {
            self.rows.clear();
            self.snapshot = Some(snapshot);
        }
    }

    /// Encode row `r`, appending the registry handles it names to
    /// `handles` when `collect`. A caching cycle collects the handles
    /// regardless: a second subscriber reading the cached row may be the
    /// one still owed those entries.
    fn encode(
        &mut self,
        grid: &Grid,
        r: u16,
        collect: bool,
        handles: &mut RowHandles,
    ) -> Result<RowPayload, ConnError> {
        if !self.enabled {
            let view = grid.row_view(r);
            if collect {
                handles.extend_from_cells(view.cells);
            }
            return Ok(RowPayload(encode_row_view(grid, &view)?));
        }
        let idx = usize::from(r);
        if idx >= self.rows.len() {
            self.rows.resize(idx + 1, None);
        }
        if let Some(hit) = &self.rows[idx] {
            if collect {
                handles.extend_from(&hit.handles);
            }
            return Ok(hit.payload.clone());
        }
        let view = grid.row_view(r);
        let mut row_handles = RowHandles::default();
        if grid.hyperlink_count() != 0 || grid.cluster_count() != 0 {
            row_handles.extend_from_cells(view.cells);
        }
        if collect {
            handles.extend_from(&row_handles);
        }
        let payload = RowPayload(encode_row_view(grid, &view)?);
        self.rows[idx] = Some(CachedRow {
            payload: payload.clone(),
            handles: row_handles,
        });
        Ok(payload)
    }
}

/// Registry entries the background drain ships per compose cycle, per
/// registry. The drain is convergence, not correctness (the composer
/// ships what each `RowDelta` needs as it goes); bounded so it cannot
/// crowd out the cycle's rows. A table at
/// `felis_grid::CLUSTER_TABLE_CAP` drains in a few hundred cycles.
const REGISTRY_DRIP_PER_CYCLE: usize = 256;

/// Emit unsent registry entries referenced by `handles`, marking each sent.
///
/// Called before pushing the corresponding `RowDelta`. Unresolvable handles are
/// skipped without marking sent so subsequent cycles can retry them.
fn ship_referenced(
    grid: &Grid,
    sent: &mut SentRegistries,
    handles: &RowHandles,
    out: &mut Vec<OutEvent>,
) {
    for id in &handles.links {
        let ordinal = usize::from(id.get());
        if sent.links.contains(ordinal) {
            continue;
        }
        let Some(entry) = grid.hyperlink(*id) else {
            continue;
        };
        out.push(OutEvent::Grid(GridMsg::Hyperlink {
            id: id.get(),
            anchor: entry.id.as_ref().map(|id| id.as_str().to_owned()),
            uri: entry.uri.as_str().to_owned(),
        }));
        sent.links.mark(ordinal);
    }
    for id in &handles.clusters {
        let ordinal = id.get() as usize;
        if sent.clusters.contains(ordinal) {
            continue;
        }
        let Some(text) = grid.cluster_str(*id) else {
            continue;
        };
        out.push(OutEvent::Grid(GridMsg::Cluster {
            id: id.get(),
            text: text.to_owned(),
        }));
        sent.clusters.mark(ordinal);
    }
}

/// Ship up to [`REGISTRY_DRIP_PER_CYCLE`] unreferenced entries of each
/// registry, lowest id first. Unlike [`ship_referenced`], an
/// unresolvable id is marked without being emitted: the mark advances
/// the drain, which a hole would otherwise stall forever.
fn drip_registry_tail(grid: &Grid, sent: &mut SentRegistries, out: &mut Vec<OutEvent>) {
    let links = grid.hyperlink_count();
    for _ in 0..REGISTRY_DRIP_PER_CYCLE {
        let Some(ordinal) = sent.links.next_unsent(links) else {
            break;
        };
        sent.links.mark(ordinal);
        let Some(id) = u16::try_from(ordinal).ok().and_then(NonZeroU16::new) else {
            break;
        };
        if let Some(entry) = grid.hyperlink(id) {
            out.push(OutEvent::Grid(GridMsg::Hyperlink {
                id: id.get(),
                anchor: entry.id.as_ref().map(|id| id.as_str().to_owned()),
                uri: entry.uri.as_str().to_owned(),
            }));
        }
    }
    let clusters = grid.cluster_count();
    for _ in 0..REGISTRY_DRIP_PER_CYCLE {
        let Some(ordinal) = sent.clusters.next_unsent(clusters) else {
            break;
        };
        sent.clusters.mark(ordinal);
        let Some(id) = u32::try_from(ordinal).ok().and_then(NonZeroU32::new) else {
            break;
        };
        if let Some(text) = grid.cluster_str(id) {
            out.push(OutEvent::Grid(GridMsg::Cluster {
                id: id.get(),
                text: text.to_owned(),
            }));
        }
    }
}

const fn mode_flags_msg(modes: felis_grid::ModeSnapshot) -> GridMsg {
    GridMsg::ModeFlags {
        bracketed_paste: modes.bracketed_paste,
        alt_screen: modes.alt_screen,
        mouse_protocol: modes.mouse_protocol,
        application_cursor: modes.application_cursor,
        modify_other_keys: modes.modify_other_keys,
        application_keypad: modes.application_keypad,
        win32_input_mode: modes.win32_input_mode,
        reverse_video: modes.reverse_video,
    }
}

/// Compose the attach burst for a scripted [`ConnectionMode::Ops`] connection.
///
/// Ships rehydrate bracket markers and keyboard modes; omits grid rows and images
/// because scripted verbs inspect state through `Region` and `Search` replies.
fn compose_ops_rehydrate(grid: &Grid, out: &mut Vec<OutEvent>) {
    out.push(OutEvent::Grid(GridMsg::RehydrateBegin));
    out.push(OutEvent::Grid(GridMsg::KittyKbdFlags {
        flags: grid.kitty_kbd_flags(),
    }));
    out.push(OutEvent::Grid(mode_flags_msg(grid.mode_snapshot())));
    out.push(OutEvent::Grid(GridMsg::RehydrateEnd));
}

/// What one daemon→client event body may carry before the writer
/// would refuse it (REQ-105). Half the frame ceiling: the protobuf
/// field framing around each row and the registry entries that ship
/// beside a batch both cost bytes a payload sum does not see, and
/// halving is cheaper than accounting for them.
pub(crate) const OUTBOUND_BODY_BUDGET_BYTES: usize =
    felis_protocol::frame::DEFAULT_MAX_BODY as usize / 2;

/// Emit `rows` as one `RowDelta`, or as several when their payloads
/// together would approach the frame ceiling (REQ-105). Rows are keyed
/// by index and applied independently, so a reader reaches the same
/// screen either way; the alternative is a frame the writer refuses,
/// which costs the subscriber its connection.
pub(crate) fn push_row_deltas(rows: Vec<(u16, RowPayload)>, out: &mut Vec<OutEvent>) {
    if rows.is_empty() {
        return;
    }
    let mut batch: Vec<(u16, RowPayload)> = Vec::new();
    let mut batch_bytes = 0usize;
    for (idx, payload) in rows {
        let len = payload.0.len();
        if !batch.is_empty() && batch_bytes + len > OUTBOUND_BODY_BUDGET_BYTES {
            out.push(OutEvent::Grid(GridMsg::RowDelta {
                rows: core::mem::take(&mut batch),
            }));
            batch_bytes = 0;
        }
        batch_bytes += len;
        batch.push((idx, payload));
    }
    out.push(OutEvent::Grid(GridMsg::RowDelta { rows: batch }));
}

/// Compose the full grid as a rehydration burst into `out`, recording
/// in `sent` which registry entries the burst carried.
pub(crate) fn compose_rehydrate(
    grid: &Grid,
    images: &felis_grid::images::ImageStore,
    placements: &felis_grid::images::Placements,
    sent: &mut SentRegistries,
    out: &mut Vec<OutEvent>,
) -> Result<(), ConnError> {
    out.push(OutEvent::Grid(GridMsg::RehydrateBegin));
    // Rehydrate is always at the live bottom (viewport 0). Rows are
    // encoded before anything is pushed so the handles they name are
    // known: only those entries ship ahead of the `RowDelta`, and the
    // rest of the table follows through [`drip_registry_tail`].
    let mut rows = Vec::with_capacity(usize::from(grid.rows()));
    let mut handles = RowHandles::default();
    let collect = !sent.caught_up(grid.hyperlink_count(), grid.cluster_count());
    for r in 0..grid.rows() {
        let view = grid.row_view(r);
        if collect {
            handles.extend_from_cells(view.cells);
        }
        rows.push((r, RowPayload(encode_row_view(grid, &view)?)));
    }
    ship_referenced(grid, sent, &handles, out);
    push_row_deltas(rows, out);
    let cur = grid.cursor();
    out.push(OutEvent::Grid(GridMsg::CursorState {
        row: cur.row,
        col: cur.col,
        visible: cur.visible,
        style: grid.cursor_style(),
        blink: grid.cursor_blink(),
    }));
    if let Some(title) = grid.title() {
        out.push(OutEvent::Grid(GridMsg::Title {
            value: title.to_owned(),
        }));
    }
    if let Some(cwd) = grid.cwd() {
        out.push(OutEvent::Grid(GridMsg::Cwd {
            value: cwd.to_owned(),
        }));
    }
    if let Some(shape) = grid.pointer_shape() {
        out.push(OutEvent::Grid(GridMsg::PointerShape {
            name: Some(shape.to_owned()),
        }));
    }
    for mark in grid.prompt_marks() {
        out.push(OutEvent::Grid(GridMsg::PromptMark {
            line: mark.line,
            kind: mark.kind,
            exit_code: mark.exit_code,
        }));
    }
    for channel in [
        ThemeChannel::Foreground,
        ThemeChannel::Background,
        ThemeChannel::Cursor,
    ] {
        if let Some(rgb) = grid.theme_override(channel) {
            out.push(OutEvent::Grid(GridMsg::ThemeColor {
                channel,
                action: ThemeAction::Set { rgb },
            }));
        }
    }
    // Nothing on the row wire carries a palette entry (a cell names an
    // index), so a client that missed these would paint the xterm
    // baseline.
    for (index, rgb) in grid.palette_overrides() {
        out.push(OutEvent::Grid(GridMsg::PaletteColor {
            index,
            action: PaletteAction::Set { rgb },
        }));
    }
    // The dirty flag stays: clearing it here would starve other
    // subscribers' broadcast, and a duplicate `KittyKbdFlags` next cycle
    // is idempotent.
    out.push(OutEvent::Grid(GridMsg::KittyKbdFlags {
        flags: grid.kitty_kbd_flags(),
    }));
    out.push(OutEvent::Grid(mode_flags_msg(grid.mode_snapshot())));
    // Baseline at viewport 0 so the shadow sets `viewport_max` before the
    // first wheel tick; otherwise the scrollbar thumb sizes against the
    // shadow's construction-time default until the user scrolls.
    out.push(OutEvent::Grid(GridMsg::ViewportState {
        lines_from_bottom: 0,
        max: viewport_max_for(grid),
    }));
    compose_image_rehydrate(images, placements, out);
    out.push(OutEvent::Grid(GridMsg::RehydrateEnd));
    Ok(())
}

/// Compose subscriber row diffs and cursor updates.
///
/// Broadcast facets are collected separately via [`collect_facets`].
/// Synchronized-output (`?2026`) holds diffs until ESU or timeout.
pub(crate) fn compose_diffs(
    grid: &mut Grid,
    sub: &mut SubscriberStream,
    rows_cache: &mut RowEncodeCache,
    out: &mut Vec<OutEvent>,
) -> Result<(), ConnError> {
    if !grid.ready_to_present(Instant::now()) {
        return Ok(());
    }
    let cur_modes = grid.mode_snapshot();
    if cur_modes != sub.diff.last_mode_flags {
        out.push(OutEvent::Grid(mode_flags_msg(cur_modes)));
        sub.diff.last_mode_flags = cur_modes;
    }
    // `prompt_marks_sent` is an absolute ordinal; the retained slice
    // begins at ordinal `pruned`. A mark pruned before this subscriber
    // caught up is unrecoverable, so clamp up rather than rewind into
    // evicted territory.
    let marks = grid.prompt_marks();
    let pruned = usize::try_from(grid.prompt_marks_pruned()).unwrap_or(usize::MAX);
    let end = pruned.saturating_add(marks.len());
    let mut sent = sub.diff.prompt_marks_sent.max(pruned);
    while sent < end {
        let mark = marks[sent - pruned];
        out.push(OutEvent::Grid(GridMsg::PromptMark {
            line: mark.line,
            kind: mark.kind,
            exit_code: mark.exit_code,
        }));
        sent += 1;
    }
    sub.diff.prompt_marks_sent = sent;
    // Everything below is the grid itself and goes only to a mirroring
    // subscriber: an `Ops` connection is owed no registry entries, so a
    // row shipped there could never be resolved.
    if !sub.mirrors_grid {
        sub.clear_scrolls();
        sub.damage.clear();
        return Ok(());
    }
    rows_cache.sync(grid);
    // The cells below are already shifted, so a directive drained later
    // would shift them again: replay the rows instead, at the sequence
    // this compose reads them under.
    sub.catch_up_to(grid);
    // Re-clamp every cycle: scrollback eviction or a `?1049h` can make
    // the honored offset stale, and felis-grid would compose against 0
    // while the shadow keeps showing the stale value.
    let effective_viewport = grid.clamp_viewport(sub.diff.viewport);
    if effective_viewport != sub.diff.viewport {
        sub.diff.viewport = effective_viewport;
    }
    let viewport_changed = effective_viewport != sub.diff.last_viewport_state.0;
    let cur_viewport_state = (effective_viewport, viewport_max_for(grid));
    if cur_viewport_state != sub.diff.last_viewport_state {
        out.push(OutEvent::Grid(GridMsg::ViewportState {
            lines_from_bottom: cur_viewport_state.0,
            max: cur_viewport_state.1,
        }));
        sub.diff.last_viewport_state = cur_viewport_state;
    }
    if effective_viewport == 0 {
        // docs/reference/ipc.md: accepted scroll directives ship first, in
        // arrival order, so the shadow's shift reproduces the daemon's state
        // before the `RowDelta` replay patches the changed rows.
        sub.drop_replayed_scrolls();
        let shifted = !sub.pending_scrolls.is_empty();
        for op in sub.pending_scrolls.drain(..) {
            out.push(OutEvent::Grid(GridMsg::Scrolled {
                region_top: op.region_top,
                region_bottom: op.region_bottom,
                n_rows: op.n_rows,
                direction: op.direction,
            }));
        }
        let vacated = std::mem::replace(&mut sub.vacated, Damage::new(sub.damage.len()));
        let mut blank_row: Option<RowPayload> = None;
        let dirty: Vec<_> = sub.damage.dirty_rows().collect();
        // A replay of every row leaves this mirror equal to the grid as
        // read under the core lock, so a directive at or below that
        // sequence is already in the rows it holds. A partial replay
        // leaves the shift to the directive.
        if dirty.len() == usize::from(grid.rows()) {
            sub.composed_through_scroll = grid.scroll_seq();
        }
        // docs/reference/ipc.md: one `RowDelta` per cycle carries every
        // dirty row.
        let mut rows: Vec<(u16, RowPayload)> = Vec::with_capacity(dirty.len());
        let mut handles = RowHandles::default();
        let collect = sub.owes_registry_entries(grid);
        for row_idx in dirty {
            let Ok(r) = u16::try_from(row_idx) else {
                continue;
            };
            if shifted && vacated.is_dirty(row_idx) && !row_is_blank(grid, r) {
                let blank = match &blank_row {
                    Some(blank) => blank.clone(),
                    None => blank_row.insert(blank_row_payload(grid)?).clone(),
                };
                rows.push((r, blank));
            }
            rows.push((r, rows_cache.encode(grid, r, collect, &mut handles)?));
        }
        if !rows.is_empty() {
            ship_referenced(grid, &mut sub.sent, &handles, out);
            push_row_deltas(rows, out);
        }
        sub.damage.clear();
        let cur = grid.cursor();
        let style = grid.cursor_style();
        let blink = grid.cursor_blink();
        if cur != sub.diff.last_cursor
            || style != sub.diff.last_cursor_style
            || blink != sub.diff.last_cursor_blink
        {
            out.push(OutEvent::Grid(GridMsg::CursorState {
                row: cur.row,
                col: cur.col,
                visible: cur.visible,
                style,
                blink,
            }));
            sub.diff.last_cursor = cur;
            sub.diff.last_cursor_style = style;
            sub.diff.last_cursor_blink = blink;
        }
    } else if viewport_changed {
        // The shadow has no scrollback, so the daemon composes the browsed
        // view. A scroll directive must never reach a browsing shadow (it
        // would shift the composed view); accepted ones fold back into the
        // damage so the snap-back replay covers them.
        if !sub.pending_scrolls.is_empty() {
            sub.clear_scrolls();
            sub.damage.mark_all();
        }
        // The composed rows come out of scrollback and name ids a
        // visible-first attach never sent; this arm is why a tail cursor
        // cannot stand in for the sent-set.
        let mut rows = Vec::with_capacity(usize::from(grid.rows()));
        let mut handles = RowHandles::default();
        let collect = sub.owes_registry_entries(grid);
        for r in 0..grid.rows() {
            let Some(view) = grid.viewport_row(effective_viewport, r) else {
                continue;
            };
            if collect {
                handles.extend_from_cells(view.cells);
            }
            rows.push((r, RowPayload(encode_row_view(grid, &view)?)));
        }
        if !rows.is_empty() {
            ship_referenced(grid, &mut sub.sent, &handles, out);
            push_row_deltas(rows, out);
        }
        // Hide the cursor while browsing; the sentinel in `last_cursor` makes
        // the next snap-back re-emit the live one.
        let hidden = felis_grid::Cursor {
            row: 0,
            col: 0,
            visible: false,
            pending_wrap: false,
        };
        if sub.diff.last_cursor != hidden {
            out.push(OutEvent::Grid(GridMsg::CursorState {
                row: 0,
                col: 0,
                visible: false,
                style: sub.diff.last_cursor_style,
                blink: sub.diff.last_cursor_blink,
            }));
            sub.diff.last_cursor = hidden;
        }
    } else {
        // Steady browse: the user asked the view to freeze; damage stays for
        // the snap-back flush.
    }
    // Last, so state nothing on screen needs never delays the frame, and
    // only onto a cycle that already carries something: a frame the drain
    // alone produced would answer the pull and request a repaint, walking
    // an idle attach through the whole table at vsync.
    if !out.is_empty() {
        drip_registry_tail(grid, &mut sub.sent, out);
    }
    Ok(())
}

/// Take single-consumer facets once for broadcast across all subscribers.
///
/// Because grid `take_*` methods clear on read, callers must gate on
/// `ready_to_present` and active subscribers so unobserved facets stay armed.
pub(crate) fn collect_facets(grid: &mut Grid) -> Vec<GridMsg> {
    let mut facets: Vec<GridMsg> = Vec::new();
    if let Some(value) = grid.take_title_dirty() {
        facets.push(GridMsg::Title { value });
    }
    if let Some(value) = grid.take_cwd_dirty() {
        facets.push(GridMsg::Cwd { value });
    }
    if let Some(name) = grid.take_pointer_shape_dirty() {
        facets.push(GridMsg::PointerShape { name });
    }
    for channel in [
        ThemeChannel::Foreground,
        ThemeChannel::Background,
        ThemeChannel::Cursor,
    ] {
        if let Some(rgb) = grid.take_theme_dirty(channel) {
            let action = rgb.map_or(ThemeAction::Reset, |rgb| ThemeAction::Set { rgb });
            facets.push(GridMsg::ThemeColor { channel, action });
        }
    }
    // The whole-table reset leads, so a cycle that reset and then set an
    // index does not arrive reversed.
    let palette = grid.take_palette_dirty();
    if palette.reset_all {
        facets.push(GridMsg::PaletteResetAll);
    }
    for entry in palette.entries {
        facets.push(GridMsg::PaletteColor {
            index: entry.index,
            action: entry
                .rgb
                .map_or(PaletteAction::Reset, |rgb| PaletteAction::Set { rgb }),
        });
    }
    if let Some(flags) = grid.take_kitty_kbd_dirty() {
        facets.push(GridMsg::KittyKbdFlags { flags });
    }
    // OSC 52 is absent: a clipboard write has per-client scope
    // (security-model.md "Daemon IPC"), so the session task routes it to
    // the initiating subscriber. BEL is coalesced to
    // one flag per cycle.
    if grid.take_bell_pending() {
        facets.push(GridMsg::Attention {
            source: AttentionSource::Bell,
        });
    }
    facets
}

pub(crate) struct TakenNotifications {
    notifs: Vec<felis_vt::notification::Notification>,
    session_title: Option<String>,
    cwd: Option<String>,
}

pub(crate) fn take_notifications(grid: &mut Grid) -> Option<TakenNotifications> {
    let notifs = grid.take_notifications();
    if notifs.is_empty() {
        return None;
    }
    Some(TakenNotifications {
        notifs,
        session_title: grid.title().map(str::to_owned),
        cwd: grid.cwd().map(str::to_owned),
    })
}

/// felis relays notifications without popping popups; zero subscribers is normal.
pub(crate) fn publish_notifications(
    hub: &tokio::sync::broadcast::Sender<NotifyToClientMsg>,
    id: SessionId,
    taken: TakenNotifications,
    attached: bool,
) -> Option<crate::pool::StoredNotification> {
    let TakenNotifications {
        notifs,
        session_title,
        cwd,
    } = taken;
    let mut latest = None;
    for n in notifs {
        latest = Some(crate::pool::StoredNotification {
            title: n.title.clone(),
            body: n.body.clone(),
            urgency: n.urgency,
            at: Instant::now(),
        });
        // `.is_ok()` rather than `let _ =`: `let_underscore_drop` fires on
        // the `SendError`.
        let _published = hub
            .send(NotifyToClientMsg::Event {
                session_id: id.0,
                notification: Notification {
                    title: n.title,
                    body: n.body,
                    urgency: n.urgency,
                },
                notify_id: n.id,
                session_title: session_title.clone(),
                cwd: cwd.clone(),
                attached,
            })
            .is_ok();
    }
    latest
}

/// The fan-out body, authorized where it is encoded: the pump that
/// writes it never sees the domain message.
///
/// # Errors
/// [`TransportError::Wire`] for a message past a per-operation limit.
pub(crate) fn body_for(ev: &OutEvent) -> Result<CheckedFrame, TransportError> {
    match ev {
        OutEvent::Grid(msg) => CheckedFrame::encode(msg),
        OutEvent::Image(msg) => CheckedFrame::encode(msg),
        OutEvent::Push(msg) => CheckedFrame::encode(msg),
        OutEvent::Region { msg, correlation } => CheckedFrame::encode_correlated(msg, *correlation),
        OutEvent::Session { msg, correlation } => {
            CheckedFrame::encode_correlated(msg, *correlation)
        }
        OutEvent::Search { msg, correlation } => CheckedFrame::encode_correlated(msg, *correlation),
        OutEvent::Ops { msg, correlation } => {
            CheckedFrame::encode_correlated(msg.as_ref(), *correlation)
        }
        OutEvent::Control(msg) => CheckedFrame::encode(msg),
    }
}

/// Each image ships as `Header` → `Chunk`s → `Complete`; placements
/// (anchored, then virtual) follow all images so the client's store
/// holds an id before a placement references it.
pub(crate) fn compose_image_rehydrate(
    images: &felis_grid::images::ImageStore,
    placements: &felis_grid::images::Placements,
    out: &mut Vec<OutEvent>,
) {
    for id in images.iter_ids() {
        let mut msgs = Vec::new();
        crate::graphics::image_sync_messages(images, id, &mut msgs);
        out.extend(msgs.into_iter().map(OutEvent::Image));
    }
    for p in placements.iter() {
        out.push(OutEvent::Image(ImageMsg::Placement {
            image_id: p.image_id,
            placement_id: p.placement_id,
            anchor_row: p.anchor.row,
            anchor_col: p.anchor.col,
            cols: p.cols,
            rows: p.rows,
            source: p.source,
            z_index: p.z_index,
        }));
    }
    // Virtual (`U=1`) extents too: placeholder cells ride the `RowDelta`
    // path but name only an image id, and without the extent the client
    // cannot size the tiles. `free_image` keeps this table synced to the
    // store, so every entry's image shipped above.
    for v in placements.iter_virtual() {
        out.push(OutEvent::Image(ImageMsg::VirtualPlacement {
            image_id: v.image_id,
            cols: v.cols,
            rows: v.rows,
            z_index: v.z_index,
        }));
    }
}

/// Write an unreserved actor-generated reply to the PTY.
///
/// Drops writes under saturation so a child stalled on reading stdin cannot park
/// the actor serving other subscribers. Admitted input does not count toward this cap.
pub(crate) fn write_pty(writer: &felis_pty::PtyWriter, bytes: &[u8]) -> Result<(), ConnError> {
    let outcome = writer
        .write_owned(bytes.to_vec(), None)
        .map_err(ConnError::PtyIo)?;
    if matches!(outcome, felis_pty::WriteOutcome::Dropped { report: true }) {
        // A program waiting on the answer hangs, and nothing else
        // records that it was this drop rather than the program. Once
        // per saturation episode: the child sets the query rate, so a
        // line per drop is a log a wedged program can grow at will.
        tracing::warn!(
            bytes = bytes.len(),
            "dropped a generated reply: the child is not draining its stdin              (further drops are silent until it does)"
        );
    }
    Ok(())
}

/// Inject `bytes` as a paste, wrapped in `ESC [ 200~ … ESC [ 201~`
/// when the program enabled `?2004`. `InputMsg::Paste` is the one verb
/// that lands here, so the bracketing rule has a single implementation.
/// `reservation` is the connection's admission for these bytes, held
/// until the OS writer takes them.
pub(crate) fn write_paste(
    writer: &felis_pty::PtyWriter,
    bracketed: bool,
    bytes: Vec<u8>,
    reservation: Option<felis_pty::WriteReservation>,
) -> Result<(), ConnError> {
    let payload = if bracketed {
        let mut framed =
            Vec::with_capacity(bytes.len() + felis_protocol::limits::PASTE_BRACKET_OVERHEAD);
        framed.extend_from_slice(b"\x1b[200~");
        framed.extend_from_slice(&bytes);
        framed.extend_from_slice(b"\x1b[201~");
        framed
    } else {
        bytes
    };
    write_pty_reserved(writer, payload, reservation)
}

/// Admitted input: the connection acquired the bytes against the
/// session's budget before handing them over, so this always enqueues
/// and never waits.
pub(crate) fn write_pty_reserved(
    writer: &felis_pty::PtyWriter,
    bytes: Vec<u8>,
    reservation: Option<felis_pty::WriteReservation>,
) -> Result<(), ConnError> {
    let _outcome = writer
        .write_owned(bytes, reservation)
        .map_err(ConnError::PtyIo)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ordinary cycle is still exactly one frame, and a pathological
    /// one splits rather than building a body the writer would refuse
    /// (REQ-105). Every row survives the split, in order.
    #[test]
    fn a_row_batch_past_the_budget_splits_across_frames() {
        let row = |idx: u16, len: usize| (idx, RowPayload(vec![0u8; len]));

        let mut out = Vec::new();
        push_row_deltas(vec![row(0, 64), row(1, 64)], &mut out);
        assert_eq!(out.len(), 1, "an ordinary cycle stays one frame");

        let chunk = OUTBOUND_BODY_BUDGET_BYTES / 2 + 1;
        let mut out = Vec::new();
        push_row_deltas((0..4).map(|i| row(i, chunk)).collect(), &mut out);
        let batches: Vec<Vec<u16>> = out
            .iter()
            .map(|ev| match ev {
                OutEvent::Grid(GridMsg::RowDelta { rows }) => {
                    rows.iter().map(|(idx, _)| *idx).collect()
                }
                other => panic!("expected a RowDelta, got {other:?}"),
            })
            .collect();
        assert_eq!(batches, vec![vec![0], vec![1], vec![2], vec![3]]);
    }

    /// Placeholder cells carry only an image id, so the virtual extent
    /// must replay, after the image bytes it names.
    #[test]
    fn image_rehydrate_replays_virtual_extents_after_the_image_bytes() {
        use felis_grid::images::{ImageEntry, ImageStore, Placements, VirtualPlacement};
        use felis_protocol::ImageId;
        use felis_protocol::messages::ImageFormat;
        let mut images = ImageStore::new(1024);
        images
            .insert(
                ImageId(12),
                ImageEntry::new(1, 1, ImageFormat::Rgba32, vec![0xAA, 0xBB, 0xCC, 0xDD]),
            )
            .unwrap();
        let mut placements = Placements::new();
        placements.upsert_virtual(VirtualPlacement {
            image_id: ImageId(12),
            cols: 4,
            rows: 2,
            z_index: 0,
        });
        let mut out = Vec::new();
        compose_image_rehydrate(&images, &placements, &mut out);
        let virtual_at = out
            .iter()
            .position(|e| matches!(e, OutEvent::Image(ImageMsg::VirtualPlacement { .. })))
            .expect("rehydrate must replay the virtual extent");
        let complete_at = out
            .iter()
            .position(|e| matches!(e, OutEvent::Image(ImageMsg::Complete { .. })))
            .expect("rehydrate must ship the image bytes");
        assert!(
            complete_at < virtual_at,
            "the extent references the image, so the bytes ship first"
        );
        let OutEvent::Image(ImageMsg::VirtualPlacement {
            image_id,
            cols,
            rows,
            z_index,
        }) = &out[virtual_at]
        else {
            unreachable!()
        };
        assert_eq!(
            (image_id.0, *cols, *rows, *z_index),
            (12, 4, 2, 0),
            "the replay restates the recorded extent verbatim"
        );
    }

    #[test]
    fn a_row_payload_reaches_the_wire_unrewritten() {
        let msg = GridMsg::RowDelta {
            rows: vec![(1, RowPayload(vec![1, 2, 3, 4]))],
        };
        let direct = felis_protocol::codec::encode(&msg);
        let frame = body_for(&OutEvent::Grid(msg)).expect("a row delta is sendable");
        assert_eq!(frame.kind(), felis_protocol::MessageKind::Grid.as_u16());
        assert_eq!(frame.body(), direct);
    }
}

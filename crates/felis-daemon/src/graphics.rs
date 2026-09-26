//! Kitty graphics dispatcher. Per
//! docs/explanation/protocols/kitty-graphics.md "Dispatcher architecture"
//! it runs on the daemon, not the grid: `Grid::apc_dispatch` buffers
//! raw APC bodies into its outbox, and the serve loop drains the outbox
//! after every `Parser::advance` and feeds each body through here.

use std::io::Cursor;
use std::num::NonZeroU32;

use bytes::Bytes;
use felis_grid::images::{
    CellPos, Evicted, Frame, FrameError, ImageEntry, ImageId, InsertError, Placement, PlacementId,
};
use felis_grid::{ApcBody, ScreenSwitch};
use felis_protocol::messages::{ImageFormat, ImageMsg, ImageTarget, MAX_IMAGE_CHUNK_PAYLOAD};
use felis_vt::kitty_graphics::{
    self, CompleteCommand, ErrorCode, Outcome, Reassembler, ResponseRefs, Status, base64,
    format_response, inflate as zlib,
};
// Handed to `image_decode` for the Unix `t=f` / `t=t` secure-open path;
// Windows declines those methods outright.
#[cfg(unix)]
use rustix::fs::{Mode, OFlags};

use crate::pool::Session;

mod compose;
mod image_decode;
pub use image_decode::{
    DecodeError, MAX_DECODED_BYTES, ShmDeferral, decode_image, shm_segment_name, unlink_shm_segment,
};

/// One queued change to a session's image state.
///
/// Transmissions are markers read back from the store at ship time
/// ([`materialize_image_events`]) rather than carrying pixel buffers,
/// preventing unbounded memory growth under fast producers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageEvent {
    /// The image's whole current content (root frame, every animation
    /// frame, displayed index), not the bytes that just arrived, so
    /// dropping an earlier `Transmit` for the same id is always safe;
    /// that is what lets the ship step collapse mpv's one-per-video-frame
    /// burst into one materialization.
    Transmit(ImageId),
    TransmitFrame {
        id: ImageId,
        index: u32,
    },
    Placement {
        image_id: ImageId,
        placement_id: Option<PlacementId>,
        anchor_row: i32,
        anchor_col: u16,
        cols: u16,
        rows: u16,
        source: Option<felis_protocol::messages::SourceRect>,
        z_index: i32,
    },
    PlacementRemoved {
        image_id: ImageId,
        placement_id: Option<PlacementId>,
    },
    VirtualPlacement {
        image_id: ImageId,
        cols: u16,
        rows: u16,
        z_index: i32,
    },
    PlacementsShifted {
        lines: u32,
    },
    Delete {
        id: ImageId,
    },
    ShowFrame {
        id: ImageId,
        index: u32,
    },
}

impl ImageEvent {
    /// Returns the image id if a later event supersedes earlier queued markers.
    ///
    /// `Transmit` and `Delete` restate or remove the image; delta events such
    /// as `PlacementsShifted` must survive verbatim.
    pub(crate) const fn restates(&self) -> Option<ImageId> {
        match *self {
            Self::Transmit(id) | Self::Delete { id } => Some(id),
            _ => None,
        }
    }
}

impl std::fmt::Display for ImageEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transmit(id) => write!(f, "Transmit(i={id})"),
            Self::TransmitFrame { id, index } => write!(f, "TransmitFrame(i={id},f={index})"),
            Self::Placement { image_id, .. } => write!(f, "Placement(i={image_id})"),
            Self::PlacementRemoved { image_id, .. } => {
                write!(f, "PlacementRemoved(i={image_id})")
            }
            Self::VirtualPlacement {
                image_id,
                cols,
                rows,
                ..
            } => write!(f, "VirtualPlacement(i={image_id},c={cols},r={rows})"),
            Self::Delete { id } => write!(f, "Delete(i={id})"),
            Self::ShowFrame { id, index } => write!(f, "ShowFrame(i={id},f={index})"),
            Self::PlacementsShifted { lines } => write!(f, "PlacementsShifted(n={lines})"),
        }
    }
}

/// Each chunk is a refcounted view of the store's buffer: a rehydrate
/// re-ships a whole store, so copying would duplicate up to its cap.
fn chunked(pixels: &Bytes) -> impl Iterator<Item = Bytes> + '_ {
    (0..pixels.len())
        .step_by(MAX_IMAGE_CHUNK_PAYLOAD)
        .map(|start| {
            let end = (start + MAX_IMAGE_CHUNK_PAYLOAD).min(pixels.len());
            pixels.slice(start..end)
        })
}

/// The store indexes frames from 0, the wire numbers them from 1
/// (Kitty's own numbering). Saturating rather than panicking: the store
/// caps frame counts far below `u32::MAX`, so the clamp is unreachable
/// and only exists to keep the conversion total.
pub(crate) fn frame_number(index: usize) -> NonZeroU32 {
    let raw = u32::try_from(index)
        .unwrap_or(u32::MAX - 1)
        .saturating_add(1);
    NonZeroU32::new(raw).unwrap_or(NonZeroU32::MIN)
}

/// Shared by the attach-time rehydrate and [`materialize_image_events`].
///
/// Ensures a live transmission and a reattach produce identical state.
/// A missing id is skipped if evicted before the marker is drained.
pub fn image_sync_messages(
    images: &felis_grid::images::ImageStore,
    id: ImageId,
    out: &mut Vec<ImageMsg>,
) {
    let Some(entry) = images.get(id) else {
        return;
    };
    let frames = entry.frames();
    out.push(ImageMsg::Header {
        id,
        target: ImageTarget::New {
            width: entry.width,
            height: entry.height,
            format: entry.format,
        },
    });
    for chunk in chunked(&frames[0].pixels) {
        out.push(ImageMsg::Chunk { id, bytes: chunk });
    }
    out.push(ImageMsg::Complete { id });
    // Animation frames, so a reattaching client resumes the animation
    // (docs/reference/protocols/kitty-graphics.md "Animation").
    for idx in 1..frames.len() {
        frame_sync_messages(images, id, idx as u32, out);
    }
    // The daemon kept advancing while detached; the client displays the
    // root by default.
    if entry.current_frame() != 0 {
        out.push(ImageMsg::ShowFrame {
            id,
            number: frame_number(entry.current_frame()),
        });
    }
}

/// Skips a frame index missing from the store: a `d=f` earlier in the
/// same drain renumbers surviving frames, so a queued marker can name a gap.
pub fn frame_sync_messages(
    images: &felis_grid::images::ImageStore,
    id: ImageId,
    index: u32,
    out: &mut Vec<ImageMsg>,
) {
    let Some(entry) = images.get(id) else {
        return;
    };
    let Some(frame) = entry.frame(index as usize) else {
        return;
    };
    out.push(ImageMsg::Header {
        id,
        target: ImageTarget::Frame {
            number: frame_number(index as usize),
        },
    });
    for chunk in chunked(&frame.pixels) {
        out.push(ImageMsg::Chunk { id, bytes: chunk });
    }
    out.push(ImageMsg::Complete { id });
}

/// Materializes queued image events into wire messages.
///
/// Superseded transmission markers collapse to the last event per id,
/// while controls preserve ordering (see [`ImageEvent::restates`]).
#[must_use]
pub fn materialize_image_events(
    events: &[ImageEvent],
    images: &felis_grid::images::ImageStore,
) -> Vec<ImageMsg> {
    let mut last_restate: std::collections::HashMap<ImageId, usize> =
        std::collections::HashMap::new();
    for (i, event) in events.iter().enumerate() {
        if let Some(id) = event.restates() {
            last_restate.insert(id, i);
        }
    }
    let mut framed: std::collections::HashSet<(ImageId, u32)> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(events.len());
    for (i, event) in events.iter().enumerate() {
        match *event {
            ImageEvent::Transmit(id) => {
                if last_restate.get(&id) == Some(&i) {
                    image_sync_messages(images, id, &mut out);
                }
            }
            ImageEvent::TransmitFrame { id, index } => {
                // Kept at the first position for its index: the mirror appends
                // sequentially and rejects out-of-order frame numbers, while
                // every marker reads current pixels from the store at fan-out.
                let superseded = last_restate.get(&id).is_some_and(|&p| p > i);
                if !superseded && framed.insert((id, index)) {
                    frame_sync_messages(images, id, index, &mut out);
                }
            }
            ImageEvent::Placement {
                image_id,
                placement_id,
                anchor_row,
                anchor_col,
                cols,
                rows,
                source,
                z_index,
            } => out.push(ImageMsg::Placement {
                image_id,
                placement_id,
                anchor_row,
                anchor_col,
                cols,
                rows,
                source,
                z_index,
            }),
            ImageEvent::PlacementRemoved {
                image_id,
                placement_id,
            } => out.push(ImageMsg::PlacementRemoved {
                image_id,
                placement_id,
            }),
            ImageEvent::VirtualPlacement {
                image_id,
                cols,
                rows,
                z_index,
            } => out.push(ImageMsg::VirtualPlacement {
                image_id,
                cols,
                rows,
                z_index,
            }),
            ImageEvent::PlacementsShifted { lines } => {
                out.push(ImageMsg::PlacementsShifted { lines });
            }
            ImageEvent::Delete { id } => out.push(ImageMsg::Delete { id }),
            ImageEvent::ShowFrame { id, index } => out.push(ImageMsg::ShowFrame {
                id,
                number: frame_number(index as usize),
            }),
        }
    }
    out
}

/// Default per-frame gap when `a=f` omits `z=` or sends `z=0`, matching
/// kitty's `DEFAULT_GAP` (`kitty/graphics.c`). `z<0` is gapless (0).
const DEFAULT_GAP_MS: u32 = 40;

/// One epoch for the whole daemon so a frame's `shown_at` stays
/// comparable across attach / detach / reattach: the attached select
/// loop and the detached drainer advance the same `ImageStore` against
/// the same clock (docs/reference/protocols/kitty-graphics.md
/// "Animation").
static ANIM_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

#[must_use]
pub fn anim_now_ms() -> u64 {
    let epoch = ANIM_EPOCH.get_or_init(std::time::Instant::now);
    u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// `z>0` is the gap, `z<0` gapless (`0`, skipped in playback),
/// `z=0`/absent the default.
const fn gap_from_z(z: Option<i32>) -> u32 {
    match z {
        Some(v) if v > 0 => v as u32,
        Some(v) if v < 0 => 0,
        _ => DEFAULT_GAP_MS,
    }
}

fn control_value(complete: &CompleteCommand, key: u8) -> Option<&[u8]> {
    complete
        .controls
        .iter()
        .rev()
        .find_map(|(k, v)| (*k == key).then_some(v.as_slice()))
}

fn control_byte(complete: &CompleteCommand, key: u8) -> Option<u8> {
    let v = control_value(complete, key)?;
    (v.len() == 1).then(|| v[0])
}

fn control_u32(complete: &CompleteCommand, key: u8) -> Option<u32> {
    let v = control_value(complete, key)?;
    std::str::from_utf8(v).ok()?.parse::<u32>().ok()
}

/// The geometry keys (`x=`/`y=`/`c=`/`r=`) live in cell space, so a
/// value past `u16::MAX` is as malformed as a non-numeric one.
fn control_u16(complete: &CompleteCommand, key: u8) -> Option<u16> {
    control_u32(complete, key).and_then(|v| u16::try_from(v).ok())
}

fn control_i32(complete: &CompleteCommand, key: u8) -> Option<i32> {
    let v = control_value(complete, key)?;
    std::str::from_utf8(v).ok()?.parse::<i32>().ok()
}

pub struct ApcCtx<'a> {
    pub grid: &'a mut felis_grid::Grid,
    pub images: &'a mut felis_grid::images::ImageStore,
    pub placements: &'a mut felis_grid::images::Placements,
    pub events: &'a mut Vec<ImageEvent>,
    /// `t=s` segment names to unlink at session teardown (see
    /// [`ShmDeferral`]).
    pub shm: &'a mut ShmDeferral,
    /// `0` selects `resolve_natural_cells`' single-cell fallback.
    pub cell_pixel_w: u16,
    pub cell_pixel_h: u16,
    /// Where `a=T` anchors, 0-based `(row, col)`. Production pins the
    /// cursor captured when the body arrived (see [`ApcBody`]) so a
    /// trailing DECRC in the same burst cannot move the anchor; `None`
    /// anchors at the grid's live cursor.
    pub anchor_cursor: Option<(u16, u16)>,
}

pub fn dispatch_apc_body(
    ctx: &mut ApcCtx<'_>,
    reassembler: &mut Reassembler,
    body: &[u8],
) -> Option<Vec<u8>> {
    let complete = match reassemble_apc_body(reassembler, body) {
        BodyDispatch::Assembled(complete) => complete,
        BodyDispatch::Reply(reply) => return Some(reply),
        BodyDispatch::Silent => return None,
    };
    handle_complete(ctx, &complete)
}

#[derive(Debug, PartialEq, Eq)]
pub enum BodyDispatch {
    Assembled(CompleteCommand),
    Reply(Vec<u8>),
    /// A mid-stream chunk, a non-`G` body (another APC dialect), or an
    /// error whose reply the producer's `q=` suppressed.
    Silent,
}

pub fn reassemble_apc_body(reassembler: &mut Reassembler, body: &[u8]) -> BodyDispatch {
    let Some(cmd) = kitty_graphics::parse(body) else {
        // Non-`G` bodies are other APC dialects; a Kitty graphics error
        // would inject bytes into a protocol we do not speak.
        if !body.starts_with(b"G") {
            return BodyDispatch::Silent;
        }
        // `EINVAL` so a producer's read-the-response loop terminates
        // instead of hanging on a dropped command. The controls are
        // unparseable, so no `q=` or ids can be honored: the reply is
        // unconditional and id-less.
        return BodyDispatch::Reply(format_response(
            &ResponseRefs::default(),
            Status::Error {
                code: ErrorCode::InvalidValue,
                message: "malformed graphics escape",
            },
        ));
    };
    match reassembler.feed(&cmd) {
        Outcome::Pending => BodyDispatch::Silent,
        Outcome::Done(complete) => BodyDispatch::Assembled(complete),
        Outcome::Overflow { controls } => {
            // Cap per security-model.md "Kitty graphics"; `feed` already
            // reset state. Echo the head chunk's ids and honor its `q=`.
            let head = CompleteCommand {
                controls,
                payload: Vec::new(),
            };
            let refs = response_refs(&head);
            let quiet = quiet_level(&head);
            let outcome = ActionOutcome::error(
                ErrorCode::InvalidValue,
                "payload exceeds the reassembly buffer limit; retry with smaller transmissions",
            );
            match apply_quiet_mode(quiet, &refs, outcome) {
                Some(reply) => BodyDispatch::Reply(reply),
                None => BodyDispatch::Silent,
            }
        }
    }
}

/// Drain grid APC outbox and return responses to write via PTY master.
///
/// Dispatcher runs synchronously; responses queue to `write_pty` alongside
/// query responses without blocking async IO on `&mut Session`.
pub fn dispatch_apc_bodies(
    session: &mut Session,
    grid: &mut felis_grid::Grid,
    bodies: Vec<ApcBody>,
) -> Vec<Vec<u8>> {
    let t_start = std::time::Instant::now();
    let n_bodies = bodies.len();
    let bytes_in: usize = bodies.iter().map(|b| b.body.len()).sum();
    let events_before = session.image_events.len();
    let cell_pixel_w = session.cell_pixel_w;
    let cell_pixel_h = session.cell_pixel_h;
    let mut responses = Vec::new();
    for apc in bodies {
        // The cursor captured when this body arrived, not the live one
        // (see `ApcBody`); the reassembler completes only on the
        // finishing body, and producers don't move the cursor
        // mid-transmit.
        let mut ctx = ApcCtx {
            grid,
            images: &mut session.images,
            placements: &mut session.placements,
            events: &mut session.image_events,
            shm: &mut session.shm_segments,
            cell_pixel_w,
            cell_pixel_h,
            anchor_cursor: Some((apc.cursor_row, apc.cursor_col)),
        };
        let response = dispatch_apc_body(&mut ctx, &mut session.graphics_reassembler, &apc.body);
        if let Some(response) = response {
            responses.push(response);
        }
    }
    let events_emitted = session.image_events.len() - events_before;
    let elapsed_ms = t_start.elapsed().as_millis();
    if elapsed_ms > 5 || events_emitted > 0 {
        let kinds: Vec<String> = session.image_events[events_before..]
            .iter()
            .map(ToString::to_string)
            .collect();
        tracing::info!(
            n_bodies,
            bytes_in,
            events_emitted,
            elapsed_ms,
            events = ?kinds,
            "dispatch_apc_bodies"
        );
    }
    responses
}

/// Apply one alt-screen toggle to per-screen placement contexts.
///
/// Releases refcounts on stashed placements and restores them on return,
/// keeping `refcount == live_placement_count` invariant across toggles.
/// Idempotent toggles are filtered upstream by the grid.
pub fn apply_screen_switch(
    switch: ScreenSwitch,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    saved_primary_placements: &mut Option<felis_grid::images::Placements>,
    events: &mut Vec<ImageEvent>,
) {
    tracing::trace!(
        target: "felis_daemon::graphics",
        ?switch,
        saved_primary = saved_primary_placements.is_some(),
        live_placements = placements.len(),
        "apply_screen_switch",
    );
    match switch {
        ScreenSwitch::EnteredAlternate => {
            let snapshot = std::mem::take(placements);
            for p in snapshot.iter() {
                images.release(p.image_id);
                events.push(ImageEvent::PlacementRemoved {
                    image_id: p.image_id,
                    placement_id: p.placement_id,
                });
            }
            // A leftover save means a missed `LeftAlternate` drain;
            // overwrite rather than panic.
            *saved_primary_placements = Some(snapshot);
        }
        ScreenSwitch::LeftAlternate => {
            let alt = std::mem::take(placements);
            for p in alt.iter() {
                images.release(p.image_id);
                events.push(ImageEvent::PlacementRemoved {
                    image_id: p.image_id,
                    placement_id: p.placement_id,
                });
            }
            if let Some(saved) = saved_primary_placements.take() {
                *placements = saved;
                for p in placements.iter() {
                    images.retain(p.image_id);
                    events.push(ImageEvent::Placement {
                        image_id: p.image_id,
                        placement_id: p.placement_id,
                        anchor_row: p.anchor.row,
                        anchor_col: p.anchor.col,
                        cols: p.cols,
                        rows: p.rows,
                        source: p.source,
                        z_index: p.z_index,
                    });
                }
                // `free_image` purges only the live table, so an LRU
                // eviction during the alt-screen stay can leave a stashed
                // extent pointing at a freed image; this is the one seam
                // where stale entries re-enter.
                placements.retain_virtual(|v| images.get(v.image_id).is_some());
                for v in placements.iter_virtual() {
                    events.push(ImageEvent::VirtualPlacement {
                        image_id: v.image_id,
                        cols: v.cols,
                        rows: v.rows,
                        z_index: v.z_index,
                    });
                }
            }
        }
    }
}

/// Replay every placement anchor through a reflow's row map (REQ-604).
/// Survivors are re-stated in full: a re-wrap moves each anchor by its
/// own delta, so the uniform `PlacementsShifted` cannot carry it.
/// Anchors whose line left the retained scrollback evict as
/// [`felis_grid::images::Placements::shift_up`] eviction does.
pub fn apply_reflow_remap(
    remap: &felis_grid::ReflowRemap,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
) {
    if placements.is_empty() {
        return;
    }
    let evicted = placements.remap_rows(|row| remap.remap_row(row));
    for p in placements.iter() {
        events.push(ImageEvent::Placement {
            image_id: p.image_id,
            placement_id: p.placement_id,
            anchor_row: p.anchor.row,
            anchor_col: p.anchor.col,
            cols: p.cols,
            rows: p.rows,
            source: p.source,
            z_index: p.z_index,
        });
    }
    for p in evicted {
        images.release(p.image_id);
        events.push(ImageEvent::PlacementRemoved {
            image_id: p.image_id,
            placement_id: p.placement_id,
        });
    }
}

fn handle_complete(ctx: &mut ApcCtx<'_>, complete: &CompleteCommand) -> Option<Vec<u8>> {
    let ApcCtx {
        grid,
        images,
        placements,
        events,
        shm,
        cell_pixel_w,
        cell_pixel_h,
        anchor_cursor,
    } = ctx;
    let (cell_pixel_w, cell_pixel_h) = (*cell_pixel_w, *cell_pixel_h);
    let anchor_cursor = anchor_cursor.unwrap_or_else(|| {
        let cursor = grid.cursor();
        (cursor.row, cursor.col)
    });
    let action = control_byte(complete, b'a').unwrap_or(b't');
    let mut refs = response_refs(complete);
    let quiet = quiet_level(complete);

    let outcome: ActionOutcome = match action {
        b'q' => handle_query(complete),
        b't' => handle_transmit(images, placements, events, complete, &mut refs),
        b'T' => handle_transmit_and_display(
            grid,
            images,
            placements,
            events,
            complete,
            &mut refs,
            cell_pixel_w,
            cell_pixel_h,
            anchor_cursor,
        ),
        b'p' => handle_display_existing(
            grid,
            images,
            placements,
            events,
            complete,
            &mut refs,
            cell_pixel_w,
            cell_pixel_h,
            anchor_cursor,
        ),
        b'd' => {
            // kitty parity: deletes are never acknowledged on success
            // (`kitty/graphics.c` `case 'd'` has no
            // `finish_command_response`). mpv's vo_kitty cleanup sends a
            // bare `a=d;` without q= and reads nothing, so an OK would
            // leak into the user's shell on exit.
            let outcome = handle_delete(grid, images, placements, events, complete, &mut refs);
            return match outcome {
                ActionOutcome::Ok => None,
                err @ ActionOutcome::Err(..) => apply_quiet_mode(quiet, &refs, err),
            };
        }
        b'f' => handle_animation_frame(images, placements, events, complete, &mut refs),
        b'a' => {
            // kitty parity: a successful a=a is never acknowledged
            // (`kitty/graphics.c`). kitten icat sends a=a without q= and
            // never reads responses, so an OK would land in the user's
            // shell as `_Gi=...;OK` garbage.
            let outcome = handle_animation_control(images, events, complete, &mut refs);
            return match outcome {
                ActionOutcome::Ok => None,
                err @ ActionOutcome::Err(..) => apply_quiet_mode(quiet, &refs, err),
            };
        }
        b'c' => handle_compose(images, placements, events, complete, &mut refs),
        _ => ActionOutcome::error(ErrorCode::InvalidValue, "unknown a= value"),
    };

    // Defer the unlink only once the command succeeded, the only outcome
    // proving the daemon opened the object; keeping unresolved names
    // would let a producer spend a deferral slot per escape sequence
    // without owning a segment. `a=d`/`a=a` return above and read no
    // payload.
    if matches!(outcome, ActionOutcome::Ok)
        && let Some(name) = shm_segment_name(complete)
        && let Some(evicted) = shm.record(name)
    {
        unlink_shm_segment(&evicted);
    }

    apply_quiet_mode(quiet, &refs, outcome)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionOutcome {
    Ok,
    Err(ErrorCode, &'static str),
}

impl ActionOutcome {
    const fn error(code: ErrorCode, message: &'static str) -> Self {
        Self::Err(code, message)
    }
}

/// Filled before any handler runs so an early failure still threads the
/// producer's id hint back.
fn response_refs(complete: &CompleteCommand) -> ResponseRefs {
    ResponseRefs {
        image_id: control_u32(complete, b'i'),
        image_number: control_u32(complete, b'I'),
        placement_id: control_u32(complete, b'p'),
    }
}

/// `q=3` or any non-standard value is treated as fully quiet, so the
/// dispatcher never out-shouts the producer.
fn quiet_level(complete: &CompleteCommand) -> u8 {
    match control_u32(complete, b'q').unwrap_or(0) {
        0 => 0,
        1 => 1,
        _ => 2,
    }
}

fn apply_quiet_mode(quiet: u8, refs: &ResponseRefs, outcome: ActionOutcome) -> Option<Vec<u8>> {
    let status = match outcome {
        ActionOutcome::Ok => Status::Ok,
        ActionOutcome::Err(code, message) => Status::Error { code, message },
    };
    match (quiet, &status) {
        (2, _) | (1, Status::Ok) => {
            // `q=` suppresses the wire response only: mpv-class producers
            // run `q=2` permanently, so a dropped frame would otherwise
            // be invisible even at debug level.
            if let Status::Error { code, message } = &status {
                tracing::debug!(
                    ?code,
                    message,
                    "graphics command failed (response suppressed by q=)"
                );
            }
            None
        }
        _ => Some(format_response(refs, status)),
    }
}

/// Producers feature-detect the dispatcher this way (the spec
/// recommends a tiny test image with `a=q`).
fn handle_query(complete: &CompleteCommand) -> ActionOutcome {
    match decode_image(complete) {
        Ok(_) => ActionOutcome::Ok,
        Err(err) => decode_error_to_outcome(err),
    }
}

fn handle_transmit(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
) -> ActionOutcome {
    match transmit_image(images, placements, events, complete, refs) {
        Ok(_) => ActionOutcome::Ok,
        Err(outcome) => outcome,
    }
}

/// Returns the resolved [`ImageId`] so `a=T` can place an anonymous
/// transmit (no `i=`, so `refs.image_id` stays unset).
fn transmit_image(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
) -> Result<ImageId, ActionOutcome> {
    let id = resolve_or_allocate_image_id(complete, refs, images)?;
    let entry = decode_image(complete).map_err(decode_error_to_outcome)?;
    match insert_into_store(images, placements, events, id, entry) {
        ActionOutcome::Ok => Ok(id),
        err @ ActionOutcome::Err(..) => Err(err),
    }
}

/// Evict the oldest images, placements and all, until `needed` more bytes fit.
///
/// Ensures space when un-erased pinned placements would otherwise prevent
/// the refcount-respecting LRU from freeing space. Replacement images
/// free superseded memory inside `insert` and are not evicted here.
fn evict_oldest_to_fit(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    incoming: ImageId,
    needed: usize,
) {
    let replacing = images.get(incoming).map_or(0, ImageEntry::byte_len);
    while images.bytes_used() - replacing + needed > images.bytes_cap() {
        let Some(oldest) = images.iter_ids().find(|id| *id != incoming) else {
            break;
        };
        for removed in placements.delete_image(oldest) {
            images.release(removed.image_id);
            events.push(ImageEvent::PlacementRemoved {
                image_id: removed.image_id,
                placement_id: removed.placement_id,
            });
        }
        if !free_image(images, placements, events, oldest) {
            break;
        }
    }
}

/// The store frees refcount-0 entries on its own to fit a transmit or
/// an animation frame. Removals are emitted as `Delete` events so client
/// mirrors do not count bytes the daemon does not hold.
fn announce_evictions(
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    evicted: Evicted,
) {
    for id in evicted {
        placements.remove_virtual(id);
        events.push(ImageEvent::Delete { id });
    }
}

/// Central removal helper for dispatcher image deletions.
///
/// Synchronizes store deletion with virtual placement removal so rehydrate
/// never replays dangling extents against missing images.
fn free_image(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    id: ImageId,
) -> bool {
    if images.delete(id).is_none() {
        return false;
    }
    placements.remove_virtual(id);
    events.push(ImageEvent::Delete { id });
    true
}

/// Like [`resolve_image_id`], but a missing id is not an error: the
/// Kitty spec lets a producer transmit unnamed (yazi's direct preview
/// path does, with `q=2`). `refs.image_id` stays unset since there is
/// no client-chosen id to echo.
fn resolve_or_allocate_image_id(
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
    images: &mut felis_grid::images::ImageStore,
) -> Result<ImageId, ActionOutcome> {
    if control_u32(complete, b'i').is_some() || control_u32(complete, b'I').is_some() {
        return resolve_image_id(complete, refs);
    }
    Ok(images.allocate_anonymous_id())
}

fn handle_transmit_and_display(
    grid: &mut felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
    cell_pixel_w: u16,
    cell_pixel_h: u16,
    anchor_cursor: (u16, u16),
) -> ActionOutcome {
    let id = match transmit_image(images, placements, events, complete, refs) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    // `U=1`: the producer paints `U+10EEEE` placeholder cells wherever
    // it wants tiles (presenterm, tmux-compatible producers); anchoring
    // at the cursor would paint a stray copy.
    if control_u32(complete, b'U') == Some(1) {
        let cols = control_u16(complete, b'c').unwrap_or(0);
        let rows = control_u16(complete, b'r').unwrap_or(0);
        let z_index = control_i32(complete, b'z').unwrap_or(0);
        // Virtual placements are not pinned: producers churn image ids across
        // renders without removal messages, so refcount 0 lets the LRU evict
        // them while `free_image` drops the extent with the image.
        placements.upsert_virtual(felis_grid::images::VirtualPlacement {
            image_id: id,
            cols,
            rows,
            z_index,
        });
        events.push(ImageEvent::VirtualPlacement {
            image_id: id,
            cols,
            rows,
            z_index,
        });
        refs.image_id = Some(id.0);
        return ActionOutcome::Ok;
    }
    upsert_placement(
        grid,
        images,
        placements,
        events,
        id,
        complete,
        refs,
        cell_pixel_w,
        cell_pixel_h,
        anchor_cursor,
    )
}

fn handle_display_existing(
    grid: &mut felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
    cell_pixel_w: u16,
    cell_pixel_h: u16,
    anchor_cursor: (u16, u16),
) -> ActionOutcome {
    let id = match resolve_image_id(complete, refs) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    if images.get(id).is_none() {
        return ActionOutcome::error(ErrorCode::NotFound, "image not in store");
    }
    upsert_placement(
        grid,
        images,
        placements,
        events,
        id,
        complete,
        refs,
        cell_pixel_w,
        cell_pixel_h,
        anchor_cursor,
    )
}

fn handle_delete(
    grid: &felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
) -> ActionOutcome {
    // An absent `d=` means `a` (`kitty/graphics.c` handle_delete_command
    // falls `case 0:` through to `'a'`). mpv's vo_kitty cleanup is a
    // bare `\e_Ga=d;`, which an `'i'` default would answer with EINVAL.
    let mode = control_byte(complete, b'd').unwrap_or(b'a');
    match mode {
        b'i' => delete_by_id(images, placements, events, complete, refs, false),
        b'I' => delete_by_id(images, placements, events, complete, refs, true),
        b'a' | b'A' => delete_all(images, placements, events, mode == b'A'),
        b'c' | b'C' => {
            let cursor = grid.cursor();
            // Cursor is 0-based, placement geometry 1-based.
            let row_1 = cursor.row.saturating_add(1);
            let col_1 = cursor.col.saturating_add(1);
            delete_filtered(
                images,
                placements,
                events,
                |p| p.contains_cell(row_1, col_1),
                mode == b'C',
            )
        }
        b'p' | b'P' => {
            let x = control_u16(complete, b'x');
            let y = control_u16(complete, b'y');
            let (Some(x), Some(y)) = (x, y) else {
                return ActionOutcome::error(ErrorCode::InvalidValue, "d=p requires x= and y=");
            };
            delete_filtered(
                images,
                placements,
                events,
                |p| p.contains_cell(y, x),
                mode == b'P',
            )
        }
        b'q' | b'Q' => {
            let x = control_u16(complete, b'x');
            let y = control_u16(complete, b'y');
            let z = control_i32(complete, b'z');
            let (Some(x), Some(y), Some(z)) = (x, y, z) else {
                return ActionOutcome::error(
                    ErrorCode::InvalidValue,
                    "d=q requires x=, y=, and z=",
                );
            };
            delete_filtered(
                images,
                placements,
                events,
                |p| p.contains_cell(y, x) && p.z_index == z,
                mode == b'Q',
            )
        }
        b'x' | b'X' => {
            let Some(x) = control_u16(complete, b'x') else {
                return ActionOutcome::error(ErrorCode::InvalidValue, "d=x requires x=");
            };
            delete_filtered(
                images,
                placements,
                events,
                |p| p.contains_col(x),
                mode == b'X',
            )
        }
        b'y' | b'Y' => {
            let Some(y) = control_u16(complete, b'y') else {
                return ActionOutcome::error(ErrorCode::InvalidValue, "d=y requires y=");
            };
            delete_filtered(
                images,
                placements,
                events,
                |p| p.contains_row(y),
                mode == b'Y',
            )
        }
        b'z' | b'Z' => {
            let Some(z) = control_i32(complete, b'z') else {
                return ActionOutcome::error(ErrorCode::InvalidValue, "d=z requires z=");
            };
            delete_filtered(images, placements, events, |p| p.z_index == z, mode == b'Z')
        }
        b'r' | b'R' => {
            let lo = control_u32(complete, b'x');
            let hi = control_u32(complete, b'y');
            let (Some(lo), Some(hi)) = (lo, hi) else {
                return ActionOutcome::error(
                    ErrorCode::InvalidValue,
                    "d=r requires x= (low id) and y= (high id)",
                );
            };
            if lo > hi {
                return ActionOutcome::error(
                    ErrorCode::InvalidValue,
                    "d=r needs x= <= y= (low <= high image id)",
                );
            }
            delete_filtered(
                images,
                placements,
                events,
                |p| {
                    let id = p.image_id.0;
                    id >= lo && id <= hi
                },
                mode == b'R',
            )
        }
        b'f' | b'F' => {
            handle_delete_frame(images, placements, events, complete, refs, mode == b'F')
        }
        _ => ActionOutcome::error(
            ErrorCode::Unsupported,
            "unknown d= letter (animation frame delete is d=f/d=F)",
        ),
    }
}

fn delete_all(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    free_images: bool,
) -> ActionOutcome {
    let removed = std::mem::replace(placements, felis_grid::images::Placements::new());
    for placement in removed.iter() {
        images.release(placement.image_id);
        events.push(ImageEvent::PlacementRemoved {
            image_id: placement.image_id,
            placement_id: placement.placement_id,
        });
    }
    if free_images {
        let ids: Vec<ImageId> = images.iter_ids().collect();
        for id in ids {
            free_image(images, placements, events, id);
        }
    }
    ActionOutcome::Ok
}

fn delete_filtered(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    pred: impl FnMut(&Placement) -> bool,
    free_images: bool,
) -> ActionOutcome {
    let removed = placements.remove_where(pred);
    let mut touched_ids: Vec<ImageId> = Vec::new();
    for placement in &removed {
        if !touched_ids.contains(&placement.image_id) {
            touched_ids.push(placement.image_id);
        }
        images.release(placement.image_id);
        events.push(ImageEvent::PlacementRemoved {
            image_id: placement.image_id,
            placement_id: placement.placement_id,
        });
    }
    if free_images {
        for id in touched_ids {
            free_image(images, placements, events, id);
        }
    }
    ActionOutcome::Ok
}

fn delete_by_id(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
    free_image: bool,
) -> ActionOutcome {
    let id = match resolve_image_id(complete, refs) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    let placement_id = control_u32(complete, b'p').map(PlacementId);
    if placement_id.is_some() {
        if let Some(removed) = placements.remove(id, placement_id) {
            images.release(removed.image_id);
            events.push(ImageEvent::PlacementRemoved {
                image_id: removed.image_id,
                placement_id: removed.placement_id,
            });
        }
    } else {
        for removed in placements.delete_image(id) {
            images.release(removed.image_id);
            events.push(ImageEvent::PlacementRemoved {
                image_id: removed.image_id,
                placement_id: removed.placement_id,
            });
        }
    }
    if free_image {
        self::free_image(images, placements, events, id);
    }
    ActionOutcome::Ok
}

/// `i=` wins over `I=` per the Kitty spec; both absent is `EINVAL`
/// (transmit actions use [`resolve_or_allocate_image_id`]). `0` is
/// reserved.
fn resolve_image_id(
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
) -> Result<ImageId, ActionOutcome> {
    if let Some(id) = control_u32(complete, b'i') {
        if id == 0 {
            return Err(ActionOutcome::error(
                ErrorCode::InvalidValue,
                "i= must be non-zero",
            ));
        }
        refs.image_id = Some(id);
        return Ok(ImageId(id));
    }
    if let Some(num) = control_u32(complete, b'I') {
        if num == 0 {
            return Err(ActionOutcome::error(
                ErrorCode::InvalidValue,
                "I= must be non-zero",
            ));
        }
        // Image numbers are a separate terminal-assigned namespace per
        // spec; `I=` is treated as the id and echoed so `I=`-only
        // producers get consistent ids across re-queries.
        refs.image_id = Some(num);
        return Ok(ImageId(num));
    }
    Err(ActionOutcome::error(
        ErrorCode::InvalidValue,
        "transmit/display requires i= or I=",
    ))
}

fn insert_into_store(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    id: ImageId,
    entry: ImageEntry,
) -> ActionOutcome {
    // Make room before the refcount-respecting insert, or a
    // frame-streaming producer's pinned frames fill the cap once and
    // every later frame is rejected.
    evict_oldest_to_fit(images, placements, events, id, entry.byte_len());
    match images.insert(id, entry) {
        Ok(evicted) => {
            announce_evictions(placements, events, evicted);
            events.push(ImageEvent::Transmit(id));
            ActionOutcome::Ok
        }
        Err(InsertError::OverCapacity { .. }) => ActionOutcome::error(
            ErrorCode::Unsupported,
            "image store cap reached; free placements and retry",
        ),
        // `InsertError` is `#[non_exhaustive]`; unmapped variants surface
        // as ENOTSUP so the producer still learns the transmission did
        // not stick.
        Err(_) => ActionOutcome::error(ErrorCode::Unsupported, "image store rejected insert"),
    }
}

fn upsert_placement(
    grid: &mut felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    image_id: ImageId,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
    cell_pixel_w: u16,
    cell_pixel_h: u16,
    anchor_cursor: (u16, u16),
) -> ActionOutcome {
    let (cursor_row, cursor_col) = anchor_cursor;
    let placement_id = control_u32(complete, b'p').map(PlacementId);
    let req_cols = control_u16(complete, b'c').unwrap_or(0);
    let req_rows = control_u16(complete, b'r').unwrap_or(0);
    let z_index = control_i32(complete, b'z').unwrap_or(0);
    let no_cursor_move = control_u32(complete, b'C').is_some_and(|v| v == 1);
    let quiet = quiet_level(complete);
    let source = source_rect(complete);
    // Natural sizing (`c=0` / `r=0`) resolves now, while the image
    // dimensions are in scope: `advance_cursor_after_image_placement`
    // short-circuits on `(0, 0)`, leaving the cursor at the anchor row
    // so the next prompt prints under the image (the pixcat burn-in).
    let (cols, rows) = resolve_natural_cells(
        images,
        image_id,
        req_cols,
        req_rows,
        cell_pixel_w,
        cell_pixel_h,
    );
    let anchor = CellPos {
        // 1-based `CellPos`. The i32 widening is the scrollback-anchor
        // space (docs/reference/protocols/kitty-graphics.md
        // "Scrollback-anchored placements"); fresh placements are >= 1.
        row: i32::from(cursor_row) + 1,
        col: cursor_col.saturating_add(1),
    };
    if !images.retain(image_id) {
        return ActionOutcome::error(
            ErrorCode::NotFound,
            "image vanished before placement could pin it",
        );
    }
    placements.upsert(Placement {
        image_id,
        placement_id,
        anchor,
        cols,
        rows,
        source,
        z_index,
        no_cursor_move,
        quiet,
    });
    refs.image_id = Some(image_id.0);
    if let Some(p) = placement_id {
        refs.placement_id = Some(p.0);
    }
    events.push(ImageEvent::Placement {
        image_id,
        placement_id,
        anchor_row: anchor.row,
        anchor_col: anchor.col,
        cols,
        rows,
        source,
        z_index,
    });
    // The only Grid mutation the dispatcher makes
    // (docs/explanation/protocols/kitty-graphics.md "Dispatcher
    // architecture").
    grid.advance_cursor_after_image_placement(rows, cols, no_cursor_move);
    ActionOutcome::Ok
}

/// Falls back to a single cell when the cell pixel dimensions are not
/// yet known (no client attached, or the resize arrived after the
/// placement), so the cursor keeps moving instead of burning in at the
/// anchor.
fn resolve_natural_cells(
    images: &felis_grid::images::ImageStore,
    image_id: ImageId,
    req_cols: u16,
    req_rows: u16,
    cell_pixel_w: u16,
    cell_pixel_h: u16,
) -> (u16, u16) {
    if req_cols != 0 && req_rows != 0 {
        return (req_cols, req_rows);
    }
    let Some(entry) = images.get(image_id) else {
        return (req_cols.max(1), req_rows.max(1));
    };
    let cw = u32::from(cell_pixel_w.max(1));
    let ch = u32::from(cell_pixel_h.max(1));
    let natural_cols = entry.width.div_ceil(cw);
    let natural_rows = entry.height.div_ceil(ch);
    let resolved_cols = if req_cols == 0 {
        u16::try_from(natural_cols).unwrap_or(u16::MAX)
    } else {
        req_cols
    };
    let resolved_rows = if req_rows == 0 {
        u16::try_from(natural_rows).unwrap_or(u16::MAX)
    } else {
        req_rows
    };
    (resolved_cols.max(1), resolved_rows.max(1))
}

/// All four or none: a partial spec collapses to the whole image per
/// Kitty.
fn source_rect(complete: &CompleteCommand) -> Option<felis_protocol::messages::SourceRect> {
    let x = control_u32(complete, b'x')?;
    let y = control_u32(complete, b'y')?;
    let width = control_u32(complete, b'w')?;
    let height = control_u32(complete, b'h')?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(felis_protocol::messages::SourceRect {
        x,
        y,
        width,
        height,
    })
}

const fn decode_error_to_outcome(err: DecodeError) -> ActionOutcome {
    match err {
        DecodeError::InvalidValue(m) => ActionOutcome::error(ErrorCode::InvalidValue, m),
        DecodeError::Unsupported(m) => ActionOutcome::error(ErrorCode::Unsupported, m),
        DecodeError::BadImage(m) => ActionOutcome::error(ErrorCode::BadImage, m),
        DecodeError::OverBudget => ActionOutcome::error(
            ErrorCode::Unsupported,
            "image exceeds per-image decoder budget",
        ),
        DecodeError::IoError(m) => ActionOutcome::error(ErrorCode::IoError, m),
    }
}

/// `a=f`: the transmitted rectangle is composited onto a full canvas
/// (a copy of base frame `c=`, the `Y=` fill, or the edited frame's
/// pixels), so every stored frame is a coalesced full-canvas frame
/// (docs/reference/protocols/kitty-graphics.md "Animation").
fn handle_animation_frame(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
) -> ActionOutcome {
    let id = match resolve_image_id(complete, refs) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    let (img_w, img_h, format, frame_count) = match images.get(id) {
        Some(e) => (e.width, e.height, e.format, e.frame_count()),
        None => return ActionOutcome::error(ErrorCode::NotFound, "a=f: image not in store"),
    };
    let img_bpp = format.bytes_per_pixel();

    let data = match decode_image(complete) {
        Ok(d) => d,
        Err(err) => return decode_error_to_outcome(err),
    };
    let off_x = control_u32(complete, b'x').unwrap_or(0);
    let off_y = control_u32(complete, b'y').unwrap_or(0);
    if off_x.saturating_add(data.width) > img_w || off_y.saturating_add(data.height) > img_h {
        return ActionOutcome::error(
            ErrorCode::InvalidValue,
            "a=f: frame data rectangle exceeds image bounds",
        );
    }
    let data_px = compose::convert_bpp(data.pixels(), data.format.bytes_per_pixel(), img_bpp);
    let replace = control_u32(complete, b'C') == Some(1);
    let z = control_i32(complete, b'z');
    let target = control_u32(complete, b'r'); // 1-based frame to edit; absent → append
    let base = control_u32(complete, b'c'); // 1-based base frame for a new frame
    let bgcolor = control_u32(complete, b'Y').unwrap_or(0);

    if let Some(rn) = target
        && rn >= 1
        && (rn as usize) <= frame_count
    {
        let idx = rn as usize - 1;
        let (mut canvas, prev_gap) = match images.get(id).and_then(|e| e.frame(idx)) {
            Some(f) => (f.pixels.to_vec(), f.gap_ms),
            None => return ActionOutcome::error(ErrorCode::NotFound, "a=f: frame vanished"),
        };
        compose::blit(
            &mut canvas,
            img_w,
            img_h,
            &data_px,
            data.width,
            data.height,
            off_x,
            off_y,
            img_bpp,
            replace,
        );
        // kitty only changes an edited frame's gap when z is given.
        let gap = if z.is_some() { gap_from_z(z) } else { prev_gap };
        return match images.replace_frame(
            id,
            idx,
            Frame {
                pixels: canvas.into(),
                gap_ms: gap,
            },
        ) {
            Ok(evicted) => {
                announce_evictions(placements, events, evicted);
                events.push(ImageEvent::TransmitFrame {
                    id,
                    index: idx as u32,
                });
                ActionOutcome::Ok
            }
            Err(err) => frame_error_to_outcome(err),
        };
    }

    let mut canvas = match base {
        Some(cn) if cn >= 1 && (cn as usize) <= frame_count => images
            .get(id)
            .and_then(|e| e.frame(cn as usize - 1))
            .map_or_else(
                || compose::background_canvas(img_w, img_h, format, bgcolor),
                |f| f.pixels.to_vec(),
            ),
        _ => compose::background_canvas(img_w, img_h, format, bgcolor),
    };
    compose::blit(
        &mut canvas,
        img_w,
        img_h,
        &data_px,
        data.width,
        data.height,
        off_x,
        off_y,
        img_bpp,
        replace,
    );
    match images.push_frame(
        id,
        Frame {
            pixels: canvas.into(),
            gap_ms: gap_from_z(z),
        },
    ) {
        Ok(pushed) => {
            announce_evictions(placements, events, pushed.evicted);
            events.push(ImageEvent::TransmitFrame {
                id,
                index: pushed.index as u32,
            });
            ActionOutcome::Ok
        }
        Err(err) => frame_error_to_outcome(err),
    }
}

fn handle_animation_control(
    images: &mut felis_grid::images::ImageStore,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
) -> ActionOutcome {
    let id = match resolve_image_id(complete, refs) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    let Some(mut entry) = images.animation_control(id) else {
        return ActionOutcome::error(ErrorCode::NotFound, "a=a: image not in store");
    };

    if let Some(rn) = control_u32(complete, b'r')
        && let Some(z) = control_i32(complete, b'z')
        && rn >= 1
    {
        entry.set_gap(rn as usize - 1, gap_from_z(Some(z)));
    }
    let jumped = if let Some(cn) = control_u32(complete, b'c')
        && cn >= 1
        && entry.jump_to(cn as usize - 1)
    {
        Some(cn - 1)
    } else {
        None
    };
    match control_u32(complete, b's') {
        Some(1) => entry.set_mode(felis_grid::images::AnimationMode::Stopped),
        Some(2) => entry.set_mode(felis_grid::images::AnimationMode::Loading),
        Some(3) => entry.set_mode(felis_grid::images::AnimationMode::Running),
        _ => {}
    }
    // max_loops = v - 1; v=0 ignored, v=1 is infinite.
    if let Some(v) = control_u32(complete, b'v')
        && v >= 1
    {
        entry.set_max_loops(v - 1);
    }

    if let Some(idx) = jumped {
        events.push(ImageEvent::ShowFrame { id, index: idx });
    }
    ActionOutcome::Ok
}

fn handle_compose(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
) -> ActionOutcome {
    let id = match resolve_image_id(complete, refs) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    let (img_w, img_h, format, frame_count) = match images.get(id) {
        Some(e) => (e.width, e.height, e.format, e.frame_count()),
        None => return ActionOutcome::error(ErrorCode::NotFound, "a=c: image not in store"),
    };
    let (Some(src_n), Some(dst_n)) = (control_u32(complete, b'r'), control_u32(complete, b'c'))
    else {
        return ActionOutcome::error(
            ErrorCode::InvalidValue,
            "a=c requires r= (source frame) and c= (destination frame)",
        );
    };
    if src_n < 1 || src_n as usize > frame_count {
        return ActionOutcome::error(ErrorCode::NotFound, "a=c: source frame does not exist");
    }
    if dst_n < 1 || dst_n as usize > frame_count {
        return ActionOutcome::error(ErrorCode::NotFound, "a=c: destination frame does not exist");
    }
    let w = control_u32(complete, b'w')
        .filter(|&w| w > 0)
        .unwrap_or(img_w);
    let h = control_u32(complete, b'h')
        .filter(|&h| h > 0)
        .unwrap_or(img_h);
    let dst_x = control_u32(complete, b'x').unwrap_or(0);
    let dst_y = control_u32(complete, b'y').unwrap_or(0);
    let src_x = control_u32(complete, b'X').unwrap_or(0);
    let src_y = control_u32(complete, b'Y').unwrap_or(0);
    if dst_x.saturating_add(w) > img_w || dst_y.saturating_add(h) > img_h {
        return ActionOutcome::error(
            ErrorCode::InvalidValue,
            "a=c: destination rectangle out of bounds",
        );
    }
    if src_x.saturating_add(w) > img_w || src_y.saturating_add(h) > img_h {
        return ActionOutcome::error(
            ErrorCode::InvalidValue,
            "a=c: source rectangle out of bounds",
        );
    }
    if src_n == dst_n {
        let x_overlaps = src_x.max(dst_x) < src_x.min(dst_x).saturating_add(w);
        let y_overlaps = src_y.max(dst_y) < src_y.min(dst_y).saturating_add(h);
        if x_overlaps && y_overlaps {
            return ActionOutcome::error(
                ErrorCode::InvalidValue,
                "a=c: source and destination overlap within the same frame",
            );
        }
    }
    let replace = control_u32(complete, b'C') == Some(1);
    let bpp = format.bytes_per_pixel();
    let src_idx = src_n as usize - 1;
    let dst_idx = dst_n as usize - 1;
    let src_px = match images.get(id).and_then(|e| e.frame(src_idx)) {
        Some(f) => f.pixels.clone(),
        None => return ActionOutcome::error(ErrorCode::NotFound, "a=c: source frame vanished"),
    };
    let (mut dst_px, dst_gap) = match images.get(id).and_then(|e| e.frame(dst_idx)) {
        Some(f) => (f.pixels.to_vec(), f.gap_ms),
        None => {
            return ActionOutcome::error(ErrorCode::NotFound, "a=c: destination frame vanished");
        }
    };
    compose::blit_region(
        &mut dst_px,
        &src_px,
        img_w,
        dst_x,
        dst_y,
        src_x,
        src_y,
        w,
        h,
        bpp,
        replace,
    );
    let _ = img_h;
    match images.replace_frame(
        id,
        dst_idx,
        Frame {
            pixels: dst_px.into(),
            gap_ms: dst_gap,
        },
    ) {
        Ok(evicted) => {
            announce_evictions(placements, events, evicted);
            events.push(ImageEvent::TransmitFrame {
                id,
                index: dst_idx as u32,
            });
            ActionOutcome::Ok
        }
        Err(err) => frame_error_to_outcome(err),
    }
}

/// `d=F` on an image with no extra frames deletes the whole image. No
/// per-frame removal message exists and the surviving frames shift
/// index, so a removal re-states the whole frame set.
fn handle_delete_frame(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
    free_image_if_no_frames: bool,
) -> ActionOutcome {
    let id = match resolve_image_id(complete, refs) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    let Some(frame_count) = images.get(id).map(ImageEntry::frame_count) else {
        return ActionOutcome::error(ErrorCode::NotFound, "d=f: image not in store");
    };
    if frame_count <= 1 {
        if free_image_if_no_frames {
            return delete_by_id(images, placements, events, complete, refs, true);
        }
        return ActionOutcome::Ok;
    }
    let rn = control_u32(complete, b'r')
        .unwrap_or(1)
        .clamp(1, frame_count as u32);
    let idx = rn as usize - 1;
    match images.remove_frame(id, idx) {
        Ok(_) => {
            events.push(ImageEvent::Transmit(id));
            ActionOutcome::Ok
        }
        Err(err) => frame_error_to_outcome(err),
    }
}

const fn frame_error_to_outcome(err: FrameError) -> ActionOutcome {
    match err {
        FrameError::NoSuchImage => ActionOutcome::error(ErrorCode::NotFound, "image not in store"),
        FrameError::NoSuchFrame => {
            ActionOutcome::error(ErrorCode::NotFound, "frame does not exist")
        }
        FrameError::OverCapacity { .. } => ActionOutcome::error(
            ErrorCode::Unsupported,
            "image store cap reached; free frames or images and retry",
        ),
        // Kitty's closed status set has no ENOSPC, so a full image
        // answers with the same ENOTSUP its byte cap already uses: the
        // producer's remedy is identical (delete frames, or split the
        // animation across ids).
        FrameError::TooManyFrames { .. } => ActionOutcome::error(
            ErrorCode::Unsupported,
            "image frame cap reached; delete frames and retry",
        ),
        // `FrameError` is `#[non_exhaustive]`; unmapped variants surface
        // as ENOTSUP, like the `InsertError` path.
        _ => ActionOutcome::error(ErrorCode::Unsupported, "frame mutation rejected"),
    }
}

#[cfg(test)]
mod tests;

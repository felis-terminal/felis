//! Kitty graphics dispatcher. Per
//! docs/explanation/protocols/kitty-graphics.md "Dispatcher architecture"
//! it runs on the daemon, not the grid: `Grid::apc_dispatch` buffers
//! raw APC bodies into its outbox, and the serve loop drains the outbox
//! after every `Parser::advance` and feeds each body through here.

use std::io::Cursor;
use std::num::NonZeroU32;

use bytes::Bytes;
use felis_grid::images::{
    AnyPlacement, CellPos, Displaced, Evicted, Extent, Frame, FrameError, ImageEntry, ImageId,
    InsertError, Placement, PlacementId, VirtualPlacement,
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
#[cfg(all(test, unix))]
use image_decode::temp_file_is_deletable;
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
        requested: felis_protocol::messages::RequestedExtent,
        serial: u64,
    },
    PlacementRemoved {
        image_id: ImageId,
        placement_id: Option<PlacementId>,
    },
    VirtualPlacement {
        image_id: ImageId,
        placement_id: Option<PlacementId>,
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
            serial: Some(entry.serial()),
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
                requested,
                serial,
            } => out.push(ImageMsg::Placement {
                image_id,
                placement_id,
                anchor_row,
                anchor_col,
                cols,
                rows,
                source,
                z_index,
                requested: Some(requested),
                serial: Some(serial),
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
                placement_id,
                cols,
                rows,
                z_index,
            } => out.push(ImageMsg::VirtualPlacement {
                image_id,
                placement_id,
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

/// Continues an in-place upgrade predecessor's clock, which read
/// `elapsed_ms` at its dump. `false` when this process already started
/// its own clock, or cannot reach that far back on its own.
pub fn resume_anim_clock(elapsed_ms: u64) -> bool {
    std::time::Instant::now()
        .checked_sub(std::time::Duration::from_millis(elapsed_ms))
        .is_some_and(|epoch| ANIM_EPOCH.set(epoch).is_ok())
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
    kitty_graphics::control(&complete.controls, key)
}

fn control_byte(complete: &CompleteCommand, key: u8) -> Option<u8> {
    let v = control_value(complete, key)?;
    (v.len() == 1).then(|| v[0])
}

fn control_u32(complete: &CompleteCommand, key: u8) -> Option<u32> {
    kitty_graphics::control_u32(&complete.controls, key)
}

/// `i=` or `I=`; kitty's parser defaults both to 0, so an explicit zero
/// is the same as leaving the key out.
fn image_ref(complete: &CompleteCommand, key: u8) -> Option<u32> {
    control_u32(complete, key).filter(|&v| v != 0)
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
    /// `0` (no client size yet) resolves an auto extent with a 1 px
    /// cell; see [`felis_grid::images::Extent::resolve`].
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
        // kitty logs an unparseable command and answers nothing: with no
        // parsed `i=`, any reply would land in the shell as text.
        tracing::debug!("malformed graphics escape dropped");
        return BodyDispatch::Silent;
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
            match apply_quiet_mode(addressed(&head), quiet, &refs, outcome) {
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

/// The primary screen's images while the alternate screen is up: kitty
/// gives each screen a store of its own, so no alternate-screen command
/// reaches these.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedImages {
    pub images: felis_grid::images::ImageStore,
    pub placements: felis_grid::images::Placements,
}

/// The images of the screen not shown: the primary one while the
/// alternate screen is up, and the alternate one a `?47l` kept.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SavedScreens {
    pub primary: Option<SavedImages>,
    pub alternate: Option<SavedImages>,
}

/// Swap the live image store and placements for the other screen's,
/// which the client's one mirrored store follows. Each placement is
/// removed explicitly: a `Delete` then a `Transmit` of one id collapses
/// to the `Transmit` at materialization.
pub fn apply_screen_switch(
    switch: ScreenSwitch,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    saved: &mut SavedScreens,
    events: &mut Vec<ImageEvent>,
) {
    tracing::trace!(
        target: "felis_daemon::graphics",
        ?switch,
        saved_primary = saved.primary.is_some(),
        live_placements = placements.len(),
        "apply_screen_switch",
    );
    let coming = match switch {
        ScreenSwitch::EnteredAlternate { resumed } => {
            let alternate = saved.alternate.take().filter(|_| resumed);
            alternate.unwrap_or_else(|| SavedImages {
                images: felis_grid::images::ImageStore::new(images.bytes_cap()),
                placements: felis_grid::images::Placements::with_cap(placements.cap()),
            })
        }
        ScreenSwitch::LeftAlternate { .. } => {
            let Some(primary) = saved.primary.take() else {
                return;
            };
            primary
        }
    };
    let leaving = SavedImages {
        images: std::mem::replace(images, coming.images),
        placements: std::mem::replace(placements, coming.placements),
    };
    for p in leaving.placements.iter() {
        events.push(ImageEvent::PlacementRemoved {
            image_id: p.image_id,
            placement_id: p.placement_id,
        });
    }
    for v in leaving.placements.iter_virtual() {
        events.push(virtual_removed_event(v));
    }
    for id in leaving.images.iter_ids() {
        events.push(ImageEvent::Delete { id });
    }
    for id in images.iter_ids() {
        events.push(ImageEvent::Transmit(id));
    }
    events.extend(placements.iter().map(placement_event));
    events.extend(placements.iter_virtual().map(virtual_event));
    match switch {
        // A leftover save means a missed `LeftAlternate` drain;
        // overwrite rather than panic.
        ScreenSwitch::EnteredAlternate { .. } => saved.primary = Some(leaving),
        ScreenSwitch::LeftAlternate { kept } => saved.alternate = kept.then_some(leaving),
    }
}

const fn virtual_event(v: &VirtualPlacement) -> ImageEvent {
    ImageEvent::VirtualPlacement {
        image_id: v.image_id,
        placement_id: v.placement_id,
        cols: v.cols,
        rows: v.rows,
        z_index: v.z_index,
    }
}

const fn virtual_removed_event(v: &VirtualPlacement) -> ImageEvent {
    ImageEvent::PlacementRemoved {
        image_id: v.image_id,
        placement_id: v.placement_id,
    }
}

const fn placement_event(p: &Placement) -> ImageEvent {
    ImageEvent::Placement {
        image_id: p.image_id,
        placement_id: p.placement_id,
        anchor_row: p.anchor.row,
        anchor_col: p.anchor.col,
        cols: p.extent().0,
        rows: p.extent().1,
        source: p.source,
        z_index: p.z_index,
        requested: p.requested(),
        serial: p.serial,
    }
}

/// A cell-size change re-resolves every auto extent, on both screens
/// (kitty's `screen_rescale_images`). A screen not shown emits
/// nothing: it is re-stated in full when it comes back.
pub fn rescale_placements(
    images: &felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    saved: &mut SavedScreens,
    events: &mut Vec<ImageEvent>,
    cell_px: (u16, u16),
) {
    placements.rescale(images, cell_px, |p| events.push(placement_event(p)));
    for screen in [saved.primary.as_mut(), saved.alternate.as_mut()]
        .into_iter()
        .flatten()
    {
        screen.placements.rescale(&screen.images, cell_px, |_| {});
    }
}

/// Moves the placements inside a scrolled band (kitty's
/// `scroll_filter_margins_func`). Survivors are re-stated in full: a
/// clip changes a placement's source and extent, and the placements
/// outside the band stay, so the uniform `PlacementsShifted` cannot
/// carry it.
pub fn scroll_region_placements(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    (op, into_history): (felis_grid::ScrollOp, bool),
    cell_px: (u16, u16),
    retain_rows: u32,
) {
    if placements.is_empty() {
        return;
    }
    let (moved, removed) = placements.scroll_region(op, into_history, images, cell_px, retain_rows);
    events.extend(moved.iter().map(placement_event));
    for p in removed {
        images.release(p.image_id);
        events.push(ImageEvent::PlacementRemoved {
            image_id: p.image_id,
            placement_id: p.placement_id,
        });
    }
    free_unreferenced_anonymous(images, placements, events);
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
    events.extend(placements.iter().map(placement_event));
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
    let mut addressed = addressed(complete);
    if image_ref(complete, b'i').is_some() && image_ref(complete, b'I').is_some() {
        let both = ActionOutcome::error(
            ErrorCode::InvalidValue,
            "must not specify both image id and image number",
        );
        return apply_quiet_mode(true, quiet, &refs, both);
    }

    let outcome: ActionOutcome = match action {
        b'q' => {
            // kitty answers a query with its `i=` alone, and a query
            // without one not at all.
            refs = ResponseRefs {
                image_id: refs.image_id,
                ..ResponseRefs::default()
            };
            addressed = refs.image_id.is_some();
            handle_query(complete)
        }
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
            // kitty's `case 'd'` calls no `finish_command_response`, so a
            // delete is never answered, failed or not. mpv's vo_kitty
            // cleanup sends a bare `a=d;` without q= and reads nothing.
            if let ActionOutcome::Err(code, message) =
                handle_delete(grid, images, placements, events, complete, &mut refs)
            {
                tracing::debug!(
                    ?code,
                    message,
                    "graphics delete failed (kitty sends no reply)"
                );
            }
            free_unreferenced_anonymous(images, placements, events);
            return None;
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
                err @ ActionOutcome::Err(..) => apply_quiet_mode(addressed, quiet, &refs, err),
            };
        }
        b'c' => handle_compose(images, placements, events, complete, &mut refs),
        _ => {
            tracing::debug!(
                action,
                "unknown graphics action dropped (kitty sends no reply)"
            );
            return None;
        }
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

    apply_quiet_mode(addressed, quiet, &refs, outcome)
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
        image_id: image_ref(complete, b'i'),
        image_number: image_ref(complete, b'I'),
        placement_id: control_u32(complete, b'p').filter(|&p| p != 0),
        frame: (control_byte(complete, b'a') == Some(b'f'))
            .then(|| control_u32(complete, b'r').filter(|&r| r != 0))
            .flatten(),
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

/// kitty's `finish_command_response` answers only a command that names
/// its image with a non-zero `i=` or `I=`; anything else has no id to
/// address the reply to, and the reply would reach a shell as text.
fn addressed(complete: &CompleteCommand) -> bool {
    image_ref(complete, b'i').is_some() || image_ref(complete, b'I').is_some()
}

fn apply_quiet_mode(
    addressed: bool,
    quiet: u8,
    refs: &ResponseRefs,
    outcome: ActionOutcome,
) -> Option<Vec<u8>> {
    let status = match outcome {
        ActionOutcome::Ok => Status::Ok,
        ActionOutcome::Err(code, message) => Status::Error { code, message },
    };
    match (addressed, quiet, &status) {
        (false, ..) | (_, 2, _) | (_, 1, Status::Ok) => {
            // Only the wire response is suppressed: mpv-class producers
            // run `q=2` permanently, so a dropped frame would otherwise
            // be invisible even at debug level.
            if let Status::Error { code, message } = &status {
                tracing::debug!(
                    ?code,
                    message,
                    "graphics command failed (no reply: q= or no image id)"
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
    let id = resolve_or_allocate_image_id(complete, refs, images);
    let mut entry = decode_image(complete).map_err(decode_error_to_outcome)?;
    if refs.image_id.is_none() {
        entry = entry.into_anonymous();
    }
    // kitty keeps an image's number when `i=` sends it again.
    let number = image_ref(complete, b'I').or_else(|| images.get(id).and_then(ImageEntry::number));
    if let Some(number) = number {
        entry = entry.with_number(number);
    }
    match insert_into_store(images, placements, events, id, entry) {
        ActionOutcome::Ok => Ok(id),
        err @ ActionOutcome::Err(..) => Err(err),
    }
}

/// kitty's `apply_storage_quota`: every image nothing references goes
/// first, then the least recently used, placements and all, until
/// `growth` more bytes fit. The incoming image is spared.
fn evict_to_fit(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    incoming: ImageId,
    growth: usize,
) {
    let over =
        |images: &felis_grid::images::ImageStore| images.bytes_used() + growth > images.bytes_cap();
    if !over(images) {
        return;
    }
    free_unreferenced_where(images, placements, events, |id, _| id != incoming);
    while over(images) {
        let Some(oldest) = images
            .iter_ids()
            .filter(|id| *id != incoming)
            .min_by_key(|id| images.get(*id).map_or(0, ImageEntry::atime))
        else {
            break;
        };
        for removed in placements.delete_image(oldest) {
            images.release(removed.image_id);
            events.push(ImageEvent::PlacementRemoved {
                image_id: removed.image_id,
                placement_id: removed.placement_id,
            });
        }
        free_image(images, placements, events, oldest);
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

/// kitty's `grman_clear` for ED 2 / ED 3 / RIS: drop the placements the
/// clear covers, then every image nothing references any more.
pub fn clear_images(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    scope: felis_grid::ImageClear,
) {
    for p in placements.clear(scope) {
        images.release(p.image_id);
        events.push(ImageEvent::PlacementRemoved {
            image_id: p.image_id,
            placement_id: p.placement_id,
        });
    }
    free_unreferenced(images, placements, events);
}

fn free_unreferenced(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
) {
    free_unreferenced_where(images, placements, events, |_, _| true);
}

/// kitty frees an image sent without an id once its last placement
/// goes, since nothing can place it again; its `modify_refs` (every
/// scroll) and `filter_refs` (every delete) sweep for such images.
pub fn free_unreferenced_anonymous(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
) {
    free_unreferenced_where(images, placements, events, |_, entry| entry.is_anonymous());
}

fn free_unreferenced_where(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    candidate: impl Fn(ImageId, &ImageEntry) -> bool,
) {
    let candidates: Vec<ImageId> = images
        .iter_ids()
        .filter(|&id| images.get(id).is_some_and(|entry| candidate(id, entry)))
        .collect();
    if candidates.is_empty() {
        return;
    }
    let referenced: std::collections::HashSet<ImageId> = std::iter::once(&*placements)
        .flat_map(|table| {
            table
                .iter()
                .map(|p| p.image_id)
                .chain(table.iter_virtual().map(|v| v.image_id))
        })
        .collect();
    for id in candidates.into_iter().filter(|id| !referenced.contains(id)) {
        free_image(images, placements, events, id);
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
/// no client-chosen id to echo. An `I=` always names a new image, so it
/// gets a fresh id, echoed as kitty does.
fn resolve_or_allocate_image_id(
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
    images: &mut felis_grid::images::ImageStore,
) -> ImageId {
    if let Some(id) = image_ref(complete, b'i') {
        refs.image_id = Some(id);
        return ImageId(id);
    }
    if image_ref(complete, b'I').is_some() {
        let id = images.lowest_free_id();
        refs.image_id = Some(id.0);
        return id;
    }
    images.allocate_anonymous_id()
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
    if control_u32(complete, b'U') == Some(1) {
        return upsert_virtual_placement(images, placements, events, id, complete, refs);
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
    let id = match resolve_image_id(complete, refs, images) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    // kitty's put answers with the id it resolved an `I=` to.
    refs.image_id = Some(id.0);
    if images.get(id).is_none() {
        return ActionOutcome::error(ErrorCode::NotFound, "image not in store");
    }
    if control_u32(complete, b'U') == Some(1) {
        return upsert_virtual_placement(images, placements, events, id, complete, refs);
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

/// kitty keys an image's placements, direct and virtual, by one `p=`:
/// before a put takes `id`, the anonymous placement felis gave it moves
/// to a fresh id, and a producer's placement of the other kind holding
/// it goes, its serial returned for the put to keep as kitty keeps the
/// ref; one of the same kind the put's upsert replaces in place.
fn claim_placement_id(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    image_id: ImageId,
    id: PlacementId,
    virtual_put: bool,
) -> Option<u64> {
    if let Some(moved) = placements.rekey_anonymous(image_id, id) {
        events.push(ImageEvent::PlacementRemoved {
            image_id,
            placement_id: Some(id),
        });
        events.push(match &moved {
            AnyPlacement::Anchored(p) => placement_event(p),
            AnyPlacement::Virtual(v) => virtual_event(v),
        });
    }
    if virtual_put {
        let removed = placements.remove(image_id, Some(id))?;
        images.release(removed.image_id);
        events.push(ImageEvent::PlacementRemoved {
            image_id,
            placement_id: removed.placement_id,
        });
        Some(removed.serial)
    } else {
        let removed = placements.remove_virtual_placement(image_id, id)?;
        events.push(virtual_removed_event(&removed));
        Some(removed.serial)
    }
}

/// `U=1`, for `a=T` and `a=p` alike (kitty's `handle_put_command`): the
/// producer paints `U+10EEEE` placeholder cells wherever it wants tiles
/// (presenterm, tmux-compatible producers), so nothing anchors at the
/// cursor and the cursor stays.
fn upsert_virtual_placement(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    id: ImageId,
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
) -> ActionOutcome {
    let cols = control_u16(complete, b'c').unwrap_or(0);
    let rows = control_u16(complete, b'r').unwrap_or(0);
    let z_index = control_i32(complete, b'z').unwrap_or(0);
    let requested = control_u32(complete, b'p')
        .filter(|&p| p != 0 && addressed(complete))
        .map(PlacementId);
    let inherited = requested
        .and_then(|p| claim_placement_id(images, placements, events, id, p, true))
        .unwrap_or(0);
    // Virtual placements are not pinned: producers churn image ids across
    // renders without removal messages, so refcount 0 lets the LRU evict
    // them while `free_image` drops the extent with the image.
    let placement = VirtualPlacement {
        image_id: id,
        placement_id: Some(requested.unwrap_or_else(|| placements.allocate_anonymous_id(id))),
        anonymous: requested.is_none(),
        cols,
        rows,
        z_index,
        serial: inherited,
    };
    let evicted = placements.upsert_virtual(placement);
    images.touch(id);
    if let Some(gone) = evicted {
        drop_evicted(images, placements, events, gone);
    }
    events.push(virtual_event(&placement));
    refs.image_id = Some(id.0);
    ActionOutcome::Ok
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
        b'i' | b'I' => delete_by_id(images, placements, events, complete, mode == b'I'),
        b'n' | b'N' => delete_by_number(images, placements, events, complete, mode == b'N'),
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
            let in_range: Vec<ImageId> = images
                .iter_ids()
                // kitty's `id_range_filter_func` skips images sent without an id.
                .filter(|id| {
                    (lo..=hi).contains(&id.0)
                        && images.get(*id).is_some_and(|entry| !entry.is_anonymous())
                })
                .collect();
            for id in in_range {
                remove_all_placements(images, placements, events, id);
                if mode == b'R' {
                    free_image(images, placements, events, id);
                }
            }
            ActionOutcome::Ok
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

/// kitty's `clear_filter_func_noncell`: virtual placements and those
/// wholly in history stay, and `d=A` frees only the images left with
/// no placement of either kind.
fn delete_all(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    free_images: bool,
) -> ActionOutcome {
    if free_images {
        clear_images(images, placements, events, felis_grid::ImageClear::Screen);
        return ActionOutcome::Ok;
    }
    for placement in placements.clear(felis_grid::ImageClear::Screen) {
        images.release(placement.image_id);
        events.push(ImageEvent::PlacementRemoved {
            image_id: placement.image_id,
            placement_id: placement.placement_id,
        });
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
            free_if_unplaced(images, placements, events, id);
        }
    }
    ActionOutcome::Ok
}

/// An uppercase delete frees the image only once no placement of it is
/// left, as kitty's `filter_refs` does.
fn free_if_unplaced(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    id: ImageId,
) {
    if placements.for_image(id).next().is_none()
        && !placements.iter_virtual().any(|v| v.image_id == id)
    {
        free_image(images, placements, events, id);
    }
}

/// Direct and virtual alike, as kitty's `id_filter_func` matches every
/// ref of the image.
fn remove_all_placements(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    id: ImageId,
) {
    for removed in placements.delete_image(id) {
        images.release(removed.image_id);
        events.push(ImageEvent::PlacementRemoved {
            image_id: removed.image_id,
            placement_id: removed.placement_id,
        });
    }
    for removed in placements.remove_virtual(id) {
        events.push(virtual_removed_event(&removed));
    }
}

/// kitty's `d=i` matches `i=` alone; an `I=` names nothing here.
fn delete_by_id(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    free_image: bool,
) -> ActionOutcome {
    if let Some(id) = image_ref(complete, b'i') {
        delete_image_placements(
            images,
            placements,
            events,
            complete,
            ImageId(id),
            free_image,
        );
    }
    ActionOutcome::Ok
}

/// kitty's `d=n`: the newest image sent with this `I=`.
fn delete_by_number(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    free_image: bool,
) -> ActionOutcome {
    if let Some(id) = image_ref(complete, b'I').and_then(|n| images.newest_with_number(n)) {
        delete_image_placements(images, placements, events, complete, id, free_image);
    }
    ActionOutcome::Ok
}

fn delete_image_placements(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    complete: &CompleteCommand,
    id: ImageId,
    free_image: bool,
) {
    let placement_id = control_u32(complete, b'p')
        .filter(|&p| p != 0)
        .map(PlacementId);
    if let Some(placement_id) = placement_id {
        if let Some(removed) = placements.remove(id, Some(placement_id)) {
            images.release(removed.image_id);
            events.push(ImageEvent::PlacementRemoved {
                image_id: removed.image_id,
                placement_id: removed.placement_id,
            });
        }
        if let Some(removed) = placements.remove_virtual_placement(id, placement_id) {
            events.push(virtual_removed_event(&removed));
        }
    } else {
        remove_all_placements(images, placements, events, id);
    }
    if free_image {
        free_if_unplaced(images, placements, events, id);
    }
}

/// [`handle_complete`] has refused `i=` with `I=`; both absent is `EINVAL`
/// (transmit actions use [`resolve_or_allocate_image_id`]). Only `i=` is
/// echoed: kitty answers a command naming `I=` with the number alone.
fn resolve_image_id(
    complete: &CompleteCommand,
    refs: &mut ResponseRefs,
    images: &felis_grid::images::ImageStore,
) -> Result<ImageId, ActionOutcome> {
    if let Some(id) = image_ref(complete, b'i') {
        refs.image_id = Some(id);
        return Ok(ImageId(id));
    }
    if let Some(number) = image_ref(complete, b'I') {
        return images
            .newest_with_number(number)
            .ok_or_else(|| ActionOutcome::error(ErrorCode::NotFound, "no image with this number"));
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
    let growth = entry
        .byte_len()
        .saturating_sub(images.get(id).map_or(0, ImageEntry::byte_len));
    evict_to_fit(images, placements, events, id, growth);
    let replacing = images.get(id).is_some();
    match images.insert(id, entry) {
        Ok(evicted) => {
            announce_evictions(placements, events, evicted);
            // kitty deletes an image's placements when its data is
            // replaced; `insert` already reset the refcount they held.
            if replacing {
                for p in placements.delete_image(id) {
                    events.push(ImageEvent::PlacementRemoved {
                        image_id: p.image_id,
                        placement_id: p.placement_id,
                    });
                }
                for v in placements.remove_virtual(id) {
                    events.push(virtual_removed_event(&v));
                }
            }
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
    // kitty ignores `p=` on an image the producer did not name.
    let named = addressed(complete);
    let requested = control_u32(complete, b'p')
        .filter(|&p| p != 0 && named)
        .map(PlacementId);
    let req_cols = control_u16(complete, b'c').unwrap_or(0);
    let req_rows = control_u16(complete, b'r').unwrap_or(0);
    let z_index = control_i32(complete, b'z').unwrap_or(0);
    let no_cursor_move = !kitty_graphics::moves_cursor(&complete.controls);
    let quiet = quiet_level(complete);
    let (image_w, image_h) = images
        .get(image_id)
        .map_or((0, 0), |entry| (entry.width, entry.height));
    let source = source_rect(complete, image_w, image_h);
    // Natural sizing (`c=0` / `r=0`) resolves now, while the image
    // dimensions are in scope: the cursor advance needs the extent, and
    // a cursor left at the anchor row prints the next prompt under the
    // image (the pixcat burn-in).
    let source_px = felis_grid::images::clip_source(image_w, image_h, source)
        .map_or((0, 0), |r| (r.width, r.height));
    let (cols, rows) = Extent::resolve(
        (req_cols, req_rows),
        source_px,
        (cell_pixel_w, cell_pixel_h),
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
    images.touch(image_id);
    let inherited = requested
        .and_then(|p| claim_placement_id(images, placements, events, image_id, p, false))
        .unwrap_or(0);
    let placement_id =
        Some(requested.unwrap_or_else(|| placements.allocate_anonymous_id(image_id)));
    let displaced = placements.upsert(Placement {
        image_id,
        placement_id,
        anchor,
        cols,
        rows,
        source,
        z_index,
        no_cursor_move,
        quiet,
        anonymous: requested.is_none(),
        serial: inherited,
    });
    match displaced {
        Some(Displaced::Replaced(_)) => {
            images.release(image_id);
        }
        Some(Displaced::Evicted(gone)) => drop_evicted(images, placements, events, gone),
        None => {}
    }
    refs.image_id = Some(image_id.0);
    if let Some(p) = requested {
        refs.placement_id = Some(p.0);
    }
    events.push(ImageEvent::Placement {
        image_id,
        placement_id,
        anchor_row: anchor.row,
        anchor_col: anchor.col,
        cols: cols.cells().get(),
        rows: rows.cells().get(),
        source,
        z_index,
        requested: felis_protocol::messages::RequestedExtent {
            cols: cols.requested(),
            rows: rows.requested(),
        },
        serial: placements.serial(image_id, placement_id).unwrap_or(0),
    });
    // The only Grid mutation the dispatcher makes
    // (docs/explanation/protocols/kitty-graphics.md "Dispatcher
    // architecture").
    grid.advance_cursor_after_image_placement(rows.cells(), cols.cells(), no_cursor_move);
    ActionOutcome::Ok
}

/// The placement a put pushed over the table's cap, announced as a
/// delete would announce it.
fn drop_evicted(
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    events: &mut Vec<ImageEvent>,
    gone: AnyPlacement,
) {
    let image_id = match gone {
        AnyPlacement::Anchored(p) => {
            images.release(p.image_id);
            events.push(ImageEvent::PlacementRemoved {
                image_id: p.image_id,
                placement_id: p.placement_id,
            });
            p.image_id
        }
        AnyPlacement::Virtual(v) => {
            events.push(virtual_removed_event(&v));
            v.image_id
        }
    };
    let unreferenced = images.get(image_id).is_some_and(ImageEntry::is_anonymous)
        && !placements.references(image_id);
    if unreferenced {
        free_image(images, placements, events, image_id);
    }
}

/// kitty's `handle_put_command` defaults each key on its own: `x`/`y`
/// to 0, an absent or zero `w`/`h` to the image's size. Clipping to what
/// is left past `x`/`y` is [`felis_grid::images::clip_source`]'s.
fn source_rect(
    complete: &CompleteCommand,
    image_width: u32,
    image_height: u32,
) -> Option<felis_protocol::messages::SourceRect> {
    let [x, y, w, h] = b"xywh".map(|key| control_u32(complete, key));
    if [x, y, w, h].iter().all(Option::is_none) {
        return None;
    }
    Some(felis_protocol::messages::SourceRect {
        x: x.unwrap_or(0),
        y: y.unwrap_or(0),
        width: w.filter(|&w| w != 0).unwrap_or(image_width),
        height: h.filter(|&h| h != 0).unwrap_or(image_height),
    })
}

const fn decode_error_to_outcome(err: DecodeError) -> ActionOutcome {
    match err {
        DecodeError::InvalidValue(m) => ActionOutcome::error(ErrorCode::InvalidValue, m),
        DecodeError::Unsupported(m) => ActionOutcome::error(ErrorCode::Unsupported, m),
        DecodeError::NoData(m) => ActionOutcome::error(ErrorCode::NoData, m),
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
    let id = match resolve_image_id(complete, refs, images) {
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
        let frame = Frame {
            pixels: canvas.into(),
            gap_ms: gap,
        };
        return match images.replace_frame(id, idx, frame) {
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
    let frame = Frame {
        pixels: canvas.into(),
        gap_ms: gap_from_z(z),
    };
    match images.push_frame(id, frame) {
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
    let id = match resolve_image_id(complete, refs, images) {
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
    let id = match resolve_image_id(complete, refs, images) {
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
    let frame = Frame {
        pixels: dst_px.into(),
        gap_ms: dst_gap,
    };
    match images.replace_frame(id, dst_idx, frame) {
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
    let id = match resolve_image_id(complete, refs, images) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    let Some(frame_count) = images.get(id).map(ImageEntry::frame_count) else {
        return ActionOutcome::error(ErrorCode::NotFound, "d=f: image not in store");
    };
    if frame_count <= 1 {
        if free_image_if_no_frames {
            delete_image_placements(images, placements, events, complete, id, true);
            return ActionOutcome::Ok;
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

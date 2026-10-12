//! Pins the `ImageMsg` stream the serve loop forwards to the client
//! per Kitty graphics action (`snapshot_kitty_graphics_dispatcher`
//! pins what the dispatcher returns to the producer). `drain_events`
//! runs the session task's materialization, so the assertions cover
//! the marker → store → wire path. No `pool::Session`, no PTY.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_daemon::graphics::{
    ApcCtx, ImageEvent, SavedScreens, ShmDeferral, apply_screen_switch, clear_images,
    dispatch_apc_body, materialize_image_events,
};
use felis_grid::images::{ImageStore, Placements};
use felis_grid::{Grid, PtyEffect};
use felis_protocol::{
    ImageId, PlacementId,
    messages::{ImageFormat, ImageMsg, ImageTarget},
};
use felis_vt::Parser;
use felis_vt::kitty_graphics::Reassembler;

mod common;
use common::{b64, body};

const TEST_STORE_CAP: usize = 1024 * 1024;

struct State {
    reassembler: Reassembler,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    /// Mirror of `Session::saved_screens`.
    saved: SavedScreens,
    events: Vec<ImageEvent>,
    shm: ShmDeferral,
}

impl State {
    fn new() -> Self {
        Self {
            reassembler: Reassembler::new(),
            grid: Grid::new(24, 80),
            images: ImageStore::new(TEST_STORE_CAP),
            placements: Placements::new(),
            saved: SavedScreens::default(),
            events: Vec::new(),
            shm: ShmDeferral::default(),
        }
    }

    fn dispatch(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        dispatch_apc_body(
            &mut ApcCtx {
                grid: &mut self.grid,
                images: &mut self.images,
                placements: &mut self.placements,
                events: &mut self.events,
                shm: &mut self.shm,
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &mut self.reassembler,
            body,
        )
    }

    fn drain_events(&mut self) -> Vec<ImageMsg> {
        let events = std::mem::take(&mut self.events);
        materialize_image_events(&events, &self.images)
    }

    fn advance_and_drain(&mut self, bytes: &[u8]) {
        let mut parser = Parser::new();
        parser.advance(&mut self.grid, bytes);
        for effect in self.grid.take_pty_effects() {
            match effect {
                PtyEffect::ScreenSwitch(switch) => apply_screen_switch(
                    switch,
                    &mut self.images,
                    &mut self.placements,
                    &mut self.saved,
                    &mut self.events,
                ),
                PtyEffect::ImagesCleared(scope) => clear_images(
                    &mut self.images,
                    &mut self.placements,
                    &mut self.events,
                    scope,
                ),
                PtyEffect::HardReset if self.saved.primary.is_some() => {
                    apply_screen_switch(
                        felis_grid::ScreenSwitch::LeftAlternate { kept: false },
                        &mut self.images,
                        &mut self.placements,
                        &mut self.saved,
                        &mut self.events,
                    );
                }
                _ => {}
            }
        }
    }
}

fn tiny_rgba_payload() -> Vec<u8> {
    b64(&[0xAA, 0xBB, 0xCC, 0xDD])
}

#[test]
fn action_t_emits_header_chunk_complete_only() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=t,i=7,f=32,s=1,v=1", &payload))
        .unwrap();
    let events = s.drain_events();
    assert_eq!(events.len(), 3, "Header + Chunk + Complete, got {events:?}");
    match &events[0] {
        ImageMsg::Header {
            id,
            target:
                ImageTarget::New {
                    width,
                    height,
                    format,
                },
        } => {
            assert_eq!(id.0, 7);
            assert_eq!(*width, 1);
            assert_eq!(*height, 1);
            assert_eq!(*format, ImageFormat::Rgba32);
        }
        m => panic!("expected Header, got {m:?}"),
    }
    match &events[1] {
        ImageMsg::Chunk { id, bytes } => {
            assert_eq!(id.0, 7);
            assert_eq!(bytes[..], [0xAA, 0xBB, 0xCC, 0xDD]);
        }
        m => panic!("expected Chunk, got {m:?}"),
    }
    assert!(matches!(events[2], ImageMsg::Complete { id: ImageId(7) }));
}

#[test]
fn action_uppercase_t_emits_header_chunk_complete_then_placement() {
    // The placement trails the transmission so the client's mirror
    // holds the bytes before any placement references the id.
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=11,f=32,s=1,v=1", &payload))
        .unwrap();
    let events = s.drain_events();
    assert_eq!(
        events.len(),
        4,
        "Header + Chunk + Complete + Placement, got {events:?}"
    );
    assert!(matches!(
        events[0],
        ImageMsg::Header {
            id: ImageId(11),
            ..
        }
    ));
    assert!(matches!(
        events[1],
        ImageMsg::Chunk {
            id: ImageId(11),
            ..
        }
    ));
    assert!(matches!(events[2], ImageMsg::Complete { id: ImageId(11) }));
    match &events[3] {
        ImageMsg::Placement {
            image_id,
            placement_id,
            anchor_row,
            anchor_col,
            ..
        } => {
            assert_eq!(image_id.0, 11);
            assert!(
                placement_id.is_some(),
                "an id-less put still gets its own id"
            );
            // Anchors are 1-based on the wire.
            assert_eq!(*anchor_row, 1);
            assert_eq!(*anchor_col, 1);
        }
        m => panic!("expected Placement, got {m:?}"),
    }
}

#[test]
fn unicode_placeholder_transmit_emits_virtual_placement_not_a_cursor_placement() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,U=1,i=12,f=32,s=1,v=1,c=4,r=2,z=0", &payload))
        .unwrap();
    let events = s.drain_events();
    assert_eq!(
        events.len(),
        4,
        "Header + Chunk + Complete + VirtualPlacement, got {events:?}"
    );
    assert!(matches!(
        events[0],
        ImageMsg::Header {
            id: ImageId(12),
            ..
        }
    ));
    assert!(matches!(
        events[1],
        ImageMsg::Chunk {
            id: ImageId(12),
            ..
        }
    ));
    assert!(matches!(events[2], ImageMsg::Complete { id: ImageId(12) }));
    match &events[3] {
        ImageMsg::VirtualPlacement {
            image_id,
            cols,
            rows,
            z_index,
            ..
        } => {
            assert_eq!(image_id.0, 12);
            assert_eq!(*cols, 4);
            assert_eq!(*rows, 2);
            assert_eq!(*z_index, 0);
        }
        m => panic!("expected VirtualPlacement, got {m:?}"),
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ImageMsg::Placement { .. })),
        "U=1 must not emit a cursor Placement",
    );
    assert_eq!(s.placements.len(), 0, "no anchored placement recorded");
    // Rehydrate replays the extent from this table; the placeholder
    // cells alone carry no size.
    let recorded: Vec<_> = s.placements.iter_virtual().collect();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].image_id, ImageId(12));
    assert_eq!((recorded[0].cols, recorded[0].rows), (4, 2));
}

/// kitty treats `a=p,U=1` like `a=T,U=1` (`graphics.c`
/// `handle_put_command`): a virtual placement, and the cursor stays.
#[test]
fn unicode_placeholder_put_of_a_stored_image_is_virtual_and_keeps_the_cursor() {
    let mut s = State::new();
    s.dispatch(&body("Ga=t,i=12,f=32,s=1,v=1", &tiny_rgba_payload()))
        .unwrap();
    s.drain_events();
    s.dispatch(&body("Ga=p,U=1,i=12,c=4,r=2", b"")).unwrap();
    let events = s.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ImageMsg::VirtualPlacement { .. })),
        "got {events:?}"
    );
    assert_eq!(s.placements.len(), 0, "no anchored placement recorded");
    let cur = s.grid.cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
}

#[test]
fn virtual_extent_dies_with_its_image() {
    // A surviving extent would rehydrate a tile size for a missing image.
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,U=1,i=12,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    assert!(s.dispatch(&body("Ga=d,d=I,i=12", b"")).is_none());
    let events = s.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ImageMsg::Delete { id: ImageId(12) })),
        "free path must announce the delete, got {events:?}"
    );
    assert_eq!(s.placements.iter_virtual().count(), 0);
}

#[test]
fn removing_one_virtual_placement_tells_the_client_its_id() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,U=1,i=12,p=3,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    assert!(s.dispatch(&body("Ga=d,d=i,i=12,p=3", b"")).is_none());
    assert_eq!(
        s.drain_events(),
        vec![ImageMsg::PlacementRemoved {
            image_id: ImageId(12),
            placement_id: Some(PlacementId(3)),
        }]
    );
}

#[test]
fn alt_screen_round_trip_restores_and_restates_virtual_extents() {
    // The restore re-states a fresh `VirtualPlacement` so a client
    // that attached mid-alt learns the tile size.
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,U=1,i=12,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    s.advance_and_drain(b"\x1b[?1049h");
    assert_eq!(
        s.placements.iter_virtual().count(),
        0,
        "alt screen starts with no virtual extents"
    );
    let entered = s.drain_events();
    assert!(
        entered.iter().any(|e| matches!(
            e,
            ImageMsg::PlacementRemoved {
                image_id: ImageId(12),
                placement_id: Some(_),
            }
        )),
        "the client drops the stashed extent too, got {entered:?}"
    );
    s.advance_and_drain(b"\x1b[?1049l");
    let events = s.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ImageMsg::VirtualPlacement {
                image_id: ImageId(12),
                cols: 4,
                rows: 2,
                ..
            }
        )),
        "restore must re-state the extent, got {events:?}"
    );
    assert_eq!(s.placements.iter_virtual().count(), 1);
}

#[test]
fn a_delete_on_the_alternate_screen_spares_a_primary_virtual_placement() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,U=1,i=12,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    s.advance_and_drain(b"\x1b[?1049h");
    assert!(s.dispatch(&body("Ga=d,d=I,i=12", b"")).is_none());
    s.advance_and_drain(b"\x1b[?1049l");
    let events = s.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ImageMsg::VirtualPlacement {
                image_id: ImageId(12),
                ..
            }
        )),
        "the extent comes back with the primary screen, got {events:?}"
    );
    assert_eq!(s.placements.iter_virtual().count(), 1);
}

#[test]
fn action_p_emits_only_placement_when_image_already_stored() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=t,i=99,f=32,s=1,v=1", &payload))
        .unwrap();
    s.drain_events(); // discard the transmit events
    s.dispatch(&body("Ga=p,i=99,p=3,z=-2", b"")).unwrap();
    let events = s.drain_events();
    assert_eq!(events.len(), 1, "only Placement, got {events:?}");
    match &events[0] {
        ImageMsg::Placement {
            image_id,
            placement_id,
            z_index,
            ..
        } => {
            assert_eq!(image_id.0, 99);
            assert_eq!(*placement_id, Some(PlacementId(3)));
            assert_eq!(*z_index, -2);
        }
        m => panic!("expected Placement, got {m:?}"),
    }
}

#[test]
fn action_d_default_emits_placement_removed_only() {
    // A bare `a=d` defaults to `d=a` (kitty parity; mpv sends it).
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=1,f=32,s=1,v=1", &payload))
        .unwrap();
    s.drain_events();
    assert!(
        s.dispatch(&body("Ga=d,i=1", b"")).is_none(),
        "deletes are not acknowledged (kitty parity)",
    );
    let events = s.drain_events();
    assert_eq!(events.len(), 1, "PlacementRemoved only, got {events:?}");
    match &events[0] {
        ImageMsg::PlacementRemoved {
            image_id,
            placement_id,
        } => {
            assert_eq!(image_id.0, 1);
            assert!(placement_id.is_some());
        }
        m => panic!("expected PlacementRemoved, got {m:?}"),
    }
}

#[test]
fn action_d_capital_emits_placement_removed_then_delete() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=42,f=32,s=1,v=1", &payload))
        .unwrap();
    s.drain_events();
    assert!(s.dispatch(&body("Ga=d,d=I,i=42", b"")).is_none());
    let events = s.drain_events();
    assert_eq!(events.len(), 2, "PlacementRemoved + Delete, got {events:?}");
    assert!(matches!(
        events[0],
        ImageMsg::PlacementRemoved {
            image_id: ImageId(42),
            ..
        }
    ));
    assert!(matches!(events[1], ImageMsg::Delete { id: ImageId(42) }));
}

#[test]
fn action_d_a_clears_every_placement_no_delete() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    for id in 1..=3u32 {
        let ctrls = format!("Ga=T,i={id},f=32,s=1,v=1");
        s.dispatch(&body(&ctrls, &payload)).unwrap();
    }
    s.drain_events();
    assert!(s.dispatch(&body("Ga=d,d=a", b"")).is_none());
    let events = s.drain_events();
    assert_eq!(events.len(), 3, "one PlacementRemoved per id");
    let ids: Vec<u32> = events
        .iter()
        .filter_map(|m| match m {
            ImageMsg::PlacementRemoved { image_id, .. } => Some(image_id.0),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn action_d_capital_a_clears_placements_and_deletes_every_image() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    for id in 1..=2u32 {
        let ctrls = format!("Ga=T,i={id},f=32,s=1,v=1");
        s.dispatch(&body(&ctrls, &payload)).unwrap();
    }
    s.drain_events();
    assert!(s.dispatch(&body("Ga=d,d=A", b"")).is_none());
    let events = s.drain_events();
    assert_eq!(events.len(), 4);
    let kinds: Vec<&'static str> = events
        .iter()
        .map(|m| match m {
            ImageMsg::PlacementRemoved { .. } => "rem",
            ImageMsg::Delete { .. } => "del",
            _ => "other",
        })
        .collect();
    // PlacementRemoved first, so the client un-binds the draw before
    // the bytes vanish.
    assert_eq!(kinds, vec!["rem", "rem", "del", "del"]);
}

#[test]
fn action_q_emits_nothing_into_image_outbox() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=q,i=1,f=32,s=1,v=1", &payload))
        .unwrap();
    assert!(
        s.drain_events().is_empty(),
        "a=q must not emit any image events",
    );
}

#[test]
fn failed_transmit_emits_nothing_into_image_outbox() {
    let mut s = State::new();
    // 2 bytes against the 4 that `f=32,s=1,v=1` promises: EINVAL.
    let bad_payload = b64(&[0xAA, 0xBB]);
    s.dispatch(&body("Ga=t,i=2,f=32,s=1,v=1", &bad_payload))
        .unwrap();
    assert!(
        s.drain_events().is_empty(),
        "failed transmit must not emit events",
    );
}

#[test]
fn replace_under_same_id_emits_a_fresh_header_chunk_complete() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=t,i=5,f=32,s=1,v=1", &payload))
        .unwrap();
    s.drain_events();
    let bigger = b64(&[0; 8]);
    s.dispatch(&body("Ga=t,i=5,f=32,s=2,v=1", &bigger)).unwrap();
    let events = s.drain_events();
    assert_eq!(events.len(), 3, "Header + Chunk + Complete on replace");
    match &events[0] {
        ImageMsg::Header {
            id,
            target: ImageTarget::New { width, height, .. },
        } => {
            assert_eq!(id.0, 5);
            assert_eq!(*width, 2);
            assert_eq!(*height, 1, "the geometry is the byte count");
        }
        m => panic!("expected Header on replace, got {m:?}"),
    }
}

/// Without erase-evicts-placements, `clear` / `Ctrl+L` leaves the
/// previous yazi / mdcat image painted over the cleared screen.
fn removed_and_deleted(events: &[ImageMsg]) -> (Vec<u32>, Vec<u32>) {
    let mut removed = Vec::new();
    let mut deleted = Vec::new();
    for m in events {
        match m {
            ImageMsg::PlacementRemoved { image_id, .. } => removed.push(image_id.0),
            ImageMsg::Delete { id } => deleted.push(id.0),
            _ => {}
        }
    }
    removed.sort_unstable();
    deleted.sort_unstable();
    (removed, deleted)
}

/// kitty's `grman_clear`, here through the selective `DECSED 2`: every
/// placement on screen goes, `C=1` included, and then every image
/// nothing references.
#[test]
fn ed_2_clears_the_screen_placements_and_frees_unreferenced_images() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=42,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.dispatch(&body("Ga=T,i=99,C=1,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.dispatch(&body("Ga=t,i=5,f=32,s=1,v=1", &payload))
        .unwrap();
    s.dispatch(&body("Ga=t,i=6,f=32,s=1,v=1", &payload))
        .unwrap();
    s.dispatch(&body("Ga=p,U=1,i=6,c=2,r=1", b"")).unwrap();
    s.drain_events();

    s.advance_and_drain(b"\x1b[?2J");

    let (removed, deleted) = removed_and_deleted(&s.drain_events());
    assert_eq!(removed, vec![42, 99]);
    assert_eq!(
        deleted,
        vec![5, 42, 99],
        "the virtual placement keeps image 6"
    );
    assert_eq!(s.placements.len(), 0);
    assert_eq!(s.placements.iter_virtual().count(), 1);
}

#[test]
fn partial_erases_and_a_soft_reset_leave_images_alone() {
    let mut s = State::new();
    s.dispatch(&body(
        "Ga=T,i=42,f=32,s=1,v=1,c=4,r=2",
        &tiny_rgba_payload(),
    ))
    .unwrap();
    s.drain_events();

    s.advance_and_drain(b"\x1b[H\x1b[J\x1b[1J\x1b[2K\x1b[!p");

    assert_eq!(s.drain_events(), Vec::new());
    assert_eq!(s.placements.len(), 1);
}

#[test]
fn ed_3_also_clears_scrollback_placements() {
    let mut s = State::new();
    s.dispatch(&body(
        "Ga=T,i=42,f=32,s=1,v=1,c=4,r=2",
        &tiny_rgba_payload(),
    ))
    .unwrap();
    s.placements.shift_up(30, 1000);
    s.drain_events();

    s.advance_and_drain(b"\x1b[2J");
    assert_eq!(
        s.placements.len(),
        1,
        "ED 2 spares a placement wholly in history"
    );

    s.advance_and_drain(b"\x1b[3J");
    let (removed, deleted) = removed_and_deleted(&s.drain_events());
    assert_eq!((removed, deleted), (vec![42], vec![42]));
}

#[test]
fn ris_clears_the_screen_placements_and_frees_their_images() {
    let mut s = State::new();
    s.dispatch(&body(
        "Ga=T,i=7,C=1,f=32,s=1,v=1,c=4,r=2",
        &tiny_rgba_payload(),
    ))
    .unwrap();
    s.drain_events();

    s.advance_and_drain(b"\x1bc");

    let (removed, deleted) = removed_and_deleted(&s.drain_events());
    assert_eq!((removed, deleted), (vec![7], vec![7]));
}

/// Nothing can place an image sent without an id again, so kitty frees
/// it with its last placement; a named image stays for the next put.
#[test]
fn an_image_without_an_id_goes_with_its_last_placement() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,q=2,f=32,s=1,v=1,c=4,r=2", &payload));
    s.dispatch(&body("Ga=T,i=42,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    let anonymous = s.images.iter_ids().find(|id| *id != ImageId(42)).unwrap();
    s.drain_events();

    s.dispatch(&body("Ga=d,d=a", b""));

    let (removed, deleted) = removed_and_deleted(&s.drain_events());
    let mut expected_removed = vec![anonymous.0, 42];
    expected_removed.sort_unstable();
    assert_eq!(removed, expected_removed);
    assert_eq!(deleted, vec![anonymous.0]);
    assert!(s.images.get(ImageId(42)).is_some());
}

/// kitty's RIS clears the primary screen's live rows and keeps its
/// history; leaving the alternate screen on the way changes nothing.
#[test]
fn ris_on_the_alternate_screen_keeps_primary_scrollback_placements() {
    let mut s = State::new();
    s.dispatch(&body(
        "Ga=T,i=42,f=32,s=1,v=1,c=4,r=2",
        &tiny_rgba_payload(),
    ))
    .unwrap();
    s.placements.shift_up(30, 1000);
    s.dispatch(&body(
        "Ga=T,i=43,f=32,s=1,v=1,c=4,r=2",
        &tiny_rgba_payload(),
    ))
    .unwrap();
    s.advance_and_drain(b"\x1b[?1049h");
    s.dispatch(&body(
        "Ga=T,i=44,f=32,s=1,v=1,c=4,r=2",
        &tiny_rgba_payload(),
    ))
    .unwrap();
    s.drain_events();

    s.advance_and_drain(b"\x1bc");

    assert_eq!(s.saved.primary, None);
    assert_eq!(
        s.placements
            .iter()
            .map(|p| p.image_id.0)
            .collect::<Vec<_>>(),
        vec![42]
    );
    let mut stored: Vec<u32> = s.images.iter_ids().map(|id| id.0).collect();
    stored.sort_unstable();
    assert_eq!(stored, vec![42]);
}

/// kitty keeps the primary screen's images in a store of its own, so a
/// full-screen program clearing the alternate screen cannot free them.
#[test]
fn a_clear_on_the_alternate_screen_keeps_the_primary_screen_images() {
    let mut s = State::new();
    s.dispatch(&body(
        "Ga=T,i=42,f=32,s=1,v=1,c=4,r=2",
        &tiny_rgba_payload(),
    ))
    .unwrap();
    s.advance_and_drain(b"\x1b[?1049h\x1b[2J");
    s.drain_events();
    let saved = s.saved.primary.as_ref().unwrap();
    assert!(saved.images.get(ImageId(42)).is_some());

    s.advance_and_drain(b"\x1b[?1049l");
    assert_eq!(s.placements.len(), 1);
}

/// `?1049h` stashes every primary placement and emits a
/// `PlacementRemoved` per entry; full-screen TUIs would otherwise
/// show primary-screen images painted on top.
#[test]
fn alt_screen_entry_hides_primary_placements() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=11,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    assert_eq!(s.placements.len(), 1, "primary placement landed");
    let primary_refcount = s.images.get(ImageId(11)).unwrap().refcount();
    assert!(primary_refcount >= 1, "primary placement holds a refcount");

    s.advance_and_drain(b"\x1b[?1049h");

    assert!(
        s.placements.is_empty(),
        "alt-screen entry must clear the live placements table",
    );
    assert!(
        s.saved.primary.is_some(),
        "the primary placement must be stashed for restore",
    );
    let saved = s.saved.primary.as_ref().unwrap();
    assert_eq!(saved.placements.len(), 1, "stashed primary placement count");
    assert_eq!(
        saved.images.get(ImageId(11)).unwrap().refcount(),
        primary_refcount,
        "the image moves to the stash with the placement that pins it",
    );
    assert!(
        s.images.is_empty(),
        "the alternate screen starts with no images"
    );
    let events = s.drain_events();
    assert!(
        matches!(
            &events[..],
            [
                ImageMsg::PlacementRemoved {
                    image_id: ImageId(11),
                    ..
                },
                ImageMsg::Delete { id: ImageId(11) },
            ]
        ),
        "the placement goes before its image, got {events:?}",
    );
}

/// `?1049l` restores the saved primary placements and emits a
/// `Placement` per restored entry.
#[test]
fn alt_screen_leave_restores_primary_placements() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=22,f=32,s=1,v=1,c=3,r=4", &payload))
        .unwrap();
    s.drain_events();
    let primary_anchor = s.placements.iter().next().unwrap().anchor;

    s.advance_and_drain(b"\x1b[?1049h");
    s.drain_events();

    s.advance_and_drain(b"\x1b[?1049l");

    assert_eq!(s.placements.len(), 1, "primary placement restored");
    assert!(
        s.saved.primary.is_none(),
        "save slot must be empty after restore",
    );
    let restored = s.placements.iter().next().unwrap();
    assert_eq!(restored.image_id.0, 22);
    assert_eq!(
        restored.anchor, primary_anchor,
        "anchor preserved across toggle"
    );
    let events = s.drain_events();
    assert!(
        events.iter().any(|m| matches!(
            m,
            ImageMsg::Placement {
                image_id: ImageId(22),
                ..
            }
        )),
        "expected Placement for image 22 on alt leave, got {events:?}",
    );
    assert!(
        events.iter().any(|m| matches!(
            m,
            ImageMsg::Header {
                id: ImageId(22),
                target: ImageTarget::New { .. },
            }
        )),
        "the window deleted image 22 on alt entry, so leaving re-sends it: {events:?}",
    );
}

/// Placements created on the alternate screen die on `?1049l` (xterm
/// convention: the alt screen has no persistent state).
#[test]
fn alt_screen_placements_are_destroyed_on_leave() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=1,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    let primary_refs = s.images.get(ImageId(1)).unwrap().refcount();

    s.advance_and_drain(b"\x1b[?1049h");
    s.drain_events();

    s.dispatch(&body("Ga=T,i=2,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    assert_eq!(
        s.placements.len(),
        1,
        "alt-only placement in the live table"
    );
    let alt_refs = s.images.get(ImageId(2)).unwrap().refcount();
    assert!(alt_refs >= 1, "alt placement pins image 2");

    s.advance_and_drain(b"\x1b[?1049l");

    let ids: Vec<u32> = s.placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(ids, vec![1], "only the primary placement survives");
    assert_eq!(
        s.images.get(ImageId(1)).unwrap().refcount(),
        primary_refs,
        "primary refcount unchanged across the round-trip",
    );
    assert!(
        s.images.get(ImageId(2)).is_none(),
        "image 2 left with the alternate screen's store",
    );
}

/// A re-entered alt screen starts with an empty live table. The
/// seeded primary placement (image 9) gives the assertion a second
/// way to fail: a second entry that forgot to stash it.
#[test]
fn second_alt_screen_entry_starts_with_no_alt_placements() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=9,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    assert_eq!(s.placements.len(), 1, "primary placement seeded");

    s.advance_and_drain(b"\x1b[?1049h");
    s.drain_events();
    s.dispatch(&body("Ga=T,i=42,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    assert_eq!(s.placements.len(), 1);
    s.advance_and_drain(b"\x1b[?1049l");
    s.drain_events();
    let ids: Vec<u32> = s.placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(ids, vec![9], "primary restored before the second entry");

    s.advance_and_drain(b"\x1b[?1049h");

    assert!(
        s.placements.is_empty(),
        "second alt entry starts fresh — got {:?}",
        s.placements.iter().collect::<Vec<_>>(),
    );
    assert_eq!(
        s.saved.primary.as_ref().map(|saved| saved
            .placements
            .iter()
            .map(|p| p.image_id.0)
            .collect::<Vec<_>>()),
        Some(vec![9]),
        "the second entry stashes the primary set it displaced",
    );
}

/// A redundant `?1049h` on alt is a no-op; double-stashing would
/// overwrite the saved primary set with the empty alt table.
#[test]
fn redundant_alt_screen_entry_does_not_overwrite_saved_primary() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=7,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();

    s.advance_and_drain(b"\x1b[?1049h\x1b[?1049h");
    s.drain_events();
    assert!(s.saved.primary.is_some());
    assert_eq!(s.saved.primary.as_ref().unwrap().placements.len(), 1);

    s.advance_and_drain(b"\x1b[?1049l");
    assert_eq!(
        s.placements.len(),
        1,
        "primary survives the redundant-entry path"
    );
}

#[test]
fn a_range_delete_on_the_alternate_screen_keeps_the_primary_screen_images() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=12,f=32,s=1,v=1,q=2", &payload));
    s.advance_and_drain(b"\x1b[?1049h");
    s.dispatch(&body("Ga=d,d=R,x=1,y=20", b""));
    assert!(
        s.saved
            .primary
            .as_ref()
            .unwrap()
            .images
            .get(ImageId(12))
            .is_some()
    );
    s.advance_and_drain(b"\x1b[?1049l");
    assert_eq!(s.placements.len(), 1);
}

/// kitty's stores are per screen: an alternate-screen image reusing a
/// primary id is a different image, and leaving restores the primary one.
#[test]
fn an_alternate_screen_image_does_not_replace_a_primary_one_with_its_id() {
    let mut s = State::new();
    s.dispatch(&body(
        "Ga=T,i=5,f=32,s=1,v=1,c=4,r=2,q=2",
        &b64(&[1, 1, 1, 1]),
    ));
    s.advance_and_drain(b"\x1b[?1049h");
    s.dispatch(&body(
        "Ga=T,i=5,f=32,s=1,v=1,c=4,r=2,q=2",
        &b64(&[2, 2, 2, 2]),
    ));
    s.advance_and_drain(b"\x1b[?1049l");

    let entry = s.images.get(ImageId(5)).unwrap();
    assert_eq!(&entry.frame(0).unwrap().pixels[..], &[1, 1, 1, 1]);
    assert_eq!(s.placements.len(), 1);
}

/// kitty keeps the alternate screen's images with the buffer `?47l`
/// keeps, so `?47h` brings both back.
#[test]
fn a_47_reentry_brings_back_the_alternate_screen_images() {
    let mut s = State::new();
    s.dispatch(&body(
        "Ga=T,i=1,f=32,s=1,v=1,c=4,r=2,q=2",
        &tiny_rgba_payload(),
    ));
    s.advance_and_drain(b"\x1b[?47h");
    s.dispatch(&body(
        "Ga=T,i=2,f=32,s=1,v=1,c=4,r=2,q=2",
        &tiny_rgba_payload(),
    ));
    s.advance_and_drain(b"\x1b[?47l");
    s.drain_events();

    s.advance_and_drain(b"\x1b[?47h");

    assert_eq!(
        s.placements
            .iter()
            .map(|p| p.image_id.0)
            .collect::<Vec<_>>(),
        vec![2]
    );
    let events = s.drain_events();
    assert!(
        events.iter().any(|m| matches!(
            m,
            ImageMsg::Placement {
                image_id: ImageId(2),
                ..
            }
        )),
        "the window gets the alternate placement back: {events:?}",
    );
}

/// `?1047l` discards the alternate buffer, and its images with it.
#[test]
fn a_1047_leave_discards_the_alternate_screen_images() {
    let mut s = State::new();
    s.advance_and_drain(b"\x1b[?1047h");
    s.dispatch(&body(
        "Ga=T,i=2,f=32,s=1,v=1,c=4,r=2,q=2",
        &tiny_rgba_payload(),
    ));
    s.advance_and_drain(b"\x1b[?1047l\x1b[?47h");

    assert!(s.placements.is_empty());
    assert!(s.images.is_empty());
}

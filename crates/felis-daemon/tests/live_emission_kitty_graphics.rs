//! Pins the `ImageMsg` stream the serve loop forwards to the client
//! per Kitty graphics action (`snapshot_kitty_graphics_dispatcher`
//! pins what the dispatcher returns to the producer). `drain_events`
//! runs the session task's materialization, so the assertions cover
//! the marker → store → wire path. No `pool::Session`, no PTY.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_daemon::graphics::{
    ApcCtx, ImageEvent, ShmDeferral, apply_screen_switch, dispatch_apc_body,
    materialize_image_events,
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
    /// Mirror of `Session::saved_primary_placements`.
    saved_primary_placements: Option<Placements>,
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
            saved_primary_placements: None,
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
            if let PtyEffect::ScreenSwitch(switch) = effect {
                apply_screen_switch(
                    switch,
                    &mut self.images,
                    &mut self.placements,
                    &mut self.saved_primary_placements,
                    &mut self.events,
                );
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
            assert_eq!(*placement_id, None);
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

#[test]
fn virtual_extent_dies_with_its_image() {
    // A virtual placement has no `PlacementRemoved` of its own; a
    // surviving extent would rehydrate a tile size for a missing image.
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
fn alt_screen_restore_sweeps_virtual_extent_of_a_freed_image() {
    // The free-path purge sweeps only the live table, so the restore
    // is where a stashed extent of a freed image must be dropped.
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
        !events
            .iter()
            .any(|e| matches!(e, ImageMsg::VirtualPlacement { .. })),
        "no extent may be re-stated for a freed image, got {events:?}"
    );
    assert_eq!(s.placements.iter_virtual().count(), 0);
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
            assert_eq!(*placement_id, None);
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
#[test]
fn ed_2_clears_placements_without_c_1() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=42,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    let initial = s.drain_events();
    assert!(
        initial.iter().any(|m| matches!(
            m,
            ImageMsg::Placement {
                image_id: ImageId(42),
                ..
            }
        )),
        "expected initial Placement to land, got {initial:?}",
    );

    let mut parser = Parser::new();
    parser.advance(&mut s.grid, b"\x1b[2J");

    let ranges: Vec<_> = s
        .grid
        .take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Erased(r) => Some(r),
            _ => None,
        })
        .collect();
    assert!(
        !ranges.is_empty(),
        "ED 2 must push at least one ErasedRange"
    );
    for r in ranges {
        let removed = s.placements.remove_intersecting(r.top, r.bottom, r.force);
        for p in removed {
            s.images.release(p.image_id);
            s.events.push(ImageEvent::PlacementRemoved {
                image_id: p.image_id,
                placement_id: p.placement_id,
            });
        }
    }

    let events = s.drain_events();
    assert!(
        events.iter().any(|m| matches!(
            m,
            ImageMsg::PlacementRemoved {
                image_id: ImageId(42),
                ..
            }
        )),
        "expected PlacementRemoved for image 42 after ED 2, got {events:?}",
    );
    assert!(s.placements.is_empty(), "placement must be evicted");
}

/// `C=1` opts out of erase-eviction; a pinned status-line image would
/// otherwise vanish on every `clear`.
#[test]
fn ed_2_preserves_placements_with_c_1() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=99,C=1,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();

    let mut parser = Parser::new();
    parser.advance(&mut s.grid, b"\x1b[2J");

    let ranges: Vec<_> = s
        .grid
        .take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Erased(r) => Some(r),
            _ => None,
        })
        .collect();
    for r in ranges {
        let removed = s.placements.remove_intersecting(r.top, r.bottom, r.force);
        for p in removed {
            s.images.release(p.image_id);
            s.events.push(ImageEvent::PlacementRemoved {
                image_id: p.image_id,
                placement_id: p.placement_id,
            });
        }
    }

    let events = s.drain_events();
    assert!(
        events.is_empty(),
        "C=1 placement must survive ED 2; got events {events:?}",
    );
    assert_eq!(s.placements.len(), 1, "placement must be retained");
}

/// RIS (`ESC c`) wipes every placement, `C=1` included.
#[test]
fn ris_wipes_placements_including_c_1() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=7,C=1,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    assert_eq!(s.placements.len(), 1);

    let mut parser = Parser::new();
    parser.advance(&mut s.grid, b"\x1bc");

    let ranges: Vec<_> = s
        .grid
        .take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Erased(r) => Some(r),
            _ => None,
        })
        .collect();
    assert!(!ranges.is_empty(), "RIS must push a forced ErasedRange");
    for r in ranges {
        let removed = s.placements.remove_intersecting(r.top, r.bottom, r.force);
        for p in removed {
            s.images.release(p.image_id);
            s.events.push(ImageEvent::PlacementRemoved {
                image_id: p.image_id,
                placement_id: p.placement_id,
            });
        }
    }

    assert!(s.placements.is_empty(), "RIS must clear every placement");
    let events = s.drain_events();
    assert!(
        events.iter().any(|m| matches!(
            m,
            ImageMsg::PlacementRemoved {
                image_id: ImageId(7),
                ..
            }
        )),
        "expected PlacementRemoved for image 7, got {events:?}",
    );
}

/// DECSTR (`CSI ! p`) wipes every placement, `C=1` included; the
/// spec gives the soft reset no exemption.
#[test]
fn decstr_wipes_placements_including_c_1() {
    let mut s = State::new();
    let payload = tiny_rgba_payload();
    s.dispatch(&body("Ga=T,i=8,C=1,f=32,s=1,v=1,c=4,r=2", &payload))
        .unwrap();
    s.drain_events();
    assert_eq!(s.placements.len(), 1);

    let mut parser = Parser::new();
    parser.advance(&mut s.grid, b"\x1b[!p");

    let ranges: Vec<_> = s
        .grid
        .take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Erased(r) => Some(r),
            _ => None,
        })
        .collect();
    assert!(!ranges.is_empty(), "DECSTR must push a forced ErasedRange");
    for r in ranges {
        let removed = s.placements.remove_intersecting(r.top, r.bottom, r.force);
        for p in removed {
            s.images.release(p.image_id);
            s.events.push(ImageEvent::PlacementRemoved {
                image_id: p.image_id,
                placement_id: p.placement_id,
            });
        }
    }

    assert!(s.placements.is_empty(), "DECSTR must clear every placement");
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
        s.saved_primary_placements.is_some(),
        "the primary placement must be stashed for restore",
    );
    let saved = s.saved_primary_placements.as_ref().unwrap();
    assert_eq!(saved.len(), 1, "stashed primary placement count");
    // Save releases the refcount, symmetric with retain on restore;
    // see `apply_screen_switch` for why the symmetry matters.
    assert_eq!(
        s.images.get(ImageId(11)).unwrap().refcount(),
        primary_refcount - 1,
        "save releases the refcount; image bytes still in store",
    );
    let events = s.drain_events();
    assert!(
        events.iter().any(|m| matches!(
            m,
            ImageMsg::PlacementRemoved {
                image_id: ImageId(11),
                ..
            }
        )),
        "expected PlacementRemoved for image 11 on alt entry, got {events:?}",
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
        s.saved_primary_placements.is_none(),
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
    assert_eq!(
        s.images.get(ImageId(2)).unwrap().refcount(),
        alt_refs - 1,
        "alt placement release decrements image 2 refcount",
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
        s.saved_primary_placements
            .as_ref()
            .map(|saved| saved.iter().map(|p| p.image_id.0).collect::<Vec<_>>()),
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
    assert!(s.saved_primary_placements.is_some());
    assert_eq!(s.saved_primary_placements.as_ref().unwrap().len(), 1);

    s.advance_and_drain(b"\x1b[?1049l");
    assert_eq!(
        s.placements.len(),
        1,
        "primary survives the redundant-entry path"
    );
}

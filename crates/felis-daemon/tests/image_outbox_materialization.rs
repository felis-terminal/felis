//! Pins what the marker indirection of the image outbox (the
//! dispatcher queues [`ImageEvent`]s, the session task reads pixels
//! back from the store at fan-out) must preserve: a drained batch
//! leaves the client's [`ImageShadow`] agreeing with the store, and a
//! marker whose image the same batch evicted ships nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_client_core::image_shadow::ImageShadow;
use felis_daemon::graphics::{
    ApcCtx, ImageEvent, ShmDeferral, dispatch_apc_body, materialize_image_events,
};
use felis_grid::Grid;
use felis_grid::images::{ImageId, ImageStore, Placements};
use felis_vt::kitty_graphics::Reassembler;

mod common;
use common::{b64, body};

const ROOMY_CAP: usize = 1024 * 1024;

struct State {
    reassembler: Reassembler,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    events: Vec<ImageEvent>,
    shm: ShmDeferral,
}

impl State {
    fn with_cap(cap: usize) -> Self {
        Self {
            reassembler: Reassembler::new(),
            grid: Grid::new(24, 80),
            images: ImageStore::new(cap),
            placements: Placements::new(),
            events: Vec::new(),
            shm: ShmDeferral::default(),
        }
    }

    fn dispatch(&mut self, body: &[u8]) {
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
        );
    }

    fn ship(&mut self, shadow: &mut ImageShadow) -> Vec<felis_protocol::messages::ImageMsg> {
        let events = std::mem::take(&mut self.events);
        let wire = materialize_image_events(&events, &self.images);
        for msg in &wire {
            // The daemon's own emissions are inside every protocol
            // limit by construction; a refusal here is the interesting
            // failure, not something to swallow.
            shadow
                .apply(msg)
                .expect("daemon emission the mirror admits");
        }
        wire
    }
}

fn transmit(s: &mut State, id: u32, width: usize, fill: u8) {
    let payload = b64(&vec![fill; width * 4]);
    s.dispatch(&body(
        &format!("Ga=t,i={id},f=32,s={width},v=1,q=2"),
        &payload,
    ));
}

#[test]
fn a_shipped_batch_leaves_the_mirror_agreeing_with_the_store() {
    let mut s = State::with_cap(ROOMY_CAP);
    let mut shadow = ImageShadow::new();

    transmit(&mut s, 1, 4, 0x11);
    transmit(&mut s, 1, 4, 0x22);
    transmit(&mut s, 2, 2, 0x33);
    s.dispatch(&body("Ga=f,i=2,f=32,s=2,v=1,z=60,q=2", &b64(&[0x44; 8])));
    s.dispatch(&body("Ga=d,d=f,i=2,r=2,q=2", &[]));

    s.ship(&mut shadow);

    // The per-id loop is vacuous on an empty store.
    assert!(!s.images.is_empty(), "fixture must leave images to check");
    for id in s.images.iter_ids() {
        let entry = s.images.get(id).unwrap();
        let mirrored = shadow
            .image(id)
            .unwrap_or_else(|| panic!("store holds {id:?} but the mirror does not"));
        assert_eq!(mirrored.width, entry.width, "{id:?} width");
        assert_eq!(mirrored.height, entry.height, "{id:?} height");
        assert_eq!(mirrored.frames.len(), entry.frame_count(), "{id:?} frames");
        assert_eq!(
            mirrored.current_frame(),
            entry.current_frame(),
            "{id:?} displayed frame",
        );
        assert_eq!(mirrored.pixels(), entry.pixels(), "{id:?} pixels");
    }
}

#[test]
fn a_superseded_transmission_ships_once_with_the_latest_pixels() {
    // mpv's `--vo=kitty` overwrites one image id per video frame;
    // materializing every marker would ship that many copies.
    let mut s = State::with_cap(ROOMY_CAP);
    let mut shadow = ImageShadow::new();
    for fill in [0xA1, 0xA2, 0xA3, 0xA4] {
        transmit(&mut s, 1, 4, fill);
    }
    let wire = s.ship(&mut shadow);

    let headers = wire
        .iter()
        .filter(|m| matches!(m, felis_protocol::messages::ImageMsg::Header { .. }))
        .count();
    assert_eq!(
        headers, 1,
        "four transmissions collapse to one, got {wire:?}"
    );
    assert_eq!(shadow.image(ImageId(1)).unwrap().pixels(), &[0xA4; 16]);
}

#[test]
fn a_placement_between_two_transmissions_survives_the_collapse() {
    // Collapsing moves the placement ahead of any pixels for its
    // image; `ImageShadow::apply_placement` must not require the image.
    let mut s = State::with_cap(ROOMY_CAP);
    let mut shadow = ImageShadow::new();
    // Re-transmitting deletes the first placement, as in kitty; the one
    // made after it must still land.
    transmit(&mut s, 1, 4, 0x11);
    s.dispatch(&body("Ga=p,i=1,c=1,r=1,q=2", &[]));
    transmit(&mut s, 1, 4, 0x22);
    s.dispatch(&body("Ga=p,i=1,c=2,r=1,q=2", &[]));
    s.ship(&mut shadow);

    assert_eq!(
        shadow.placements().len(),
        1,
        "the later placement must outlive the collapsed transmission",
    );
    assert_eq!(shadow.placements()[0].cols, 2);
    assert_eq!(shadow.placements()[0].image_id, ImageId(1));
    assert_eq!(shadow.image(ImageId(1)).unwrap().pixels(), &[0x22; 16]);
}

#[test]
fn an_image_evicted_later_in_the_batch_ships_nothing() {
    // The cap fits one image, so the second transmission evicts the
    // first while its marker is still queued.
    let mut s = State::with_cap(300);
    let mut shadow = ImageShadow::new();
    transmit(&mut s, 1, 16, 0x11);
    s.dispatch(&body("Ga=p,i=1,c=1,r=1,q=2", &[]));
    transmit(&mut s, 2, 16, 0x22);

    assert!(
        s.images.get(ImageId(1)).is_none(),
        "the cap must have evicted image 1 for this test to mean anything",
    );
    s.ship(&mut shadow);

    assert!(shadow.image(ImageId(1)).is_none(), "no stale image 1");
    assert!(shadow.image(ImageId(2)).is_some(), "image 2 arrives");
    assert!(
        shadow.placements().iter().all(|p| p.image_id != ImageId(1)),
        "the evicted image must not keep a ghost placement",
    );
}

#[test]
fn an_edit_of_an_earlier_frame_ships_in_frame_order() {
    // Pins: one drain can carry two appends and then an edit of the
    // first of them. The mirror appends and refuses a frame number past
    // its count, so the edit's marker must not be emitted after the
    // later append's.
    let mut s = State::with_cap(ROOMY_CAP);
    let mut shadow = ImageShadow::new();
    transmit(&mut s, 1, 2, 0x11);
    s.ship(&mut shadow);

    s.dispatch(&body("Ga=f,i=1,f=32,s=2,v=1,z=60,q=2", &b64(&[0x22; 8])));
    s.dispatch(&body("Ga=f,i=1,f=32,s=2,v=1,z=60,q=2", &b64(&[0x33; 8])));
    s.dispatch(&body("Ga=f,i=1,r=2,f=32,s=2,v=1,q=2", &b64(&[0x44; 8])));
    assert_eq!(
        s.images.get(ImageId(1)).unwrap().frame_count(),
        3,
        "fixture must leave two appended frames, the first of them edited",
    );

    s.ship(&mut shadow);

    let entry = s.images.get(ImageId(1)).unwrap();
    let mirrored = shadow.image(ImageId(1)).unwrap();
    assert_eq!(mirrored.frames.len(), entry.frame_count());
    for (i, frame) in mirrored.frames.iter().enumerate() {
        assert_eq!(
            frame.pixels,
            entry.frame(i).unwrap().pixels,
            "frame {i} must mirror the store, edit included",
        );
    }
}

#[test]
fn a_frame_jump_in_the_same_batch_lands_the_mirror_on_the_live_frame() {
    // The transmission marker materializes after the `a=a,c=` jump, so
    // its resync must carry the store's current index, not the root.
    let mut s = State::with_cap(ROOMY_CAP);
    let mut shadow = ImageShadow::new();
    transmit(&mut s, 1, 2, 0x11);
    s.dispatch(&body("Ga=f,i=1,f=32,s=2,v=1,z=60,q=2", &b64(&[0x22; 8])));
    s.dispatch(&body("Ga=a,i=1,c=2,q=2", &[]));

    let entry_frame = s.images.get(ImageId(1)).unwrap().current_frame();
    assert_eq!(entry_frame, 1, "c=2 selects the 0-based index 1");
    s.ship(&mut shadow);

    assert_eq!(
        shadow.image(ImageId(1)).unwrap().current_frame(),
        entry_frame,
        "the mirror must display the frame the store displays",
    );
}

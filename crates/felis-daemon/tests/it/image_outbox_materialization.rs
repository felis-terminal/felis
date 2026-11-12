//! Pins what the marker indirection of the image outbox (the
//! dispatcher queues [`ImageEvent`]s, the session task reads pixels
//! back from the store at fan-out) must preserve: a drained batch
//! leaves the client's [`ImageShadow`] agreeing with the store, and a
//! marker whose image the same batch evicted ships nothing.

use felis_client_core::image_shadow::ImageShadow;
use felis_daemon::graphics::{
    ApcCtx, ImageEvent, ShmDeferral, dispatch_apc_body, materialize_image_events,
};
use felis_grid::Grid;
use felis_grid::images::{ImageId, ImageStore, Placements};
use felis_grid::kitty_graphics::Reassembler;
use felis_protocol::messages::{ImageMsg, ImageTarget, SourceRect};

use crate::common::{b64, body};

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

    fn ship(&mut self, shadow: &mut ImageShadow) -> Vec<ImageMsg> {
        self.ship_at(true, shadow)
    }

    fn ship_at(&mut self, regions: bool, shadow: &mut ImageShadow) -> Vec<ImageMsg> {
        let events = std::mem::take(&mut self.events);
        let wire = materialize_image_events(&events, &self.images, regions);
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
        .filter(|m| matches!(m, ImageMsg::Header { .. }))
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

    let placed: Vec<_> = shadow.placements().map(|p| (p.image_id, p.cols)).collect();
    assert_eq!(
        placed,
        [(ImageId(1), 2)],
        "the later placement must outlive the collapsed transmission",
    );
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
        shadow.placements().all(|p| p.image_id != ImageId(1)),
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

/// An 8×4 RGBA image whose pixel `i` is `[i; 4]`, shipped to `shadows`.
fn pattern_image(s: &mut State, shadows: &mut [&mut ImageShadow]) {
    let pixels: Vec<u8> = (0..32u8).flat_map(|i| [i; 4]).collect();
    s.dispatch(&body("Ga=t,i=1,f=32,s=8,v=4,q=2", &b64(&pixels)));
    let events = std::mem::take(&mut s.events);
    for shadow in shadows {
        for msg in materialize_image_events(&events, &s.images, true) {
            shadow.apply(&msg).unwrap();
        }
    }
}

fn edit(s: &mut State, frame: u32, rect: (u32, u32, u32, u32), fill: u8) {
    let (x, y, width, height) = rect;
    s.dispatch(&body(
        &format!("Ga=f,i=1,r={frame},x={x},y={y},s={width},v={height},f=32,X=1,q=2"),
        &b64(&vec![fill; (width * height * 4) as usize]),
    ));
}

fn targets(wire: &[ImageMsg]) -> Vec<ImageTarget> {
    wire.iter()
        .filter_map(|m| match m {
            ImageMsg::Header { target, .. } => Some(*target),
            _ => None,
        })
        .collect()
}

fn chunk_bytes(wire: &[ImageMsg]) -> Vec<u8> {
    wire.iter()
        .filter_map(|m| match m {
            ImageMsg::Chunk { bytes, .. } => Some(&bytes[..]),
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

const fn region(x: u32, y: u32, width: u32, height: u32) -> ImageTarget {
    ImageTarget::Region {
        number: std::num::NonZeroU32::MIN,
        rect: SourceRect {
            x,
            y,
            width,
            height,
        },
    }
}

const ROOT: ImageTarget = ImageTarget::Frame {
    number: std::num::NonZeroU32::MIN,
};

fn root_pixels(s: &State) -> Vec<u8> {
    s.images
        .get(ImageId(1))
        .unwrap()
        .frame(0)
        .unwrap()
        .pixels
        .to_vec()
}

#[test]
fn an_edit_ships_its_rectangle_alone_and_the_whole_frame_to_a_window_without_regions() {
    let mut s = State::with_cap(ROOMY_CAP);
    let (mut current, mut whole) = (ImageShadow::new(), ImageShadow::new());
    pattern_image(&mut s, &mut [&mut current, &mut whole]);
    edit(&mut s, 1, (2, 1, 3, 2), 0xEE);
    let events = s.events.clone();

    let wire = s.ship(&mut current);
    assert_eq!(targets(&wire), [region(2, 1, 3, 2)]);
    let store = root_pixels(&s);
    let rows: Vec<u8> = [1usize, 2]
        .iter()
        .flat_map(|y| store[(y * 8 + 2) * 4..(y * 8 + 5) * 4].to_vec())
        .collect();
    assert_eq!(chunk_bytes(&wire), rows, "exactly the rectangle's rows");
    assert_eq!(current.image(ImageId(1)).unwrap().pixels(), store);

    s.events = events;
    let wire = s.ship_at(false, &mut whole);
    assert_eq!(targets(&wire), [ROOT]);
    assert_eq!(chunk_bytes(&wire), store);
    assert_eq!(whole.image(ImageId(1)).unwrap().pixels(), store);
}

#[test]
fn edits_of_one_frame_in_a_batch_ship_once_each_and_a_covered_one_not_at_all() {
    let mut s = State::with_cap(ROOMY_CAP);
    let mut shadow = ImageShadow::new();
    pattern_image(&mut s, &mut [&mut shadow]);
    edit(&mut s, 1, (0, 0, 1, 1), 0x01);
    edit(&mut s, 1, (6, 3, 2, 1), 0x02);
    edit(&mut s, 1, (0, 0, 2, 1), 0x03);
    edit(&mut s, 1, (1, 0, 1, 1), 0x04);

    let wire = s.ship(&mut shadow);
    assert_eq!(targets(&wire), [region(6, 3, 2, 1), region(0, 0, 2, 1)]);
    assert_eq!(shadow.image(ImageId(1)).unwrap().pixels(), root_pixels(&s));
}

#[test]
fn edits_past_the_rectangle_cap_ship_as_their_bounding_box() {
    let mut s = State::with_cap(ROOMY_CAP);
    let mut shadow = ImageShadow::new();
    pattern_image(&mut s, &mut [&mut shadow]);
    for k in 0..8 {
        edit(&mut s, 1, (k, 0, 1, 1), 0x10);
        edit(&mut s, 1, (k, 1, 1, 1), 0x20);
    }
    assert_eq!(
        targets(&s.ship(&mut shadow)).len(),
        16,
        "at the cap each ships"
    );
    for k in 0..8 {
        edit(&mut s, 1, (k, 0, 1, 1), 0x10);
        edit(&mut s, 1, (k, 1, 1, 1), 0x20);
    }
    edit(&mut s, 1, (0, 2, 1, 1), 0x30);
    let wire = s.ship(&mut shadow);
    assert_eq!(targets(&wire), [ROOT], "an 8x3 box is past half the frame");
    assert_eq!(shadow.image(ImageId(1)).unwrap().pixels(), root_pixels(&s));
}

#[test]
fn edits_over_half_the_frame_ship_it_whole() {
    let mut s = State::with_cap(ROOMY_CAP);
    let mut shadow = ImageShadow::new();
    pattern_image(&mut s, &mut [&mut shadow]);
    edit(&mut s, 1, (0, 0, 8, 2), 0x55);
    assert_eq!(
        targets(&s.ship(&mut shadow)),
        [region(0, 0, 8, 2)],
        "half ships as a region"
    );
    edit(&mut s, 1, (0, 0, 8, 2), 0x66);
    edit(&mut s, 1, (0, 3, 1, 1), 0x66);
    assert_eq!(targets(&s.ship(&mut shadow)), [ROOT]);
    assert_eq!(shadow.image(ImageId(1)).unwrap().pixels(), root_pixels(&s));
}

#[test]
fn a_whole_frame_or_image_in_the_same_batch_supersedes_its_edits() {
    let mut s = State::with_cap(ROOMY_CAP);
    let mut shadow = ImageShadow::new();
    pattern_image(&mut s, &mut [&mut shadow]);
    s.dispatch(&body("Ga=f,i=1,f=32,s=8,v=4,q=2", &b64(&[0x77; 128])));
    edit(&mut s, 2, (1, 1, 1, 1), 0x78);
    let two = ImageTarget::Frame {
        number: std::num::NonZeroU32::new(2).unwrap(),
    };
    assert_eq!(
        targets(&s.ship(&mut shadow)),
        [two],
        "an append carries its edits"
    );

    edit(&mut s, 1, (1, 1, 1, 1), 0x79);
    s.dispatch(&body("Ga=d,d=f,i=1,r=2,q=2", &[]));
    let wire = s.ship(&mut shadow);
    assert!(
        targets(&wire)
            .iter()
            .all(|t| !matches!(t, ImageTarget::Region { .. })),
        "a restate re-ships every frame, got {wire:?}"
    );
    let held = shadow.image(ImageId(1)).unwrap();
    assert_eq!(held.frames.len(), 1);
    assert_eq!(held.pixels(), root_pixels(&s));
}

#[derive(Debug, Clone)]
enum PlaceOp {
    Put(Option<u32>),
    Delete(Option<u32>),
}

fn place_op() -> impl proptest::strategy::Strategy<Value = PlaceOp> {
    use proptest::prelude::*;
    // `u32::MAX` is the first id an id-less put is given.
    let p = proptest::option::of(prop_oneof![1u32..4, Just(u32::MAX)]);
    prop_oneof![
        4 => p.clone().prop_map(PlaceOp::Put),
        1 => p.prop_map(PlaceOp::Delete),
    ]
}

proptest::proptest! {
    /// The mirror holds the daemon's placements in the daemon's order,
    /// which is the renderer's tiebreak among equal z-indices, after any
    /// mix of id-less and named puts, a named one landing on an id-less
    /// placement's id included.
    #[test]
    fn the_mirror_keeps_the_daemons_placement_order(
        ops in proptest::collection::vec(place_op(), 0..24),
    ) {
        let mut s = State::with_cap(ROOMY_CAP);
        let mut shadow = ImageShadow::new();
        transmit(&mut s, 1, 4, 0x11);
        for op in ops {
            let (action, p) = match op {
                PlaceOp::Put(p) => ("a=p,i=1,c=1,r=1", p),
                PlaceOp::Delete(p) => ("a=d,d=i,i=1", p),
            };
            let p = p.map_or(String::new(), |p| format!(",p={p}"));
            s.dispatch(&body(&format!("G{action}{p},q=2"), &[]));
            s.ship(&mut shadow);
            let daemon: Vec<_> = s.placements.iter().map(|p| p.placement_id.map(|id| id.0)).collect();
            let mirror: Vec<_> = shadow.placements().map(|p| p.placement_id.map(|id| id.0)).collect();
            proptest::prop_assert_eq!(daemon, mirror);
        }
    }
}

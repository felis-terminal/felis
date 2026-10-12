//! Pins the dispatcher's response bytes and post-dispatch state per
//! Kitty graphics command, bucketed as in
//! `docs/reference/protocols/kitty-graphics.md` (action, transmission
//! medium, format, `q=` gating, error mapping).
//! `felis-vt::tests::snapshot_kitty_graphics` pins the parser side.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_daemon::graphics::{ApcCtx, ShmDeferral, dispatch_apc_body};
use felis_grid::Grid;
use felis_grid::images::{ImageId, ImageStore, Placements};
use felis_vt::kitty_graphics::Reassembler;

mod common;
use common::{b64, body};

const TEST_STORE_CAP: usize = 1024 * 1024;

struct State {
    reassembler: Reassembler,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    shm: ShmDeferral,
}

impl State {
    fn new() -> Self {
        Self {
            reassembler: Reassembler::new(),
            grid: Grid::new(24, 80),
            images: ImageStore::new(TEST_STORE_CAP),
            placements: Placements::new(),
            shm: ShmDeferral::default(),
        }
    }

    fn dispatch(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        dispatch_apc_body(
            &mut ApcCtx {
                grid: &mut self.grid,
                images: &mut self.images,
                placements: &mut self.placements,
                events: &mut Vec::new(),
                shm: &mut self.shm,
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &mut self.reassembler,
            body,
        )
    }
}

const TEST_PIXEL: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];

#[test]
fn action_q_validates_without_storing() {
    let mut s = State::new();
    let response = s
        .dispatch(&body("Ga=q,i=1,f=32,s=1,v=1", &b64(&TEST_PIXEL)))
        .unwrap();
    assert_eq!(response, b"\x1b_Gi=1;OK\x1b\\");
    assert!(s.images.is_empty(), "a=q must not touch the store");
}

#[test]
fn action_t_transmits_and_stores_without_placement() {
    let mut s = State::new();
    let response = s
        .dispatch(&body("Ga=t,i=2,f=32,s=1,v=1", &b64(&TEST_PIXEL)))
        .unwrap();
    assert_eq!(response, b"\x1b_Gi=2;OK\x1b\\");
    let entry = s.images.get(ImageId(2)).expect("stored");
    assert_eq!(entry.pixels(), TEST_PIXEL);
    assert!(s.placements.is_empty(), "a=t records no placement");
}

#[test]
fn action_uppercase_t_transmits_and_records_placement_and_advances_cursor() {
    let mut s = State::new();
    let response = s
        .dispatch(&body("Ga=T,i=3,f=32,s=1,v=1,c=2,r=2", &b64(&TEST_PIXEL)))
        .unwrap();
    assert_eq!(response, b"\x1b_Gi=3;OK\x1b\\");
    assert_eq!(s.placements.len(), 1, "a=T records one placement");
    let cur = s.grid.cursor();
    assert_eq!(
        (cur.row, cur.col),
        (1, 2),
        "cursor lands right of the image on its last row (r=2, c=2)",
    );
}

#[test]
fn action_p_displays_previously_transmitted_image() {
    let mut s = State::new();
    s.dispatch(&body("Ga=t,i=4,f=32,s=1,v=1", &b64(&TEST_PIXEL)))
        .unwrap();
    let response = s.dispatch(&body("Ga=p,i=4,z=2", b"")).unwrap();
    assert_eq!(response, b"\x1b_Gi=4;OK\x1b\\");
    assert_eq!(s.placements.len(), 1);
    assert_eq!(s.placements.iter().next().unwrap().z_index, 2);
}

#[test]
fn action_p_for_unknown_image_returns_enoent() {
    let mut s = State::new();
    let response = s.dispatch(&body("Ga=p,i=99", b"")).unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=99;ENOENT"),
        "got: {response:?}",
    );
    assert!(s.placements.is_empty());
}

#[test]
fn action_d_with_d_eq_i_drops_placement_but_keeps_image() {
    let mut s = State::new();
    s.dispatch(&body("Ga=T,i=5,f=32,s=1,v=1", &b64(&TEST_PIXEL)))
        .unwrap();
    assert!(s.dispatch(&body("Ga=d,d=i,i=5", b"")).is_none());
    assert!(s.placements.is_empty());
    assert!(s.images.get(ImageId(5)).is_some(), "image survives");
}

#[test]
fn a_query_is_answered_with_its_image_id_alone() {
    let mut s = State::new();
    let q = |keys: &str| format!("Ga=q,{keys},f=32,s=1,v=1");
    assert_eq!(
        s.dispatch(&body(&q("i=17,p=2"), &b64(&TEST_PIXEL))),
        Some(b"\x1b_Gi=17;OK\x1b\\".to_vec())
    );
    assert_eq!(s.dispatch(&body(&q("I=4"), &b64(&TEST_PIXEL))), None);
}

#[test]
fn a_command_with_both_image_id_and_number_is_refused_before_it_runs() {
    let mut s = State::new();
    s.dispatch(&body("Ga=T,i=5,f=32,s=1,v=1", &b64(&TEST_PIXEL)))
        .unwrap();
    let reply = s.dispatch(&body("Ga=d,d=i,i=5,I=9", b"")).unwrap();
    assert!(reply.starts_with(b"\x1b_Gi=5,I=9;EINVAL"), "got: {reply:?}");
    assert!(!s.placements.is_empty(), "the delete did not run");
    assert_eq!(s.dispatch(&body("Ga=d,d=i,i=5,I=9,q=2", b"")), None);
}

#[test]
fn a_zero_id_key_is_not_echoed() {
    let mut s = State::new();
    assert_eq!(
        s.dispatch(&body("Ga=t,i=7,I=0,f=32,s=1,v=1", &b64(&TEST_PIXEL))),
        Some(b"\x1b_Gi=7;OK\x1b\\".to_vec())
    );
}

#[test]
fn a_failed_delete_sends_no_reply() {
    let mut s = State::new();
    assert_eq!(s.dispatch(&body("Ga=d,d=?,i=5", b"")), None);
}

#[test]
fn an_error_without_an_image_id_sends_no_reply() {
    let mut s = State::new();
    assert_eq!(
        s.dispatch(&body("Ga=t,f=64,s=1,v=1", &b64(&TEST_PIXEL))),
        None
    );
    assert_eq!(s.dispatch(&body("Ga=p,i=0", b"")), None);
}

#[test]
fn action_d_with_d_eq_capital_a_clears_everything() {
    let mut s = State::new();
    for id in [6u32, 7, 8] {
        let id_str = id.to_string();
        let payload = b64(&TEST_PIXEL);
        let controls = format!("Ga=T,i={id_str},f=32,s=1,v=1");
        s.dispatch(&body(&controls, &payload)).unwrap();
    }
    assert!(s.dispatch(&body("Ga=d,d=A", b"")).is_none());
    assert!(s.placements.is_empty());
    assert!(s.images.is_empty());
}

#[test]
fn transmission_d_direct_base64_payload() {
    let mut s = State::new();
    let response = s
        .dispatch(&body("Ga=t,t=d,i=10,f=32,s=1,v=1", &b64(&TEST_PIXEL)))
        .unwrap();
    assert_eq!(response, b"\x1b_Gi=10;OK\x1b\\");
    assert_eq!(s.images.get(ImageId(10)).unwrap().pixels(), TEST_PIXEL);
}

#[test]
#[cfg(target_os = "linux")]
fn transmission_s_shared_memory_round_trips() {
    // mpv's --vo-kitty-use-shm shape: a slash-less shm name as payload.
    use std::io::Write;
    let name = format!("/felis-snap-shm-{}", std::process::id());
    let fd = rustix::shm::open(
        &name,
        rustix::shm::OFlags::CREATE | rustix::shm::OFlags::EXCL | rustix::shm::OFlags::RDWR,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .expect("create test shm object");
    std::fs::File::from(fd)
        .write_all(&TEST_PIXEL)
        .expect("fill test shm object");
    let mut s = State::new();
    let response = s
        .dispatch(&body(
            "Ga=t,t=s,i=11,f=32,s=1,v=1",
            &b64(name.trim_start_matches('/').as_bytes()),
        ))
        .unwrap();
    assert_eq!(response, b"\x1b_Gi=11;OK\x1b\\");
    assert_eq!(s.images.get(ImageId(11)).unwrap().pixels(), TEST_PIXEL);
    // `t=s` defers the unlink to session teardown (a per-read unlink
    // gives mpv a black bottom half), so the test unlinks its own.
    let _ = rustix::shm::unlink(&name);
}

#[test]
fn format_24_raw_rgb_round_trips() {
    let mut s = State::new();
    let pixels = [0x11, 0x22, 0x33];
    s.dispatch(&body("Ga=t,t=d,i=20,f=24,s=1,v=1", &b64(&pixels)))
        .unwrap();
    let entry = s.images.get(ImageId(20)).expect("stored");
    assert_eq!(entry.pixels(), pixels);
    assert_eq!(entry.format, felis_protocol::messages::ImageFormat::Rgb24);
}

#[test]
fn format_32_raw_rgba_round_trips() {
    let mut s = State::new();
    s.dispatch(&body("Ga=t,t=d,i=21,f=32,s=1,v=1", &b64(&TEST_PIXEL)))
        .unwrap();
    let entry = s.images.get(ImageId(21)).expect("stored");
    assert_eq!(entry.pixels(), TEST_PIXEL);
    assert_eq!(entry.format, felis_protocol::messages::ImageFormat::Rgba32);
}

#[test]
fn format_100_png_round_trips_to_rgba8() {
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, 1, 1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&TEST_PIXEL).expect("png data");
    writer.finish().expect("png finish");

    let mut s = State::new();
    s.dispatch(&body("Ga=t,t=d,i=22,f=100", &b64(&png)))
        .unwrap();
    let entry = s.images.get(ImageId(22)).expect("stored");
    assert_eq!(entry.format, felis_protocol::messages::ImageFormat::Rgba32);
    assert_eq!(&entry.pixels()[..4], &TEST_PIXEL);
}

#[test]
fn quiet_zero_emits_ok_and_errors() {
    let mut s = State::new();
    let ok = s
        .dispatch(&body("Ga=t,q=0,i=30,f=32,s=1,v=1", &b64(&TEST_PIXEL)))
        .unwrap();
    assert_eq!(ok, b"\x1b_Gi=30;OK\x1b\\");
    let err = s.dispatch(&body("Ga=p,q=0,i=999", b"")).unwrap();
    assert!(err.starts_with(b"\x1b_Gi=999;ENOENT"));
}

#[test]
fn quiet_one_suppresses_ok_keeps_errors() {
    let mut s = State::new();
    assert!(
        s.dispatch(&body("Ga=t,q=1,i=31,f=32,s=1,v=1", &b64(&TEST_PIXEL),))
            .is_none(),
        "q=1 OK suppressed",
    );
    let err = s.dispatch(&body("Ga=p,q=1,i=999", b"")).unwrap();
    assert!(err.starts_with(b"\x1b_Gi=999;ENOENT"));
}

#[test]
fn quiet_two_silences_everything_including_errors() {
    let mut s = State::new();
    assert!(
        s.dispatch(&body("Ga=t,q=2,i=32,f=32,s=1,v=1", &b64(&TEST_PIXEL),))
            .is_none(),
    );
    assert!(
        s.dispatch(&body("Ga=p,q=2,i=999", b"")).is_none(),
        "q=2 silences errors too",
    );
}

#[test]
fn missing_image_id_auto_allocates_without_a_reply() {
    // Valid per the Kitty spec (yazi's direct preview relies on it).
    let mut s = State::new();
    let response = s.dispatch(&body("Ga=t,f=32,s=1,v=1", &b64(&TEST_PIXEL)));
    assert_eq!(response, None);
}

#[test]
fn error_einval_on_malformed_base64() {
    let mut s = State::new();
    let response = s
        .dispatch(&body("Ga=t,i=40,f=32,s=1,v=1", b"!@#$NOT-B64"))
        .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=40;EINVAL"),
        "got: {response:?}",
    );
}

#[test]
fn error_einval_on_unknown_format() {
    let mut s = State::new();
    let response = s
        .dispatch(&body("Ga=t,i=41,f=64,s=1,v=1", &b64(&TEST_PIXEL)))
        .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=41;EINVAL"),
        "got: {response:?}",
    );
}

#[test]
fn animation_actions_are_routed_not_enotsup() {
    let mut s = State::new();
    for action in *b"fac" {
        let response = s
            .dispatch(&body(&format!("Ga={},i=42", action as char), b""))
            .unwrap();
        assert!(
            response.starts_with(b"\x1b_Gi=42;ENOENT"),
            "a={:?} against a missing image must report ENOENT (the \
             action is understood, the image is not there), got: {response:?}",
            action as char,
        );
    }
}

#[test]
fn chunked_transmission_assembles_across_three_bodies() {
    let mut s = State::new();
    let pixels = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
    let encoded = b64(&pixels);
    // Split on multiples of 4 so each chunk is whole base64 quanta.
    let third = (encoded.len() / 3 / 4) * 4;
    assert!(third > 0, "test invariant: payload long enough to split");
    let head_payload = &encoded[..third];
    let mid_payload = &encoded[third..2 * third];
    let tail_payload = &encoded[2 * third..];
    assert!(
        s.dispatch(&body("Ga=t,i=50,f=32,s=2,v=1,m=1", head_payload,))
            .is_none(),
        "head chunk pending",
    );
    assert!(
        s.dispatch(&body("Gm=1", mid_payload)).is_none(),
        "middle chunk pending",
    );
    let response = s
        .dispatch(&body("Gm=0", tail_payload))
        .expect("tail emits response");
    assert_eq!(response, b"\x1b_Gi=50;OK\x1b\\");
    assert_eq!(s.images.get(ImageId(50)).unwrap().pixels(), pixels);
}

/// `kitty_tests/graphics.py` `test_load_images`, the image-number part.
#[test]
fn each_numbered_transmission_gets_the_lowest_free_id() {
    let mut s = State::new();
    let t = |keys: &str| format!("Ga=t,{keys},f=32,s=1,v=1");
    let mut send = |keys: &str| s.dispatch(&body(&t(keys), &b64(&TEST_PIXEL))).unwrap();
    assert_eq!(send("I=1"), b"\x1b_Gi=1,I=1;OK\x1b\\");
    assert_eq!(send("I=1"), b"\x1b_Gi=2,I=1;OK\x1b\\");
    assert_eq!(send("I=1"), b"\x1b_Gi=3,I=1;OK\x1b\\");
    assert_eq!(send("i=5"), b"\x1b_Gi=5;OK\x1b\\");
    assert_eq!(send("I=3"), b"\x1b_Gi=4,I=3;OK\x1b\\");
    assert_eq!(send("I=3"), b"\x1b_Gi=6,I=3;OK\x1b\\");
}

#[test]
fn a_put_by_number_shows_the_newest_image_and_answers_with_its_id() {
    let mut s = State::new();
    for _ in 0..2 {
        s.dispatch(&body("Ga=t,I=93,f=32,s=1,v=1", &b64(&TEST_PIXEL)));
    }
    assert_eq!(
        s.dispatch(&body("Ga=p,I=93,c=2,r=2", b"")),
        Some(b"\x1b_Gi=2,I=93;OK\x1b\\".to_vec())
    );
    assert!(s.placements.for_image(ImageId(2)).next().is_some());
    let reply = s.dispatch(&body("Ga=p,I=94", b"")).unwrap();
    assert!(reply.starts_with(b"\x1b_GI=94;ENOENT"), "got: {reply:?}");
}

#[test]
fn a_number_delete_reaches_only_the_newest_image_with_it() {
    let mut s = State::new();
    for _ in 0..2 {
        s.dispatch(&body("Ga=T,I=7,f=32,s=1,v=1", &b64(&TEST_PIXEL)));
    }
    s.dispatch(&body("Ga=d,d=n,I=7", b""));
    assert!(s.placements.for_image(ImageId(2)).next().is_none());
    assert!(s.images.get(ImageId(2)).is_some(), "d=n keeps the image");
    s.dispatch(&body("Ga=d,d=N,I=7", b""));
    assert!(s.images.get(ImageId(2)).is_none(), "d=N frees it");
    assert!(s.placements.for_image(ImageId(1)).next().is_some());
    s.dispatch(&body("Ga=d,d=N,I=7", b""));
    assert!(s.images.get(ImageId(1)).is_none());
}

#[test]
fn an_id_delete_ignores_an_image_number() {
    let mut s = State::new();
    s.dispatch(&body("Ga=T,I=7,f=32,s=1,v=1", &b64(&TEST_PIXEL)));
    s.dispatch(&body("Ga=d,d=I,I=7", b""));
    assert!(s.images.get(ImageId(1)).is_some());
}

#[test]
fn sending_a_numbered_image_again_by_id_keeps_its_number() {
    let mut s = State::new();
    s.dispatch(&body("Ga=t,I=7,f=32,s=1,v=1", &b64(&TEST_PIXEL)));
    s.dispatch(&body("Ga=t,i=1,f=32,s=1,v=1", &b64(&TEST_PIXEL)));
    assert_eq!(s.images.newest_with_number(7), Some(ImageId(1)));
}

#[test]
fn sending_an_older_numbered_image_again_does_not_make_it_the_newest() {
    let mut s = State::new();
    for _ in 0..2 {
        s.dispatch(&body("Ga=t,I=7,f=32,s=1,v=1", &b64(&TEST_PIXEL)));
    }
    s.dispatch(&body("Ga=t,i=1,f=32,s=1,v=1", &b64(&TEST_PIXEL)));
    assert_eq!(s.images.newest_with_number(7), Some(ImageId(2)));
}

#[test]
fn a_frame_reply_echoes_the_frame_it_edited() {
    let mut s = State::new();
    s.dispatch(&body("Ga=t,i=5,f=32,s=1,v=1", &b64(&TEST_PIXEL)));
    assert_eq!(
        s.dispatch(&body("Ga=f,i=5,r=1,f=32,s=1,v=1", &b64(&TEST_PIXEL))),
        Some(b"\x1b_Gi=5,r=1;OK\x1b\\".to_vec())
    );
    assert_eq!(
        s.dispatch(&body("Ga=p,i=5,r=1", b"")),
        Some(b"\x1b_Gi=5;OK\x1b\\".to_vec())
    );
}

fn virtual_ids(s: &State) -> Vec<(u32, Option<u32>)> {
    s.placements
        .iter_virtual()
        .map(|v| (v.image_id.0, v.placement_id.map(|p| p.0)))
        .collect()
}

fn with_two_virtual_placements() -> State {
    let mut s = State::new();
    s.dispatch(&body("Ga=t,i=7,f=32,s=1,v=1,q=2", &b64(&TEST_PIXEL)));
    s.dispatch(&body("Ga=p,U=1,i=7,p=1,c=2,r=1,q=2", b""));
    s.dispatch(&body("Ga=p,U=1,i=7,p=2,c=4,r=2,q=2", b""));
    s
}

#[test]
fn virtual_placements_of_one_image_are_kept_apart_by_placement_id() {
    let s = with_two_virtual_placements();
    assert_eq!(virtual_ids(&s), vec![(7, Some(1)), (7, Some(2))]);
}

#[test]
fn an_id_delete_with_a_placement_id_removes_that_virtual_placement() {
    let mut s = with_two_virtual_placements();
    s.dispatch(&body("Ga=d,d=i,i=7,p=1", b""));
    assert_eq!(virtual_ids(&s), vec![(7, Some(2))]);
}

#[test]
fn an_uppercase_id_delete_frees_an_image_shown_only_through_placeholders() {
    let mut s = with_two_virtual_placements();
    s.dispatch(&body("Ga=d,d=I,i=7,p=1", b""));
    assert!(
        s.images.get(ImageId(7)).is_some(),
        "placement 2 still shows it"
    );
    s.dispatch(&body("Ga=d,d=I,i=7", b""));
    assert_eq!(virtual_ids(&s), vec![]);
    assert!(s.images.get(ImageId(7)).is_none());
}

#[test]
fn sending_an_image_again_drops_its_virtual_placements() {
    let mut s = with_two_virtual_placements();
    s.dispatch(&body("Ga=t,i=7,f=32,s=1,v=1,q=2", &b64(&TEST_PIXEL)));
    assert_eq!(virtual_ids(&s), vec![]);
}

#[test]
fn a_screen_delete_leaves_virtual_placements() {
    let mut s = with_two_virtual_placements();
    s.dispatch(&body("Ga=d,d=A", b""));
    assert_eq!(virtual_ids(&s), vec![(7, Some(1)), (7, Some(2))]);
}

#[test]
fn a_put_replaces_the_placement_of_the_other_kind_under_its_id() {
    let mut s = with_two_virtual_placements();
    s.dispatch(&body("Ga=p,i=7,p=1,q=2", b""));
    assert_eq!(virtual_ids(&s), vec![(7, Some(2))]);
    assert_eq!(s.placements.len(), 1);
    s.dispatch(&body("Ga=p,U=1,i=7,p=1,q=2", b""));
    assert_eq!(s.placements.len(), 0);
    assert_eq!(virtual_ids(&s), vec![(7, Some(2)), (7, Some(1))]);
}

#[test]
fn each_virtual_put_without_a_placement_id_is_a_placement_of_its_own() {
    let mut s = State::new();
    s.dispatch(&body("Ga=t,i=7,f=32,s=1,v=1,q=2", &b64(&TEST_PIXEL)));
    s.dispatch(&body("Ga=p,U=1,i=7,q=2", b""));
    s.dispatch(&body("Ga=p,U=1,i=7,q=2", b""));
    assert_eq!(s.placements.iter_virtual().count(), 2);
}

#[test]
fn a_range_delete_skips_an_image_sent_without_an_id() {
    let mut s = State::new();
    s.dispatch(&body("Ga=T,U=1,f=32,s=1,v=1,q=2", &b64(&TEST_PIXEL)));
    s.dispatch(&body(&format!("Ga=d,d=R,x=1,y={}", u32::MAX), b""));
    assert_eq!(s.placements.iter_virtual().count(), 1);
}

//! Pins the `docs/explanation/security-model.md` "Kitty graphics"
//! promises. Overlap with `graphics::tests` is deliberate: the
//! security pins live in one file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
use std::os::unix::fs::symlink;

use felis_daemon::graphics::{ApcCtx, MAX_DECODED_BYTES, ShmDeferral, dispatch_apc_body};
use felis_grid::Grid;
use felis_grid::images::{ImageEntry, ImageFormat, ImageId, ImageStore, Placements};
use felis_vt::kitty_graphics::Reassembler;

mod common;
use common::{b64, body};

const SECURITY_TEST_STORE_CAP: usize = 4 * 1024;

struct State {
    reassembler: Reassembler,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    shm: ShmDeferral,
}

impl State {
    fn new(store_cap: usize) -> Self {
        Self {
            reassembler: Reassembler::new(),
            grid: Grid::new(24, 80),
            images: ImageStore::new(store_cap),
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

#[test]
fn raw_dimensions_past_per_image_cap_reject_before_allocation() {
    // 8000 × 8000 RGBA = 256 MiB.
    let mut s = State::new(SECURITY_TEST_STORE_CAP);
    let response = s
        .dispatch(&body("Ga=t,i=1,f=32,s=8000,v=8000", &b64(b"")))
        .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=1;ENOTSUP"),
        "oversize → ENOTSUP, got: {response:?}",
    );
}

#[test]
fn zlib_zip_bomb_is_capped_at_max_decoded_bytes() {
    let mut s = State::new(SECURITY_TEST_STORE_CAP);
    let huge = vec![0u8; MAX_DECODED_BYTES + 1024];
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&huge, 6);
    let response = s
        .dispatch(&body("Ga=t,i=2,f=32,s=1,v=1,o=z", &b64(&compressed)))
        .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=2;ENOTSUP"),
        "zip-bomb → ENOTSUP (OverBudget mapping), got: {response:?}",
    );
}

#[cfg(unix)]
#[test]
fn t_eq_f_with_symlink_to_real_file_returns_eio_under_nofollow() {
    // Refused even though the target is what the producer appears to
    // want: the rule keeps the namespace explicit.
    let real = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(real.path(), [0xAA, 0xBB, 0xCC, 0xDD]).unwrap();
    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("attacker_swap");
    symlink(real.path(), &link).unwrap();

    let mut s = State::new(SECURITY_TEST_STORE_CAP);
    let response = s
        .dispatch(&body(
            "Ga=t,i=3,t=f,f=32,s=1,v=1",
            &b64(link.to_str().unwrap().as_bytes()),
        ))
        .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=3;EIO"),
        "NOFOLLOW must refuse the symlink, got: {response:?}",
    );
}

#[cfg(unix)]
#[test]
fn t_eq_t_unlinks_temp_file_even_when_decode_fails() {
    // A leftover file would let a producer fill /tmp by retrying a bad
    // path.
    let mut s = State::new(SECURITY_TEST_STORE_CAP);
    let f = tempfile::NamedTempFile::new().unwrap();
    // Short, not long: `decode_raw` truncates over-long payloads
    // (page-rounded shm), so only a short one fails to decode.
    std::fs::write(f.path(), b"ab").unwrap();
    let path = f.path().to_path_buf();
    // `keep()` so the unlink observed is the dispatcher's, not the
    // tempfile crate's.
    let _kept = f.into_temp_path().keep();
    std::fs::write(&path, b"ab").unwrap();

    let response = s
        .dispatch(&body(
            "Ga=t,i=4,t=t,f=32,s=1,v=1",
            &b64(path.to_str().unwrap().as_bytes()),
        ))
        .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=4;EINVAL"),
        "raw payload length mismatch → EINVAL, got: {response:?}",
    );
    assert!(
        !path.exists(),
        "t=t must unlink even on decode failure, but {path:?} still exists",
    );
}

#[test]
fn store_full_of_pinned_images_recycles_the_oldest() {
    // kitty parity (graphics.c `ensure_space_for`): the cap evicts the
    // oldest image, placements included. Refusing to evict pinned
    // images freezes mpv --vo=kitty once the cap fills.
    // The cap is derived from what the store charges for one entry,
    // not a pixel count: a literal cap silently becomes "nothing fits".
    let small_cap = ImageEntry::new(1, 1, ImageFormat::Rgb24, vec![0u8; 3]).byte_len();
    let mut s = State::new(small_cap);
    let payload = b64(&[0xAA, 0xBB, 0xCC]);
    s.dispatch(&body("Ga=T,i=1,f=24,s=1,v=1", &payload))
        .unwrap();
    let response = s
        .dispatch(&body("Ga=t,i=2,f=24,s=1,v=1", &payload))
        .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=2;OK"),
        "the transmit recycles the oldest image, got: {response:?}",
    );
    assert!(
        s.images.get(ImageId(1)).is_none(),
        "the oldest image is evicted to make room",
    );
    assert!(
        s.placements.for_image(ImageId(1)).next().is_none(),
        "its placement goes with it",
    );
    assert!(
        s.images.get(ImageId(2)).is_some(),
        "the new image lands in the store",
    );
    assert!(
        s.images.bytes_used() <= small_cap,
        "the cap still bounds the store",
    );
}

#[test]
fn t_eq_f_with_null_byte_in_path_is_rejected() {
    let mut s = State::new(SECURITY_TEST_STORE_CAP);
    let bad_path = b"/tmp/test\x00.png";
    let response = s
        .dispatch(&body("Ga=t,i=5,t=f,f=32,s=1,v=1", &b64(bad_path)))
        .unwrap();
    // EINVAL (UTF-8 check), EIO (kernel NUL rejection) or ENOTSUP
    // (Windows refusing `t=f` wholesale): the pin is that nothing is
    // stored, not which layer says no.
    let prefix_einval = response.starts_with(b"\x1b_Gi=5;EINVAL");
    let prefix_eio = response.starts_with(b"\x1b_Gi=5;EIO");
    let prefix_enotsup = response.starts_with(b"\x1b_Gi=5;ENOTSUP");
    assert!(
        prefix_einval || prefix_eio || prefix_enotsup,
        "NUL-byte path must reject, got: {response:?}",
    );
    assert!(s.images.is_empty());
}

#[test]
fn fuzz_random_apc_bodies_never_panic() {
    // Every body starts with the `G` introducer; otherwise ~255/256
    // samples bounce off the not-graphics gate and no handler runs.
    let mut s = State::new(SECURITY_TEST_STORE_CAP);
    let mut state: u32 = 0xDEAD_BEEF;
    for _ in 0..256 {
        let len = {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state % 64) as usize
        };
        let mut bytes = Vec::with_capacity(len + 1);
        bytes.push(b'G');
        for _ in 0..len {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            bytes.push((state & 0xFF) as u8);
        }
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            s.dispatch(&bytes);
        }))
        .expect("dispatcher panicked on random APC body");
    }
}

//! Replay yazi's `KgpOld` preview burst and pin a live `z=-1` placement at the cursor.
//!
//! Simulates `delete all`, cursor repositioning, and chunked base64 image transmission
//! from `yazi-adapter/src/drivers/kgp_old.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_daemon::graphics::ImageEvent;
use felis_daemon::graphics::{ApcCtx, ShmDeferral, dispatch_apc_body};
use felis_grid::Grid;
use felis_grid::images::{ImageStore, Placements};
use felis_vt::Parser;
use felis_vt::kitty_graphics::Reassembler;

mod common;
use common::b64;

const TEST_STORE_CAP: usize = 64 * 1024 * 1024;

struct Harness {
    parser: Parser,
    reassembler: Reassembler,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    events: Vec<ImageEvent>,
    shm: ShmDeferral,
}

impl Harness {
    fn new() -> Self {
        Self {
            parser: Parser::new(),
            reassembler: Reassembler::new(),
            grid: Grid::new(24, 80),
            images: ImageStore::new(TEST_STORE_CAP),
            placements: Placements::new(),
            events: Vec::new(),
            shm: ShmDeferral::default(),
        }
    }

    fn drive(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.grid, bytes);
        for effect in self.grid.take_pty_effects() {
            if let felis_grid::PtyEffect::Apc(apc) = effect {
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
                    &apc.body,
                );
            }
        }
    }
}

fn yazi_burst(col: u16, row: u16, w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(
        pixels.len() as u32,
        w * h * 3,
        "RGB pixel count must match s*v"
    );
    let b64 = b64(pixels);

    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b_Gq=2,a=d,d=A\x1b\\");
    out.extend_from_slice(b"\x1b7");
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());

    let chunks: Vec<&[u8]> = b64.chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        let chunk_s = std::str::from_utf8(chunk).unwrap();
        if i == 0 {
            out.extend_from_slice(
                format!("\x1b_Gq=2,a=T,z=-1,C=1,f=24,s={w},v={h},m={more};{chunk_s}\x1b\\")
                    .as_bytes(),
            );
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};{chunk_s}\x1b\\").as_bytes());
        }
    }
    out.extend_from_slice(b"\x1b8");
    out
}

#[test]
fn single_chunk_preview_records_a_z_minus_one_placement_at_the_cursor() {
    let mut h = Harness::new();
    let burst = yazi_burst(2, 3, 2, 1, &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    h.drive(&burst);

    assert_eq!(
        h.placements.len(),
        1,
        "one live placement; events={:?}",
        h.events
    );
    let p = h.placements.iter().next().expect("placement present");
    assert_eq!(p.z_index, -1, "yazi places previews below text");
    // The anchor is not asserted: `anchor_cursor: None` anchors at the
    // live (post-DECRC) cursor, unlike production's parse-time capture;
    // felis-grid's `apc_body_captures_the_cursor_after_a_mid_burst_move`
    // owns that.
}

#[test]
fn multi_chunk_preview_reassembles_and_places() {
    let mut h = Harness::new();
    // 3300 bytes → 4400 base64 chars → two chunks.
    let pixels: Vec<u8> = (0..3300u32).map(|i| (i % 256) as u8).collect();
    let burst = yazi_burst(0, 0, 1100, 1, &pixels);
    h.drive(&burst);

    assert_eq!(
        h.placements.len(),
        1,
        "chunked transmit must still yield one placement; events={:?}",
        h.events
    );
    let p = h.placements.iter().next().expect("placement present");
    assert_eq!(p.z_index, -1);
}

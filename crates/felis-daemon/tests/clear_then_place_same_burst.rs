//! Verify erase vs placement ordering within a single PTY burst.
//!
//! Erase operations sweep only placements preceding them in the burst;
//! running eviction after the batch would remove newly created placements.

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

fn clear_then_place_burst() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[2J");
    out.extend_from_slice(b"\x1b[5;5H");
    let payload = b64(&[0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    out.extend_from_slice(b"\x1b_Ga=T,f=24,s=2,v=1,c=2,r=2;");
    out.extend_from_slice(&payload);
    out.extend_from_slice(b"\x1b\\");
    out
}

struct Pieces {
    parser: Parser,
    reassembler: Reassembler,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    events: Vec<ImageEvent>,
    shm: ShmDeferral,
}

impl Pieces {
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
}

#[test]
fn stream_order_replay_keeps_the_clear_then_draw_image() {
    use felis_grid::PtyEffect;
    let mut p = Pieces::new();
    p.parser.advance(&mut p.grid, &clear_then_place_burst());
    for effect in p.grid.take_pty_effects() {
        match effect {
            PtyEffect::Erased(range) => {
                for removed in
                    p.placements
                        .remove_intersecting(range.top, range.bottom, range.force)
                {
                    p.images.release(removed.image_id);
                    p.events.push(ImageEvent::PlacementRemoved {
                        image_id: removed.image_id,
                        placement_id: removed.placement_id,
                    });
                }
            }
            PtyEffect::Apc(apc) => {
                dispatch_apc_body(
                    &mut ApcCtx {
                        grid: &mut p.grid,
                        images: &mut p.images,
                        placements: &mut p.placements,
                        events: &mut p.events,
                        shm: &mut p.shm,
                        cell_pixel_w: 0,
                        cell_pixel_h: 0,
                        anchor_cursor: None,
                    },
                    &mut p.reassembler,
                    &apc.body,
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        p.placements.len(),
        1,
        "stream-order replay must keep the freshly placed image",
    );
}

//! Verify screen-switch vs placement ordering within a single PTY burst.
//!
//! Placements created after `?1049h` in the same burst stay live on the alt screen
//! rather than being swept into primary placements before screen-switch processing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_daemon::graphics::ImageEvent;
use felis_daemon::graphics::{ApcCtx, ShmDeferral, apply_screen_switch, dispatch_apc_body};
use felis_grid::images::{ImageStore, Placements};
use felis_grid::{Grid, PtyEffect};
use felis_vt::Parser;
use felis_vt::kitty_graphics::Reassembler;

mod common;
use common::{apc, b64};

const TEST_STORE_CAP: usize = 1024 * 1024;

fn drive_burst(
    parser: &mut Parser,
    reassembler: &mut Reassembler,
    grid: &mut Grid,
    images: &mut ImageStore,
    placements: &mut Placements,
    saved_primary: &mut Option<Placements>,
    events: &mut Vec<ImageEvent>,
    burst: &[u8],
) {
    parser.advance(grid, burst);
    let mut shm = ShmDeferral::default();
    for effect in grid.take_pty_effects() {
        match effect {
            PtyEffect::Apc(apc) => {
                dispatch_apc_body(
                    &mut ApcCtx {
                        grid: &mut *grid,
                        images: &mut *images,
                        placements: &mut *placements,
                        events: &mut *events,
                        shm: &mut shm,
                        cell_pixel_w: 0,
                        cell_pixel_h: 0,
                        anchor_cursor: None,
                    },
                    reassembler,
                    &apc.body,
                );
            }
            PtyEffect::ScreenSwitch(switch) => {
                apply_screen_switch(switch, images, placements, saved_primary, events);
            }
            _ => {}
        }
    }
}

#[test]
fn alt_enter_then_image_in_one_burst_keeps_the_placement() {
    let mut parser = Parser::new();
    let mut reassembler = Reassembler::new();
    let mut grid = Grid::new(24, 80);
    let mut images = ImageStore::new(TEST_STORE_CAP);
    let mut placements = Placements::new();
    let mut saved_primary: Option<Placements> = None;
    let mut events = Vec::new();

    let mut burst = Vec::new();
    burst.extend_from_slice(b"\x1b[?1049h");
    burst.extend_from_slice(&apc(
        "Ga=T,i=11,f=32,s=1,v=1",
        &b64(&[0xAA, 0xBB, 0xCC, 0xDD]),
    ));

    drive_burst(
        &mut parser,
        &mut reassembler,
        &mut grid,
        &mut images,
        &mut placements,
        &mut saved_primary,
        &mut events,
        &burst,
    );

    assert!(grid.on_alternate_screen(), "burst entered the alt screen");
    assert_eq!(
        placements.len(),
        1,
        "the image placed after ?1049h must stay live on the alt screen, not be swept into saved_primary; events={events:?}"
    );
}

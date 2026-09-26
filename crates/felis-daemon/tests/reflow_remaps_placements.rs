//! Pins the REQ-604 placement remap across the alt-screen-deferred
//! reflow (D4, `docs/explanation/data-model/scrollback.md`): the
//! `PrimaryReflowed` remap applies after the same burst's
//! `LeftAlternate` restores the saved primary placements. Mirrors
//! `serve::streaming::drive_pty_batch`'s stream-order replay.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_daemon::graphics::ImageEvent;
use felis_daemon::graphics::{
    ApcCtx, ShmDeferral, apply_reflow_remap, apply_screen_switch, dispatch_apc_body,
};
use felis_grid::images::{ImageStore, Placements};
use felis_grid::{Grid, PtyEffect};
use felis_vt::Parser;
use felis_vt::kitty_graphics::Reassembler;

mod common;
use common::{apc, b64};

const TEST_STORE_CAP: usize = 1024 * 1024;

#[allow(clippy::too_many_arguments)]
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
            PtyEffect::PrimaryReflowed(remap) => {
                apply_reflow_remap(&remap, images, placements, events);
            }
            _ => {}
        }
    }
}

#[test]
fn resize_under_alt_screen_remaps_restored_placements_on_exit() {
    let mut parser = Parser::new();
    let mut reassembler = Reassembler::new();
    let mut grid = Grid::new(6, 10);
    let mut images = ImageStore::new(TEST_STORE_CAP);
    let mut placements = Placements::new();
    let mut saved_primary: Option<Placements> = None;
    let mut events = Vec::new();

    let mut burst = Vec::new();
    burst.extend_from_slice(b"abcdefghijklmnopqr");
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
    assert_eq!(placements.len(), 1);
    assert_eq!(placements.iter().next().unwrap().anchor.row, 2);

    drive_burst(
        &mut parser,
        &mut reassembler,
        &mut grid,
        &mut images,
        &mut placements,
        &mut saved_primary,
        &mut events,
        b"\x1b[?1049h",
    );
    grid.resize(6, 4);

    // The anchor must follow its cell ("k", run offset 10) onto the
    // new third row.
    drive_burst(
        &mut parser,
        &mut reassembler,
        &mut grid,
        &mut images,
        &mut placements,
        &mut saved_primary,
        &mut events,
        b"\x1b[?1049l",
    );

    assert!(!grid.on_alternate_screen());
    assert_eq!(placements.len(), 1, "events={events:?}");
    assert_eq!(placements.iter().next().unwrap().anchor.row, 3);
}

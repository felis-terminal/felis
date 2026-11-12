//! Pins the REQ-604 placement remap across the alt-screen-deferred
//! reflow (D4, `docs/explanation/data-model/scrollback.md`): the
//! `PrimaryReflowed` remap applies after the same burst's
//! `LeftAlternate` restores the saved primary placements. Mirrors
//! `serve::streaming::drive_pty_batch`'s stream-order replay.

use crate::common::{KittyState, apc, b64};

const TEST_STORE_CAP: usize = 1024 * 1024;

#[test]
fn resize_under_alt_screen_remaps_restored_placements_on_exit() {
    let mut s = KittyState::new(6, 10, TEST_STORE_CAP);

    let mut burst = Vec::new();
    burst.extend_from_slice(b"abcdefghijklmnopqr");
    burst.extend_from_slice(&apc(
        "Ga=T,i=11,f=32,s=1,v=1",
        &b64(&[0xAA, 0xBB, 0xCC, 0xDD]),
    ));
    s.replay(&burst);
    assert_eq!(s.placements.len(), 1);
    assert_eq!(s.placements.iter().next().unwrap().anchor.row, 2);

    s.replay(b"\x1b[?1049h");
    s.grid.resize(6, 4);

    // The anchor must follow its cell ("k", run offset 10) onto the
    // new third row.
    s.replay(b"\x1b[?1049l");

    assert!(!s.grid.on_alternate_screen());
    assert_eq!(s.placements.len(), 1, "events={:?}", s.events);
    assert_eq!(s.placements.iter().next().unwrap().anchor.row, 3);
}

//! Verify screen-switch vs placement ordering within a single PTY burst.
//!
//! Placements created after `?1049h` in the same burst stay live on the alt screen
//! rather than being swept into primary placements before screen-switch processing.

use crate::common::{KittyState, apc, b64};

const TEST_STORE_CAP: usize = 1024 * 1024;

#[test]
fn alt_enter_then_image_in_one_burst_keeps_the_placement() {
    let mut s = KittyState::new(24, 80, TEST_STORE_CAP);

    let mut burst = Vec::new();
    burst.extend_from_slice(b"\x1b[?1049h");
    burst.extend_from_slice(&apc(
        "Ga=T,i=11,f=32,s=1,v=1",
        &b64(&[0xAA, 0xBB, 0xCC, 0xDD]),
    ));

    s.replay(&burst);

    assert!(s.grid.on_alternate_screen(), "burst entered the alt screen");
    assert_eq!(
        s.placements.len(),
        1,
        "the image placed after ?1049h must stay live on the alt screen, not be swept into the saved primary screen; events={:?}",
        s.events
    );
}

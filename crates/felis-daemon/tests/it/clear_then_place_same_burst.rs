//! Verify erase vs placement ordering within a single PTY burst.
//!
//! Erase operations sweep only placements preceding them in the burst;
//! running eviction after the batch would remove newly created placements.

use crate::common::{KittyState, b64};

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

#[test]
fn stream_order_replay_keeps_the_clear_then_draw_image() {
    let mut p = KittyState::new(24, 80, TEST_STORE_CAP);
    p.replay(&clear_then_place_burst());
    assert_eq!(
        p.placements.len(),
        1,
        "stream-order replay must keep the freshly placed image",
    );
}

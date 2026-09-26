//! Synchronized-output presentation-gate fuzz target.
//!
//! Where `grid_dispatch.rs` widens dispatch coverage with the
//! authoritative grid, this target stresses the `?2026` gate
//! specifically: arbitrary BSU/ESU storms interleaved with arbitrary
//! VT bytes and arbitrary virtual-clock advances. Win condition is
//! silence — panic = bug — plus the two invariants below:
//!
//! 1. `ready_to_present(now) == true` implies the BSU flag is off.
//!    Otherwise the daemon's emission loop would happily ship a
//!    half-applied frame after a buggy producer's BSU.
//! 2. The deadline returned by `synchronized_output_deadline(now)`
//!    is bounded by `last_now + SYNC_OUTPUT_TIMEOUT`. Otherwise a
//!    duplicate-BSU storm could slide the timer indefinitely and
//!    defeat REQ-1004.
//!
//! The fuzzer's bytes are split on a synthetic separator into
//! "ticks": between each tick the virtual clock advances by a
//! caller-controlled amount derived from the same stream, so a
//! single corpus entry covers many time advances. No real time is
//! spent in the harness.

#![no_main]

use std::time::{Duration, Instant};

use felis_grid::Grid;
use felis_vt::Parser;
use libfuzzer_sys::fuzz_target;

const TICK_SEPARATOR: u8 = 0x00;

fuzz_target!(|data: &[u8]| {
    let mut grid = Grid::new(4, 16);
    let mut parser = Parser::new();
    // Anchor the virtual clock at a fixed point; fuzzer-derived ms
    // advances move it forward in `Duration::from_millis` steps.
    let t0 = Instant::now();
    let mut clock_ms: u64 = 0;

    // The first byte of each tick is the ms-advance; the rest is
    // raw VT input fed through the parser. `0x00` separates ticks.
    for chunk in data.split(|&b| b == TICK_SEPARATOR) {
        let (advance, body) = match chunk.split_first() {
            Some((&hd, rest)) => (u64::from(hd), rest),
            None => (0, &[][..]),
        };
        clock_ms = clock_ms.saturating_add(advance);
        let now = t0 + Duration::from_millis(clock_ms);

        parser.advance(&mut grid, body);

        // Invariant 2: deadline is bounded by `now + TIMEOUT`. Read
        // it *before* the present check so the lazy anchoring is
        // recorded at this `now`.
        if let Some(deadline) = grid.synchronized_output_deadline(now) {
            let cap = now + Grid::SYNC_OUTPUT_TIMEOUT;
            assert!(
                deadline <= cap,
                "deadline {deadline:?} exceeds now+timeout {cap:?}"
            );
        }

        // Invariant 1: present-ready ⇒ sync cleared.
        if grid.ready_to_present(now) {
            assert!(
                !grid.synchronized_output(),
                "ready_to_present(now) opened the gate while ?2026 was still on"
            );
            assert!(
                grid.synchronized_output_deadline(now).is_none(),
                "ready_to_present(now) opened the gate but left a deadline behind"
            );
        }
    }

    // PtyEffect subsumes the former pending-response and clipboard-set
    // queues since the single-queue refactor (one stream-ordered drain).
    drop(grid.take_pty_effects());
    drop(grid.take_notifications());
    let _ = grid.take_kitty_kbd_dirty();
    drop(grid.take_title_dirty());
    drop(grid.take_cwd_dirty());
    drop(grid.take_pointer_shape_dirty());
});

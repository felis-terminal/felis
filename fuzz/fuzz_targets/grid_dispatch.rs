//! Grid dispatch fuzz target.
//!
//! Where `vt_parser.rs` widens parser-state coverage with a no-op
//! sink, this target widens *dispatch* coverage by feeding the same
//! bytes to a real `Grid`. Every CSI handler, OSC handler (including
//! the OSC 52 cache, OSC 8 hyperlink table, OSC 10/11/12 color
//! responses), the unified PTY-effect queue, scrollback path, and
//! damage tracker is exercised on whatever the fuzzer produces. The
//! win condition is silence: panic = bug.
//!
//! The harness drains every take_* queue each run so accumulated state
//! (PTY effects, notifications, grid events, dirty rows) cannot OOM the
//! corpus minimizer over long sessions.

#![no_main]

use felis_grid::Grid;
use felis_vt::Parser;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut grid = Grid::new(8, 32);
    let mut parser = Parser::new();
    parser.advance(&mut grid, data);
    // PtyEffect subsumes the former pending-response and clipboard-set
    // queues since the single-queue refactor (one stream-ordered drain).
    drop(grid.take_pty_effects());
    drop(grid.take_notifications());
    let _ = grid.take_kitty_kbd_dirty();
    drop(grid.take_title_dirty());
    drop(grid.take_cwd_dirty());
    drop(grid.take_pointer_shape_dirty());
});

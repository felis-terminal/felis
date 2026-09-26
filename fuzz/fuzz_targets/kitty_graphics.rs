//! Kitty graphics command parser fuzz target.
//!
//! Drives `felis_vt::kitty_graphics::parse` with arbitrary bytes.
//! The proptest in `crates/felis-vt/tests/proptest_kitty_graphics.rs`
//! already pins totality + structural invariants over a 256-byte
//! input space; this target widens to whatever a coverage-guided
//! fuzzer can find. Win condition is silence: panic = bug.

#![no_main]

use felis_vt::kitty_graphics::parse;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    drop(parse(data));
});

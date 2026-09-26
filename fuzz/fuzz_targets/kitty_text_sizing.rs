//! OSC 66 (Kitty text-sizing) parser fuzz target.
//!
//! Drives `felis_vt::kitty_text_sizing::parse_metadata` with
//! arbitrary bytes. Companion to the proptest in
//! `crates/felis-vt/tests/proptest_kitty_text_sizing.rs`; the
//! proptest already pins totality up to 256-byte inputs and the
//! structural invariants on accepted sizings, but coverage-guided
//! fuzzing widens the input space libfuzzer explores. Win
//! condition is silence: panic = bug.

#![no_main]

use felis_vt::kitty_text_sizing::parse_metadata;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_metadata(data);
});

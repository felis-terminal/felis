//! OSC 7501 (Program Status Protocol) decoder fuzz target.
//!
//! Input convention: the first byte's low bit picks the terminator (BEL
//! when set) and the rest is the payload after `7501;`. Companion to
//! `crates/felis-vt/tests/it/proptest_program_status.rs`; win condition is
//! silence: panic = bug.

#![no_main]

use felis_vt::program_status::{Terminator, parse_osc7501};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&first, payload)) = data.split_first() else {
        return;
    };
    let terminator = if first & 1 == 1 {
        Terminator::Bel
    } else {
        Terminator::St
    };
    let mut body = b"7501;".to_vec();
    body.extend_from_slice(payload);
    let _ = parse_osc7501(&body, terminator);
});

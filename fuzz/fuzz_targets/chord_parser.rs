//! Chord-parser fuzz target.
//!
//! Drives `felis_client_core::keymap::Chord::from_str` with
//! arbitrary bytes interpreted as UTF-8. Companion to the proptests
//! at `crates/felis-client-core/src/keymap/chord.rs` `proptest!`
//! block; PBT runs ~256 cases per `cargo test`, libFuzzer here
//! drives the input space adversarially under coverage feedback.
//!
//! Win condition is silence: panic = bug, `Err(_)` returns are
//! expected for malformed input. The fuzzer additionally pins the
//! Display-round-trip identity for every Chord the parser
//! accepts — a Display impl that emits a non-canonical form would
//! surface as a re-parse mismatch here.

#![no_main]

use felis_client_core::keymap::Chord;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let parsed = match s.parse::<Chord>() {
        Ok(c) => c,
        Err(_) => return,
    };

    // Display → re-parse identity. The PBT block in
    // `keymap::chord::tests` already covers this for random
    // Chord values constructed in-memory; here we hit it from
    // the *parser-accepted* side of the funnel, which can in
    // principle accept inputs the in-memory strategy doesn't
    // generate (e.g. modifier orderings or alias spellings that
    // canonicalize on Display).
    let canonical = parsed.to_string();
    let reparsed: Chord = canonical
        .parse()
        .expect("canonical Display output must parse");
    assert_eq!(
        reparsed, parsed,
        "Display→parse round trip changed Chord ({s:?} → {canonical:?})",
    );
    assert_eq!(
        reparsed.to_string(),
        canonical,
        "Display impl is not idempotent for {s:?} (first {canonical:?}, second {})",
        reparsed.to_string(),
    );
});

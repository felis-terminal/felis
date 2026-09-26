//! VT parser fuzz target.
//!
//! Drives `felis_vt::Parser` with arbitrary bytes through a no-op sink.
//! The win condition is silence: the property suite already asserts
//! totality and dispatch-side bounds; this target widens the coverage to
//! the unbounded inputs only a coverage-guided fuzzer can find.

#![no_main]

use felis_vt::{Parser, Sink};
use libfuzzer_sys::fuzz_target;

struct NoopSink;

impl Sink for NoopSink {}

fuzz_target!(|data: &[u8]| {
    let mut parser = Parser::new();
    let mut sink = NoopSink;
    parser.advance(&mut sink, data);
});

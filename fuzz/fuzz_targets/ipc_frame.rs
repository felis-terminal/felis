//! IPC frame decoder fuzz target.
//!
//! Drives `felis_protocol::frame::decode_with_ceiling` with arbitrary
//! bytes plus an arbitrary ceiling. The decoder is the trust boundary
//! every byte that arrives over the daemon socket crosses; a panic
//! here lets a malicious peer crash the daemon. The proptest suite
//! already asserts totality with respect to a typed-noise input
//! distribution; this fuzz target widens the coverage to the truly
//! adversarial inputs only a coverage-guided engine can find.
//!
//! The encode direction rides along: whatever the decoder admits must
//! either re-encode byte-for-byte or be refused for its size alone
//! (`Frame::encode`, REQ-105).
//!
//! `decode_with_ceiling` must, for any `(input, ceiling)`:
//!   * never panic, never over-read past `input.len()`,
//!   * either return a `Frame` whose `body` is a sub-slice of `input`,
//!   * or return one of the typed `FrameError` variants.

#![no_main]

use felis_protocol::frame::{FrameError, decode_with_ceiling};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Burn the first 4 bytes of the input as the ceiling so a single
    // fuzzer corpus item can exercise both the buffer layout and the
    // ceiling code path. Keeping the ceiling derived from the data
    // means a single mutation can flip whether the rest of the input
    // is "in budget" or not.
    let ceiling = if data.len() >= 4 {
        u32::from_le_bytes([data[0], data[1], data[2], data[3]])
    } else {
        u32::MAX
    };
    let body = if data.len() >= 4 { &data[4..] } else { data };

    match decode_with_ceiling(body, ceiling) {
        Ok((frame, consumed)) => {
            // The returned frame body must be a sub-slice of the
            // input — `decode_with_ceiling` should never synthesize
            // bytes the input did not contain.
            assert!(consumed <= body.len());
            assert!(frame.body.len() <= body.len());
            // Round-trip: re-encoding the decoded frame must produce
            // the same prefix the decoder consumed. A frame the
            // default-ceiling decoder admitted always re-encodes; one
            // admitted under a wider explicit ceiling may not, and
            // that refusal is the encode-side guard doing its job.
            match frame.encode() {
                Ok(re) => assert_eq!(re.as_slice(), &body[..consumed]),
                Err(FrameError::BodyTooLarge {
                    body_len,
                    ceiling: c,
                }) => {
                    assert_eq!(c, felis_protocol::frame::DEFAULT_MAX_BODY);
                    assert!(body_len > u64::from(c));
                }
                Err(other) => panic!("encode refused for a non-size reason: {other:?}"),
            }
        }
        Err(FrameError::Incomplete { need }) => {
            // Caller must always have a chance to make progress —
            // `need` is "additional bytes required", so it is at
            // least 1 when we got Incomplete.
            assert!(need >= 1);
        }
        Err(FrameError::BodyTooLarge {
            body_len,
            ceiling: c,
        }) => {
            assert_eq!(c, ceiling);
            assert!(body_len > u64::from(ceiling));
        }
        Err(FrameError::LenUnderflow { len }) => {
            assert!(len < felis_protocol::frame::LEN_OVERHEAD);
        }
    }
});

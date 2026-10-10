//! The Kitty graphics command parser is total on arbitrary bytes, and any
//! `Command` it emits respects the structural invariants the dispatcher
//! relies on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_vt::kitty_graphics::{
    Command, Outcome, Reassembler,
    inflate::{InflateError, inflate},
    parse,
};
use miniz_oxide::deflate::compress_to_vec_zlib;
use proptest::prelude::*;

proptest! {
    /// Every accepted control key is a single ASCII letter and every value
    /// is non-empty `[A-Za-z0-9-]`; `parse` never panics.
    #[test]
    fn accepted_command_respects_structural_invariants(
        body in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let Some(cmd) = parse(&body) else { return Ok(()) };
        for (key, value) in &cmd.controls {
            prop_assert!(
                key.is_ascii_alphabetic(),
                "key {key:#x} is not an ASCII letter",
            );
            prop_assert!(
                !value.is_empty(),
                "value for key {key:#x} is empty",
            );
            prop_assert!(
                value.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-'),
                "value for key {key:#x} contains a disallowed byte: {value:?}",
            );
        }
    }

    /// `o=z` producers expect the exact bytes they sent, so inflate must
    /// match the standard zlib encoder byte-for-byte.
    #[test]
    fn compress_inflate_round_trips_arbitrary_bytes(
        original in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let compressed = compress_to_vec_zlib(&original, 6);
        let inflated = inflate(&compressed, original.len()).unwrap();
        prop_assert_eq!(inflated, original);
    }

    /// The cap is a hard budget, not a hint: a payload one byte over it
    /// is a typed size-limit refusal, distinct from malformed input.
    #[test]
    fn the_output_cap_decides_between_the_payload_and_a_size_limit(
        original in proptest::collection::vec(any::<u8>(), 1..1024),
        cap in 0usize..1024,
    ) {
        let compressed = compress_to_vec_zlib(&original, 6);
        if cap >= original.len() {
            prop_assert_eq!(inflate(&compressed, cap), Ok(original));
        } else {
            prop_assert_eq!(inflate(&compressed, cap), Err(InflateError::SizeLimit));
        }
    }

    /// `inflate` is total: the dispatcher hands it whatever made it past
    /// the reassembler.
    #[test]
    fn inflate_is_total_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
        cap in 0usize..4096,
    ) {
        drop(inflate(&bytes, cap));
    }

    /// `feed` is associative over chunk boundaries.
    #[test]
    fn reassembled_payload_is_chunk_concatenation(
        chunks in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..32),
            1..8,
        ),
    ) {
        let mut r = Reassembler::new();
        let last = chunks.len() - 1;
        let mut emitted: Option<Vec<u8>> = None;
        for (i, chunk) in chunks.iter().enumerate() {
            let m_value: &[u8] = if i == last { b"0" } else { b"1" };
            // The head chunk carries `a=T`; mid / tail chunks carry only `m`.
            let controls: Vec<(u8, &[u8])> = if i == 0 {
                vec![(b'a', b"T".as_slice()), (b'm', m_value)]
            } else {
                vec![(b'm', m_value)]
            };
            let cmd = Command {
                controls,
                payload: chunk,
            };
            match r.feed(&cmd) {
                Outcome::Pending => {
                    prop_assert!(i < last, "Pending must only appear on non-final chunks");
                }
                Outcome::Done(complete) => {
                    prop_assert_eq!(i, last, "Done arrived before the final chunk");
                    emitted = Some(complete.payload);
                }
                Outcome::Overflow { .. } => {
                    // Correctness is only asserted when overflow is not hit.
                    return Ok(());
                }
            }
        }
        let expected: Vec<u8> = chunks.into_iter().flatten().collect();
        prop_assert_eq!(emitted.unwrap(), expected);
    }

    /// A put, compose or animation-control command completes on its own
    /// body whatever its `m=`, even mid-transfer, and the transfer it
    /// interrupts still assembles from its chunks alone.
    #[test]
    fn put_compose_and_animate_run_at_once_without_disturbing_an_open_transfer(
        chunks in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..32),
            2..6,
        ),
        mid_transfer in prop::sample::select(vec![b"p", b"c", b"a"]),
        interleaved in proptest::collection::vec(
            (
                0..6usize,
                prop::sample::select(vec![b"p", b"c", b"a"]),
                any::<bool>(),
                proptest::collection::vec(any::<u8>(), 0..8),
            ),
            0..6,
        ),
    ) {
        let mut r = Reassembler::new();
        let last = chunks.len() - 1;
        let mut emitted: Option<Vec<u8>> = None;
        let forced = (last, mid_transfer, true, Vec::new());
        for (i, chunk) in chunks.iter().enumerate() {
            let here = interleaved.iter().chain(std::iter::once(&forced));
            for (_, action, more, payload) in here.filter(|(at, ..)| *at == i) {
                let m_value: &[u8] = if *more { b"1" } else { b"0" };
                let cmd = Command {
                    controls: vec![(b'a', action.as_slice()), (b'm', m_value)],
                    payload,
                };
                let Outcome::Done(complete) = r.feed(&cmd) else {
                    return Err(TestCaseError::fail("a non-add command must complete at once"));
                };
                prop_assert_eq!(complete.controls, vec![(b'a', action.to_vec())]);
                prop_assert_eq!(&complete.payload, payload);
            }
            let m_value: &[u8] = if i == last { b"0" } else { b"1" };
            let controls: Vec<(u8, &[u8])> = if i == 0 {
                vec![(b'a', b"T".as_slice()), (b'm', m_value)]
            } else {
                vec![(b'm', m_value)]
            };
            match r.feed(&Command { controls, payload: chunk }) {
                Outcome::Pending => prop_assert!(i < last),
                Outcome::Done(complete) => {
                    prop_assert_eq!(i, last);
                    prop_assert_eq!(complete.controls, vec![(b'a', b"T".to_vec())]);
                    emitted = Some(complete.payload);
                }
                Outcome::Overflow { .. } => return Ok(()),
            }
        }
        let expected: Vec<u8> = chunks.into_iter().flatten().collect();
        prop_assert_eq!(emitted.unwrap(), expected);
    }

    /// Whatever `parse` emits is something a valid producer could have sent.
    #[test]
    fn round_trip_well_formed_input(
        keys in proptest::collection::vec(
            "[A-Za-z]",
            0..8,
        ),
        values in proptest::collection::vec(
            "-?[A-Za-z0-9]{1,8}",
            0..8,
        ),
        payload in proptest::collection::vec(any::<u8>(), 0..32),
    ) {
        let pair_count = keys.len().min(values.len());
        let mut wire = Vec::from(b"G".as_slice());
        for i in 0..pair_count {
            if i > 0 {
                wire.push(b',');
            }
            wire.push(keys[i].as_bytes()[0]);
            wire.push(b'=');
            wire.extend_from_slice(values[i].as_bytes());
        }
        wire.push(b';');
        wire.extend_from_slice(&payload);

        let cmd: Command<'_> = parse(&wire).expect("well-formed input must parse");
        prop_assert_eq!(cmd.controls.len(), pair_count);
        for i in 0..pair_count {
            prop_assert_eq!(cmd.controls[i].0, keys[i].as_bytes()[0]);
            prop_assert_eq!(cmd.controls[i].1, values[i].as_bytes());
        }
        prop_assert_eq!(cmd.payload, payload.as_slice());
    }
}

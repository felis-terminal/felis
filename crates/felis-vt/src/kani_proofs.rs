//! Kani proofs for SWAR print-run scanners (`docs/reference/testing.md` "Kani proof
//! inventory").
//!
//! Verifies borrow interactions between bytes. The mask invariant is valid
//! only at the lowest set lane, not across every non-printable lane.

use super::{
    MAX_PARAMS, Params, nonprintable_mask, scan_mixed_print_run, scan_printable_run,
    scan_string_body, string_special_mask,
};

/// `nonprintable_mask` is zero iff all eight bytes are printable, else its
/// lowest set lane is the index of the first non-printable byte.
#[kani::proof]
#[kani::unwind(9)]
fn nonprintable_mask_lowest_set_lane_is_first_nonprintable() {
    let x: u64 = kani::any();
    let mask = nonprintable_mask(x);
    let bytes = x.to_le_bytes();
    let first = bytes.iter().position(|&b| !(0x20..=0x7E).contains(&b));
    match first {
        None => assert_eq!(mask, 0, "all-printable word must yield an empty mask"),
        Some(idx) => {
            assert!(mask != 0, "a non-printable byte must set some lane");
            assert_eq!((mask.trailing_zeros() / 8) as usize, idx);
        }
    }
}

/// Six digits: two more than the widest in-range value (65535) needs, so
/// the saturation path is exercised.
#[kani::proof]
#[kani::unwind(7)]
fn params_push_digit_saturates_to_clamped_decimal() {
    let digits: [u8; 6] = kani::any();
    let mut params = Params::new();
    let mut reference: u32 = 0;
    for &d in &digits {
        kani::assume(d <= 9);
        params.push_digit(d);
        reference = (reference * 10 + u32::from(d)).min(u32::from(u16::MAX));
        assert_eq!(params.as_slice()[0], reference as u16);
    }
}

/// 16 bytes is two SWAR chunks, so an alignment bug at the chunk seam
/// surfaces here.
#[kani::proof]
#[kani::unwind(17)]
fn scan_printable_run_equals_scalar_over_16_byte_window() {
    let buf: [u8; 16] = kani::any();
    let scalar = buf
        .iter()
        .position(|&b| !(0x20..=0x7E).contains(&b))
        .unwrap_or(buf.len());
    assert_eq!(scan_printable_run(&buf), scalar);
}

/// Same 16-byte window as `scan_printable_run`.
#[kani::proof]
#[kani::unwind(17)]
fn scan_mixed_print_run_equals_scalar_over_16_byte_window() {
    let buf: [u8; 16] = kani::any();
    let scalar = buf
        .iter()
        .position(|&b| b < 0x20 || b == 0x7F)
        .unwrap_or(buf.len());
    assert_eq!(scan_mixed_print_run(&buf), scalar);
}

/// Same lowest-lane-only contract as `nonprintable_mask`.
#[kani::proof]
#[kani::unwind(9)]
fn string_special_mask_lowest_set_lane_is_first_special() {
    let x: u64 = kani::any();
    let mask = string_special_mask(x);
    let bytes = x.to_le_bytes();
    let first = bytes
        .iter()
        .position(|&b| matches!(b, 0x07 | 0x18 | 0x1A | 0x1B));
    match first {
        None => assert_eq!(mask, 0, "special-free word must yield an empty mask"),
        Some(idx) => {
            assert!(mask != 0, "a special byte must set some lane");
            assert_eq!((mask.trailing_zeros() / 8) as usize, idx);
        }
    }
}

/// The scalar position is an independent oracle: a bulk arm that over-ran
/// into the CSI final byte, or under-ran, would surface as a count mismatch.
#[kani::proof]
#[kani::unwind(7)]
fn push_run_stops_at_first_nonparameter_byte() {
    let buf: [u8; 6] = kani::any();
    let mut params = Params::new();
    let n = params.push_run(&buf);
    let scalar = buf
        .iter()
        .position(|&b| !(0x30..=0x3B).contains(&b))
        .unwrap_or(buf.len());
    assert_eq!(n, scalar);
}

/// The claim the CsiParam bulk arm rides on: `advance` hands `push_run`
/// whatever the current chunk holds, so the chunker's split points must
/// not be observable in saturation, overflow, or the subparam bitmap.
/// Eight bytes fill half the slots; every split point is tried.
#[kani::proof]
#[kani::unwind(19)]
fn push_run_split_invariance() {
    let buf: [u8; 8] = kani::any();
    let split: usize = kani::any();
    kani::assume(split <= buf.len());

    let mut one_shot = Params::new();
    let whole = one_shot.push_run(&buf);

    let mut halved = Params::new();
    let n1 = halved.push_run(&buf[..split]);
    let n2 = halved.push_run(&buf[n1..]);

    // Field-by-field rather than derived `PartialEq`: the struct eq lowers
    // to `memcmp`, whose loop sits outside `#[kani::unwind]`'s reach.
    assert_eq!(halved.len, one_shot.len);
    assert_eq!(halved.overflowed, one_shot.overflowed);
    assert_eq!(halved.subparam_mask, one_shot.subparam_mask);
    let mut slot = 0;
    while slot < MAX_PARAMS {
        assert_eq!(halved.values[slot], one_shot.values[slot]);
        slot += 1;
    }
    assert_eq!(n1 + n2, whole);
}

/// `push_run` keeps the open slot in a register instead of calling the
/// per-byte primitives, so its equivalence to them is the proof obligation.
/// Eight bytes cover a saturating run and a subparam next to a plain
/// separator.
#[kani::proof]
#[kani::unwind(17)]
fn push_run_equals_per_byte_primitives() {
    let buf: [u8; 8] = kani::any();

    let mut bulk = Params::new();
    let n = bulk.push_run(&buf);

    let mut per_byte = Params::new();
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            b @ 0x30..=0x39 => per_byte.push_digit(b - b'0'),
            b @ (0x3A | 0x3B) => per_byte.next_slot(b == 0x3A),
            _ => break,
        }
        i += 1;
    }

    assert_eq!(n, i);
    assert_eq!(bulk.len, per_byte.len);
    assert_eq!(bulk.overflowed, per_byte.overflowed);
    assert_eq!(bulk.subparam_mask, per_byte.subparam_mask);
    let mut slot = 0;
    while slot < MAX_PARAMS {
        assert_eq!(bulk.values[slot], per_byte.values[slot]);
        slot += 1;
    }
}

/// Same 16-byte window as `scan_printable_run`.
#[kani::proof]
#[kani::unwind(17)]
fn scan_string_body_equals_scalar_over_16_byte_window() {
    let buf: [u8; 16] = kani::any();
    let scalar = buf
        .iter()
        .position(|&b| matches!(b, 0x07 | 0x18 | 0x1A | 0x1B))
        .unwrap_or(buf.len());
    assert_eq!(scan_string_body(&buf), scalar);
}

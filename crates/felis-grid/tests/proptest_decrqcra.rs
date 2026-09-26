//! Property: DECRQCRA is a coherent rectangle-checksum function.
//!
//! Spec source: `xterm/charproc.c::do_dec_check_sum` (xterm-379)
//! plus the producer-side complement in
//! `esctest/escutil.AssertScreenCharsInRectEqual`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::Grid;
use felis_vt::Parser;
use proptest::prelude::*;

mod common;

fn responses_for(rows: u16, cols: u16, bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut g = Grid::new(rows, cols);
    let mut p = Parser::new();
    p.advance(&mut g, bytes);
    common::responses(&mut g)
}

fn parse_decrqcra_reply(bytes: &[u8]) -> Option<(u16, u16)> {
    let s = std::str::from_utf8(bytes).ok()?;
    let body = s.strip_prefix("\x1bP")?.strip_suffix("\x1b\\")?;
    let (pid, rest) = body.split_once("!~")?;
    Some((pid.parse().ok()?, u16::from_str_radix(rest, 16).ok()?))
}

proptest! {
    #[test]
    fn single_ascii_cell_returns_negated_codepoint(
        ch in 0x21u8..=0x7E,
    ) {
        let bytes = [ch];
        let mut cmd = bytes.to_vec();
        cmd.extend_from_slice(b"\x1b[1;1;1;1;1;1*y");
        let responses = responses_for(4, 8, &cmd);
        prop_assert_eq!(responses.len(), 1);
        let (pid, checksum) = parse_decrqcra_reply(&responses[0]).unwrap();
        prop_assert_eq!(pid, 1);
        let expected = u16::wrapping_neg(u16::from(ch));
        prop_assert_eq!(checksum, expected);
    }

    #[test]
    fn two_cell_rect_sums_negated_codepoints(
        a in 0x21u8..=0x7E,
        b in 0x21u8..=0x7E,
    ) {
        let mut cmd = vec![a, b];
        cmd.extend_from_slice(b"\x1b[2;1;1;1;1;2*y");
        let responses = responses_for(4, 8, &cmd);
        let (pid, checksum) = parse_decrqcra_reply(&responses[0]).unwrap();
        prop_assert_eq!(pid, 2);
        let expected = u16::wrapping_neg(u16::from(a).wrapping_add(u16::from(b)));
        prop_assert_eq!(checksum, expected);
    }

    /// xterm counts an empty cell as a space (32).
    #[test]
    fn empty_rect_returns_negated_space_sum(
        rows in 1u16..=6,
        cols in 1u16..=6,
    ) {
        let cmd = format!("\x1b[3;1;1;1;{rows};{cols}*y");
        let responses = responses_for(8, 8, cmd.as_bytes());
        let (pid, checksum) = parse_decrqcra_reply(&responses[0]).unwrap();
        prop_assert_eq!(pid, 3);
        let cells = u32::from(rows) * u32::from(cols);
        let space_sum = (cells * 32) as u16;
        let expected = u16::wrapping_neg(space_sum);
        prop_assert_eq!(checksum, expected);
    }

    #[test]
    fn out_of_range_rect_clamps_to_grid_bounds(
        extra_rows in 0u16..=100,
        extra_cols in 0u16..=100,
    ) {
        const ROWS: u16 = 4;
        const COLS: u16 = 8;
        let ref_cmd = format!("\x1b[10;1;1;1;{ROWS};{COLS}*y");
        let ref_responses = responses_for(ROWS, COLS, ref_cmd.as_bytes());
        let (_, ref_checksum) = parse_decrqcra_reply(&ref_responses[0]).unwrap();
        let pb = ROWS.saturating_add(extra_rows);
        let pr = COLS.saturating_add(extra_cols);
        let cmd = format!("\x1b[11;1;1;1;{pb};{pr}*y");
        let responses = responses_for(ROWS, COLS, cmd.as_bytes());
        let (pid, checksum) = parse_decrqcra_reply(&responses[0]).unwrap();
        prop_assert_eq!(pid, 11);
        prop_assert_eq!(checksum, ref_checksum);
    }
}

//! Property: DECRQM / SM / RM / DECSET / DECRESET is a coherent
//! mode-state machine. Spec sources: ECMA-48 §7.3.3 (SM/RM/DECRQM),
//! xterm's `charproc.c` (`set_ansi_mode` / `dpmodes` / `savemodes`),
//! and esctest's `decrqm.py`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::{
    Grid, KNOWN_MODIFIABLE_DEC_MODES, PERMANENTLY_RESET_ANSI_MODES, PERMANENTLY_RESET_DEC_MODES,
    PERMANENTLY_SET_DEC_MODES,
};
use felis_vt::Parser;
use proptest::prelude::*;

mod common;

const ANSI_FUNCTIONAL: &[u16] = &[4, 20];
const ANSI_UNIMPLEMENTED: &[u16] = &[2, 12, 21, 33, 99];
const DEC_FUNCTIONAL: &[u16] = &[1, 5, 6, 7, 25, 1004, 1049, 2004, 2026];

fn drive(bytes: &[u8]) -> (Grid, Vec<Vec<u8>>) {
    let mut g = Grid::new(4, 16);
    let mut p = Parser::new();
    p.advance(&mut g, bytes);
    let responses = common::responses(&mut g);
    (g, responses)
}

fn parse_decrqm_reply(bytes: &[u8]) -> Option<(bool, u16, u8)> {
    let s = std::str::from_utf8(bytes).ok()?;
    let body = s.strip_prefix("\x1b[")?.strip_suffix("$y")?;
    let (private, body) = if let Some(rest) = body.strip_prefix('?') {
        (true, rest)
    } else {
        (false, body)
    };
    let (mode, ps) = body.split_once(';')?;
    Some((private, mode.parse().ok()?, ps.parse().ok()?))
}

proptest! {
    #[test]
    fn ansi_permanently_reset_modes_report_ps4(
        idx in 0usize..PERMANENTLY_RESET_ANSI_MODES.len(),
        flip_writes in proptest::collection::vec(any::<bool>(), 0..8),
    ) {
        let mode = PERMANENTLY_RESET_ANSI_MODES[idx];
        let mut cmd = Vec::new();
        for set in &flip_writes {
            cmd.extend_from_slice(format!("\x1b[{mode}{}", if *set { 'h' } else { 'l' }).as_bytes());
        }
        cmd.extend_from_slice(format!("\x1b[{mode}$p").as_bytes());
        let (_g, responses) = drive(&cmd);
        prop_assert_eq!(responses.len(), 1);
        let (private, m, ps) = parse_decrqm_reply(&responses[0]).unwrap();
        prop_assert!(!private);
        prop_assert_eq!(m, mode);
        prop_assert_eq!(ps, 4);
    }

    #[test]
    fn ansi_functional_modes_round_trip_sm_rm(
        idx in 0usize..ANSI_FUNCTIONAL.len(),
        writes in proptest::collection::vec(any::<bool>(), 0..8),
    ) {
        let mode = ANSI_FUNCTIONAL[idx];
        let mut cmd = Vec::new();
        for set in &writes {
            cmd.extend_from_slice(format!("\x1b[{mode}{}", if *set { 'h' } else { 'l' }).as_bytes());
        }
        cmd.extend_from_slice(format!("\x1b[{mode}$p").as_bytes());
        let (_g, responses) = drive(&cmd);
        let (_, m, ps) = parse_decrqm_reply(&responses[0]).unwrap();
        prop_assert_eq!(m, mode);
        let expected = match writes.last() {
            Some(true) => 1u8,
            Some(false) | None => 2u8,
        };
        prop_assert_eq!(ps, expected);
    }

    #[test]
    fn ansi_unimplemented_modes_report_unknown(
        idx in 0usize..ANSI_UNIMPLEMENTED.len(),
        writes in proptest::collection::vec(any::<bool>(), 0..8),
    ) {
        let mode = ANSI_UNIMPLEMENTED[idx];
        let mut cmd = Vec::new();
        for set in &writes {
            cmd.extend_from_slice(format!("\x1b[{mode}{}", if *set { 'h' } else { 'l' }).as_bytes());
        }
        cmd.extend_from_slice(format!("\x1b[{mode}$p").as_bytes());
        let (_g, responses) = drive(&cmd);
        let (private, m, ps) = parse_decrqm_reply(&responses[0]).unwrap();
        prop_assert!(!private);
        prop_assert_eq!(m, mode);
        prop_assert_eq!(ps, 0);
    }

    #[test]
    fn functional_dec_modes_round_trip_via_dedicated_field(
        idx in 0usize..DEC_FUNCTIONAL.len(),
        set in any::<bool>(),
    ) {
        let mode = DEC_FUNCTIONAL[idx];
        let cmd = format!(
            "\x1b[?{mode}{}\x1b[?{mode}$p",
            if set { 'h' } else { 'l' }
        );
        let (_g, responses) = drive(cmd.as_bytes());
        let (private, m, ps) = parse_decrqm_reply(&responses[0]).unwrap();
        prop_assert!(private);
        prop_assert_eq!(m, mode);
        prop_assert_eq!(ps, if set { 1 } else { 2 });
    }

    #[test]
    fn soft_dec_modes_round_trip_decset_decreset(
        idx in 0usize..KNOWN_MODIFIABLE_DEC_MODES.len(),
        writes in proptest::collection::vec(any::<bool>(), 0..8),
    ) {
        let mode = KNOWN_MODIFIABLE_DEC_MODES[idx];
        prop_assume!(!DEC_FUNCTIONAL.contains(&mode));
        let mut cmd = Vec::new();
        for set in &writes {
            cmd.extend_from_slice(format!("\x1b[?{mode}{}", if *set { 'h' } else { 'l' }).as_bytes());
        }
        cmd.extend_from_slice(format!("\x1b[?{mode}$p").as_bytes());
        let (_g, responses) = drive(&cmd);
        let (private, m, ps) = parse_decrqm_reply(&responses[0]).unwrap();
        prop_assert!(private);
        prop_assert_eq!(m, mode);
        let expected = match writes.last() {
            Some(true) => 1u8,
            Some(false) | None => 2u8,
        };
        prop_assert_eq!(ps, expected);
    }

    #[test]
    fn permanently_reset_dec_modes_always_report_ps4(
        idx in 0usize..PERMANENTLY_RESET_DEC_MODES.len(),
        writes in proptest::collection::vec(any::<bool>(), 0..6),
    ) {
        let mode = PERMANENTLY_RESET_DEC_MODES[idx];
        let mut cmd = Vec::new();
        for set in &writes {
            cmd.extend_from_slice(format!("\x1b[?{mode}{}", if *set { 'h' } else { 'l' }).as_bytes());
        }
        cmd.extend_from_slice(format!("\x1b[?{mode}$p").as_bytes());
        let (_g, responses) = drive(&cmd);
        let (private, m, ps) = parse_decrqm_reply(&responses[0]).unwrap();
        prop_assert!(private);
        prop_assert_eq!(m, mode);
        prop_assert_eq!(ps, 4);
    }

    #[test]
    fn permanently_set_dec_modes_always_report_ps3(
        idx in 0usize..PERMANENTLY_SET_DEC_MODES.len(),
        writes in proptest::collection::vec(any::<bool>(), 0..6),
    ) {
        let mode = PERMANENTLY_SET_DEC_MODES[idx];
        let mut cmd = Vec::new();
        for set in &writes {
            cmd.extend_from_slice(format!("\x1b[?{mode}{}", if *set { 'h' } else { 'l' }).as_bytes());
        }
        cmd.extend_from_slice(format!("\x1b[?{mode}$p").as_bytes());
        let (_g, responses) = drive(&cmd);
        let (private, m, ps) = parse_decrqm_reply(&responses[0]).unwrap();
        prop_assert!(private);
        prop_assert_eq!(m, mode);
        prop_assert_eq!(ps, 3);
    }
}

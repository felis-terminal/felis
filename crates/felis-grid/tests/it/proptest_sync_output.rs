//! Synchronized output equivalence properties: `?2026` BSU/ESU must be
//! transparent to the final grid state (`docs/reference/ipc.md` "Backpressure").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use felis_grid::{Cell, Grid};
use felis_vt::Parser;
use proptest::prelude::*;

fn run(rows: u16, cols: u16, bytes: &[u8]) -> (Vec<Cell>, Vec<usize>, felis_grid::Cursor) {
    let mut g = Grid::new(rows, cols);
    g.damage_mut().clear();
    Parser::new().advance(&mut g, bytes);
    let cells: Vec<Cell> = (0..g.rows())
        .flat_map(|r| (0..g.cols()).map(move |c| (r, c)))
        .map(|(r, c)| *g.cell(r, c).unwrap())
        .collect();
    let dirty: Vec<usize> = g.damage().dirty_rows().collect();
    (cells, dirty, g.cursor())
}

fn wrap_in_sync(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 16);
    out.extend_from_slice(b"\x1b[?2026h");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\x1b[?2026l");
    out
}

/// No ESC, so the workload cannot forge a `?2026` toggle mid-stream.
fn workload_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        Just(b'\r'),
        Just(b'\n'),
        Just(0x08),
        Just(b'\t'),
        0x20u8..=0x7Eu8,
    ]
}

proptest! {
    #[test]
    fn cells_match_with_or_without_sync_wrap(
        rows in 1u16..=8,
        cols in 1u16..=24,
        body in proptest::collection::vec(workload_byte(), 0..200),
    ) {
        let (cells_a, _, cursor_a) = run(rows, cols, &body);
        let wrapped = wrap_in_sync(&body);
        let (cells_b, _, cursor_b) = run(rows, cols, &wrapped);
        prop_assert_eq!(cells_a, cells_b);
        prop_assert_eq!(cursor_a, cursor_b);
    }

    #[test]
    fn dirty_row_set_matches_with_or_without_sync_wrap(
        rows in 1u16..=8,
        cols in 1u16..=24,
        body in proptest::collection::vec(workload_byte(), 0..200),
    ) {
        let (_, dirty_a, _) = run(rows, cols, &body);
        let wrapped = wrap_in_sync(&body);
        let (_, dirty_b, _) = run(rows, cols, &wrapped);
        prop_assert_eq!(dirty_a, dirty_b);
    }

    #[test]
    fn sync_state_clears_after_esu(
        rows in 1u16..=4,
        cols in 1u16..=12,
        body in proptest::collection::vec(workload_byte(), 0..32),
    ) {
        let mut g = Grid::new(rows, cols);
        let mut p = Parser::new();
        p.advance(&mut g, &wrap_in_sync(&body));
        prop_assert!(!g.synchronized_output());
        g.damage_mut().clear();
        p.advance(&mut g, b"X");
        prop_assert!(
            g.damage().dirty_rows().next().is_some(),
            "post-ESU write must mark a row dirty"
        );
    }

    #[test]
    fn ready_to_present_implies_sync_cleared(
        rows in 1u16..=4,
        cols in 1u16..=12,
        ops in proptest::collection::vec(any::<bool>(), 0..16),
        millis_advance in 0u64..400,
    ) {
        let mut g = Grid::new(rows, cols);
        let mut p = Parser::new();
        for op in &ops {
            p.advance(&mut g, if *op { b"\x1b[?2026h" } else { b"\x1b[?2026l" });
            p.advance(&mut g, b"a");
        }
        let now = Instant::now() + Duration::from_millis(millis_advance);
        if g.ready_to_present(now) {
            prop_assert!(!g.synchronized_output());
            prop_assert!(g.synchronized_output_deadline(now).is_none());
        }
    }

    #[test]
    fn deadline_is_monotonic_or_none(
        millis_first in 0u64..200,
        millis_advance in 0u64..200,
        re_observe in any::<bool>(),
    ) {
        let mut g = Grid::new(2, 4);
        let mut p = Parser::new();
        let t0 = Instant::now() + Duration::from_millis(millis_first);
        p.advance(&mut g, b"\x1b[?2026h");
        let d0 = g.synchronized_output_deadline(t0).unwrap();
        let t1 = t0 + Duration::from_millis(millis_advance);
        if t1 < d0 {
            let d1 = g.synchronized_output_deadline(t1).unwrap();
            prop_assert_eq!(d0, d1);
        }
        if re_observe {
            let _ = g.synchronized_output_deadline(t1);
        }
        p.advance(&mut g, b"\x1b[?2026l");
        prop_assert!(g.synchronized_output_deadline(t1).is_none());
    }
}

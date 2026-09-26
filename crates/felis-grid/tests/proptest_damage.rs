//! Damage-tracker properties (`docs/reference/ipc.md` describes damage
//! as a row set). PTY reads arrive in arbitrary chunks, so the final
//! damage must not depend on where the bytes are split across `advance`;
//! and the bitset itself must answer exactly what a flag per row would.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use felis_grid::{Damage, Grid, ScrollDirection};
use felis_vt::Parser;
use proptest::prelude::*;

fn workload_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        Just(b'\r'),
        Just(b'\n'),
        Just(0x08),
        Just(b'\t'),
        0x20u8..=0x7Eu8,
    ]
}

fn dirty(rows: u16, cols: u16, bytes: &[u8]) -> BTreeSet<usize> {
    let mut g = Grid::new(rows, cols);
    g.damage_mut().clear();
    Parser::new().advance(&mut g, bytes);
    g.damage().dirty_rows().collect()
}

fn dirty_split(rows: u16, cols: u16, body: &[u8], split: usize) -> BTreeSet<usize> {
    let mut g = Grid::new(rows, cols);
    g.damage_mut().clear();
    let mut p = Parser::new();
    p.advance(&mut g, &body[..split]);
    p.advance(&mut g, &body[split..]);
    g.damage().dirty_rows().collect()
}

proptest! {
    #[test]
    fn dirty_rows_are_split_equivalent(
        rows in 1u16..=8,
        cols in 1u16..=24,
        body in proptest::collection::vec(workload_byte(), 0..200),
    ) {
        if body.is_empty() {
            return Ok(());
        }
        for split in [0, body.len() / 2, body.len()] {
            let whole = dirty(rows, cols, &body);
            let split_set = dirty_split(rows, cols, &body, split);
            prop_assert_eq!(
                whole, split_set,
                "split at {} diverged", split
            );
        }
    }

    /// Pins that `dirty_rows()` is a set view (no duplicate rows).
    #[test]
    fn dirty_rows_union_is_associative(
        rows in 1u16..=8,
        cols in 1u16..=24,
        a in proptest::collection::vec(workload_byte(), 0..64),
        b in proptest::collection::vec(workload_byte(), 0..64),
        c in proptest::collection::vec(workload_byte(), 0..64),
    ) {
        let da = dirty(rows, cols, &a);
        let db = dirty(rows, cols, &b);
        let dc = dirty(rows, cols, &c);
        let left: BTreeSet<usize> = da.union(&db).copied().collect::<BTreeSet<_>>()
            .union(&dc).copied().collect();
        let right: BTreeSet<usize> = db.union(&dc).copied().collect::<BTreeSet<_>>();
        let right: BTreeSet<usize> = da.union(&right).copied().collect();
        prop_assert_eq!(left, right);
    }

    #[test]
    fn clear_zeroes_the_dirty_set(
        rows in 1u16..=8,
        cols in 1u16..=24,
        body in proptest::collection::vec(workload_byte(), 0..64),
    ) {
        let mut g = Grid::new(rows, cols);
        let mut p = Parser::new();
        p.advance(&mut g, &body);
        g.damage_mut().clear();
        prop_assert!(g.damage().dirty_rows().next().is_none());
    }
}

#[derive(Debug, Clone)]
enum Op {
    Mark(usize),
    MarkRange(usize, usize),
    MarkAll,
    Clear,
    Resize(usize),
    ShiftBand(usize, usize, usize, bool),
}

/// Block boundaries are where the tail mask and the per-block loops can
/// disagree, so they get their own weight rather than one draw in 200.
fn row_count() -> impl Strategy<Value = usize> {
    prop_oneof![
        Just(0usize),
        Just(1),
        Just(63),
        Just(64),
        Just(65),
        Just(127),
        Just(128),
        Just(129),
        0usize..=200,
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..=210).prop_map(Op::Mark),
        (0usize..=210, 0usize..=210).prop_map(|(a, b)| Op::MarkRange(a, b)),
        Just(Op::MarkAll),
        Just(Op::Clear),
        row_count().prop_map(Op::Resize),
        (0usize..=210, 0usize..=210, 0usize..=140, any::<bool>())
            .prop_map(|(top, bottom, n, up)| Op::ShiftBand(top, bottom, n, up)),
    ]
}

fn apply(damage: &mut Damage, flags: &mut Vec<bool>, op: &Op) {
    match *op {
        Op::Mark(row) => {
            damage.mark(row);
            if row < flags.len() {
                flags[row] = true;
            }
        }
        Op::MarkRange(start, end) => {
            damage.mark_range(start, end);
            let start = start.min(flags.len());
            let end = end.min(flags.len());
            for flag in &mut flags[start..end.max(start)] {
                *flag = true;
            }
        }
        Op::MarkAll => {
            damage.mark_all();
            flags.fill(true);
        }
        Op::Clear => {
            damage.clear();
            flags.fill(false);
        }
        Op::Resize(rows) => {
            damage.resize(rows);
            flags.clear();
            flags.resize(rows, true);
        }
        Op::ShiftBand(top, bottom, n, up) => {
            let direction = if up {
                ScrollDirection::Up
            } else {
                ScrollDirection::Down
            };
            damage.shift_band(top, bottom, n, direction);
            let end = (bottom + 1).min(flags.len());
            if top < end && n > 0 {
                let band = &mut flags[top..end];
                let n = n.min(band.len());
                if up {
                    band.rotate_left(n);
                    let len = band.len();
                    band[len - n..].fill(true);
                } else {
                    band.rotate_right(n);
                    band[..n].fill(true);
                }
            }
        }
    }
}

fn dirty_set(flags: &[bool]) -> BTreeSet<usize> {
    flags
        .iter()
        .enumerate()
        .filter_map(|(row, dirty)| dirty.then_some(row))
        .collect()
}

proptest! {
    /// The bitset is an optimization of a flag per row; every operation
    /// must leave the two indistinguishable.
    #[test]
    fn op_sequence_matches_a_flag_per_row(
        rows in row_count(),
        ops in proptest::collection::vec(op(), 0..12),
        start in 0usize..=210,
        end in 0usize..=210,
    ) {
        let mut damage = Damage::new(rows);
        let mut flags = vec![false; rows];
        for op in &ops {
            apply(&mut damage, &mut flags, op);
            prop_assert_eq!(
                damage.dirty_rows().collect::<BTreeSet<_>>(),
                dirty_set(&flags),
                "after {:?}", op
            );
        }
        let clamped_start = start.min(flags.len());
        let clamped_end = end.min(flags.len());
        prop_assert_eq!(
            damage.any_dirty_in_range(start, end),
            (clamped_start..clamped_end).any(|row| flags[row]),
        );
        prop_assert_eq!(
            damage.all_dirty_in_range(start, end),
            (clamped_start..clamped_end).all(|row| flags[row]),
        );
    }

    /// A same-length merge is a union; a length mismatch means the grid
    /// resized under the subscriber, which owes it a full redraw.
    #[test]
    fn merge_unions_or_resizes_to_all_dirty(
        rows in row_count(),
        other_rows in row_count(),
        mine in proptest::collection::vec(op(), 0..6),
        theirs in proptest::collection::vec(op(), 0..6),
    ) {
        let mut damage = Damage::new(rows);
        let mut flags = vec![false; rows];
        for op in &mine {
            apply(&mut damage, &mut flags, op);
        }
        let mut other = Damage::new(other_rows);
        let mut other_flags = vec![false; other_rows];
        for op in &theirs {
            apply(&mut other, &mut other_flags, op);
        }

        damage.merge(&other);
        let expected = if flags.len() == other_flags.len() {
            dirty_set(
                &flags
                    .iter()
                    .zip(&other_flags)
                    .map(|(a, b)| *a || *b)
                    .collect::<Vec<_>>(),
            )
        } else {
            (0..other_flags.len()).collect()
        };
        prop_assert_eq!(damage.dirty_rows().collect::<BTreeSet<_>>(), expected);
    }
}

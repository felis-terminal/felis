//! Property: the `$`-intermediate rectangle operations respect the
//! rectangle boundary and the active pen / protection invariants.
//!
//! Spec source: DEC VT420 Programmer Reference §8.6 ("Rectangular
//! Editing").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::{AttrFlags, Grapheme, Grid};
use felis_vt::Parser;
use proptest::prelude::*;

mod common;
use common::drive_with;

fn snapshot_flags(g: &Grid) -> Vec<Vec<AttrFlags>> {
    (0..g.rows())
        .map(|r| {
            (0..g.cols())
                .map(|c| g.style(g.cell(r, c).unwrap().style).flags)
                .collect()
        })
        .collect()
}

fn snapshot_chars(g: &Grid) -> Vec<Vec<Grapheme>> {
    (0..g.rows())
        .map(|r| {
            (0..g.cols())
                .map(|c| g.cell(r, c).unwrap().grapheme)
                .collect()
        })
        .collect()
}

proptest! {
    #[test]
    fn decera_full_grid_clears_every_cell(
        rows in 1u16..=6,
        cols in 1u16..=6,
    ) {
        let mut g = Grid::new(rows, cols);
        let mut p = Parser::new();
        drive_with(&mut p, &mut g,b"x".repeat(usize::from(rows) * usize::from(cols)).as_slice());
        let cmd = format!("\x1b[1;1;{rows};{cols}$z");
        drive_with(&mut p, &mut g,cmd.as_bytes());
        for r in 0..rows {
            for c in 0..cols {
                prop_assert_eq!(g.cell(r, c).unwrap().grapheme, Grapheme::Empty);
            }
        }
    }

    #[test]
    fn decera_preserves_cells_outside_rectangle(
        rows in 2u16..=6,
        cols in 2u16..=6,
        pt in 1u16..=6,
        pl in 1u16..=6,
        pb in 1u16..=6,
        pr in 1u16..=6,
    ) {
        let mut g = Grid::new(rows, cols);
        let mut p = Parser::new();
        for r in 0..rows {
            for c in 0..cols {
                let ch = b'a' + ((r * cols + c) as u8 % 26);
                let cup = format!("\x1b[{};{}H", r + 1, c + 1);
                drive_with(&mut p, &mut g,cup.as_bytes());
                drive_with(&mut p, &mut g,&[ch]);
            }
        }
        let before = snapshot_chars(&g);
        let cmd = format!("\x1b[{pt};{pl};{pb};{pr}$z");
        drive_with(&mut p, &mut g,cmd.as_bytes());
        let after = snapshot_chars(&g);
        let top = pt.min(rows).saturating_sub(1);
        let bottom = pb.min(rows).saturating_sub(1);
        let left = pl.min(cols).saturating_sub(1);
        let right = pr.min(cols).saturating_sub(1);
        let valid = pt <= pb && pl <= pr;
        for r in 0..rows {
            for c in 0..cols {
                let inside = valid && r >= top && r <= bottom && c >= left && c <= right;
                if inside {
                    prop_assert_eq!(after[usize::from(r)][usize::from(c)], Grapheme::Empty);
                } else {
                    prop_assert_eq!(
                        after[usize::from(r)][usize::from(c)],
                        before[usize::from(r)][usize::from(c)],
                    );
                }
            }
        }
    }

    #[test]
    fn decfra_fills_inside_and_preserves_outside(
        rows in 2u16..=6,
        cols in 2u16..=6,
        pt in 1u16..=6,
        pl in 1u16..=6,
        pb in 1u16..=6,
        pr in 1u16..=6,
        pch in 0x21u8..=0x7E,
    ) {
        let mut g = Grid::new(rows, cols);
        let mut p = Parser::new();
        for _ in 0..(usize::from(rows) * usize::from(cols)) {
            drive_with(&mut p, &mut g,b"a");
        }
        let before = snapshot_chars(&g);
        let cmd = format!("\x1b[{};{pt};{pl};{pb};{pr}$x", u32::from(pch));
        drive_with(&mut p, &mut g,cmd.as_bytes());
        let after = snapshot_chars(&g);
        let top = pt.min(rows).saturating_sub(1);
        let bottom = pb.min(rows).saturating_sub(1);
        let left = pl.min(cols).saturating_sub(1);
        let right = pr.min(cols).saturating_sub(1);
        let valid = pt <= pb && pl <= pr;
        for r in 0..rows {
            for c in 0..cols {
                let inside = valid && r >= top && r <= bottom && c >= left && c <= right;
                if inside {
                    prop_assert_eq!(
                        after[usize::from(r)][usize::from(c)],
                        Grapheme::Ascii(pch),
                    );
                } else {
                    prop_assert_eq!(
                        after[usize::from(r)][usize::from(c)],
                        before[usize::from(r)][usize::from(c)],
                    );
                }
            }
        }
    }

    #[test]
    fn deccra_disjoint_copy_matches_source(
        rows in 4u16..=8,
        cols in 4u16..=8,
        dt in 3u16..=6,
        dl in 3u16..=6,
    ) {
        prop_assume!(dt < rows);
        prop_assume!(dl < cols);
        let mut g = Grid::new(rows, cols);
        let mut p = Parser::new();
        drive_with(&mut p, &mut g,b"\x1b[1;1HA\x1b[1;2HB\x1b[2;1HC\x1b[2;2HD");
        let mut src_before = Vec::with_capacity(4);
        for r in 0u16..2 {
            for c in 0u16..2 {
                src_before.push(g.cell(r, c).unwrap().grapheme);
            }
        }
        let cmd = format!("\x1b[1;1;2;2;1;{dt};{dl};1$v");
        drive_with(&mut p, &mut g,cmd.as_bytes());
        for r in 0u16..2 {
            for c in 0u16..2 {
                let dest_r = r + dt - 1;
                let dest_c = c + dl - 1;
                prop_assert_eq!(
                    g.cell(dest_r, dest_c).unwrap().grapheme,
                    src_before[usize::from(r) * 2 + usize::from(c)],
                );
            }
        }
        for r in 0u16..2 {
            for c in 0u16..2 {
                prop_assert_eq!(
                    g.cell(r, c).unwrap().grapheme,
                    src_before[usize::from(r) * 2 + usize::from(c)],
                );
            }
        }
    }

    #[test]
    fn deccara_sets_flags_inside_and_preserves_outside(
        rows in 2u16..=6,
        cols in 2u16..=6,
        pt in 1u16..=6,
        pl in 1u16..=6,
        pb in 1u16..=6,
        pr in 1u16..=6,
    ) {
        let mut g = Grid::new(rows, cols);
        let mut p = Parser::new();
        for _ in 0..(usize::from(rows) * usize::from(cols)) {
            drive_with(&mut p, &mut g, b"a");
        }
        let before = snapshot_flags(&g);
        let cmd = format!("\x1b[2*x\x1b[{pt};{pl};{pb};{pr};7$r");
        drive_with(&mut p, &mut g, cmd.as_bytes());
        let after = snapshot_flags(&g);
        let top = pt.min(rows).saturating_sub(1);
        let bottom = pb.min(rows).saturating_sub(1);
        let left = pl.min(cols).saturating_sub(1);
        let right = pr.min(cols).saturating_sub(1);
        let valid = pt <= pb && pl <= pr;
        for r in 0..rows {
            for c in 0..cols {
                let inside = valid && r >= top && r <= bottom && c >= left && c <= right;
                let expected = if inside {
                    AttrFlags::REVERSE
                } else {
                    before[usize::from(r)][usize::from(c)]
                };
                prop_assert_eq!(after[usize::from(r)][usize::from(c)], expected);
            }
        }
    }

    #[test]
    fn decrara_reverses_flags_inside_and_preserves_outside(
        rows in 2u16..=6,
        cols in 2u16..=6,
        pt in 1u16..=6,
        pl in 1u16..=6,
        pb in 1u16..=6,
        pr in 1u16..=6,
    ) {
        let mut g = Grid::new(rows, cols);
        let mut p = Parser::new();
        drive_with(&mut p, &mut g, b"\x1b[1m");
        for _ in 0..(usize::from(rows) * usize::from(cols)) {
            drive_with(&mut p, &mut g, b"a");
        }
        let before = snapshot_flags(&g);
        let cmd = format!("\x1b[2*x\x1b[{pt};{pl};{pb};{pr};1$t");
        drive_with(&mut p, &mut g, cmd.as_bytes());
        let after = snapshot_flags(&g);
        let top = pt.min(rows).saturating_sub(1);
        let bottom = pb.min(rows).saturating_sub(1);
        let left = pl.min(cols).saturating_sub(1);
        let right = pr.min(cols).saturating_sub(1);
        let valid = pt <= pb && pl <= pr;
        for r in 0..rows {
            for c in 0..cols {
                let inside = valid && r >= top && r <= bottom && c >= left && c <= right;
                let was = before[usize::from(r)][usize::from(c)];
                let expected = if inside { was ^ AttrFlags::BOLD } else { was };
                prop_assert_eq!(after[usize::from(r)][usize::from(c)], expected);
            }
        }
    }

}

//! Property tests for grid invariants.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::{Cell, Grapheme, Grid, ScrollDirection, Sizing, SizingHandle};
use felis_vt::Parser;
use proptest::prelude::*;

mod common;
use common::{drive, scroll_ops};

/// Models `ShadowScreen::apply` (`docs/reference/ipc.md`): drain queued
/// `Scrolled` directives first, then `RowDelta` for every still-dirty row.
fn replay_events_onto_shadow(daemon: &mut Grid, mut shadow: Vec<Cell>) -> Vec<Cell> {
    let cols = usize::from(daemon.cols());
    for op in scroll_ops(daemon) {
        let region_top = usize::from(op.region_top);
        let region_bottom = usize::from(op.region_bottom);
        let region_height = region_bottom - region_top + 1;
        let n = usize::from(op.n_rows).min(region_height);
        match op.direction {
            ScrollDirection::Up => {
                if n < region_height {
                    for r in region_top..(region_bottom + 1 - n) {
                        let src = (r + n) * cols;
                        let dst = r * cols;
                        let row = shadow[src..src + cols].to_vec();
                        shadow[dst..dst + cols].clone_from_slice(&row);
                    }
                }
                for r in (region_bottom + 1 - n)..=region_bottom {
                    let start = r * cols;
                    for slot in &mut shadow[start..start + cols] {
                        *slot = Cell::default();
                    }
                }
            }
            ScrollDirection::Down => {
                if n < region_height {
                    for r in ((region_top + n)..=region_bottom).rev() {
                        let src = (r - n) * cols;
                        let dst = r * cols;
                        let row = shadow[src..src + cols].to_vec();
                        shadow[dst..dst + cols].clone_from_slice(&row);
                    }
                }
                for r in region_top..(region_top + n) {
                    let start = r * cols;
                    for slot in &mut shadow[start..start + cols] {
                        *slot = Cell::default();
                    }
                }
            }
        }
    }
    let dirty: Vec<usize> = daemon.damage().dirty_rows().collect();
    for r in dirty {
        let start = r * cols;
        for c in 0..cols {
            shadow[start + c] = daemon
                .cell(u16::try_from(r).unwrap(), u16::try_from(c).unwrap())
                .copied()
                .unwrap_or_default();
        }
    }
    shadow
}

fn reflow_visible_snapshot(g: &Grid) -> (Vec<Grapheme>, Vec<bool>) {
    let mut cells = Vec::new();
    let mut wraps = Vec::new();
    for r in 0..g.rows() {
        for c in 0..g.cols() {
            cells.push(g.cell(r, c).unwrap().grapheme);
        }
        wraps.push(g.row_soft_wrap_continued(r));
    }
    (cells, wraps)
}

proptest! {
    #[test]
    fn cursor_always_in_bounds(
        rows in 1u16..=64,
        cols in 1u16..=160,
        bytes in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let grid = drive(rows, cols, &bytes);
        prop_assert!(grid.cursor().row < grid.rows());
        prop_assert!(grid.cursor().col < grid.cols());
    }

    #[test]
    fn cell_lookup_matches_declared_dimensions(
        rows in 1u16..=32,
        cols in 1u16..=80,
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let grid = drive(rows, cols, &bytes);
        for r in 0..grid.rows() {
            for c in 0..grid.cols() {
                prop_assert!(grid.cell(r, c).is_some());
            }
        }
        prop_assert!(grid.cell(grid.rows(), 0).is_none());
        prop_assert!(grid.cell(0, grid.cols()).is_none());
    }

    #[test]
    fn drive_is_deterministic(
        rows in 1u16..=32,
        cols in 1u16..=80,
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let g1 = drive(rows, cols, &bytes);
        let g2 = drive(rows, cols, &bytes);
        prop_assert_eq!(g1.cursor(), g2.cursor());
        prop_assert_eq!(g1.rows(), g2.rows());
        prop_assert_eq!(g1.cols(), g2.cols());
        for r in 0..g1.rows() {
            for c in 0..g1.cols() {
                prop_assert_eq!(g1.cell(r, c), g2.cell(r, c));
            }
        }
    }

    #[test]
    fn resize_keeps_invariants(
        rows in 1u16..=32,
        cols in 1u16..=80,
        new_rows in 1u16..=32,
        new_cols in 1u16..=80,
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let mut grid = drive(rows, cols, &bytes);
        grid.resize(new_rows, new_cols);
        prop_assert_eq!(grid.rows(), new_rows);
        prop_assert_eq!(grid.cols(), new_cols);
        prop_assert!(grid.cursor().row < grid.rows());
        prop_assert!(grid.cursor().col < grid.cols());
        prop_assert!(grid.cell(new_rows - 1, new_cols - 1).is_some());
        prop_assert!(grid.cell(new_rows, 0).is_none());
    }

    /// Pins the trim/pad contract of `grid-and-cells.md` "Reflow on
    /// resize" (expansion only; shrinking discards content).
    #[test]
    fn resize_expand_then_contract_round_trips_original_cells(
        rows in 1u16..=16,
        cols in 1u16..=40,
        wider_extra in 0u16..=24,
        taller_extra in 0u16..=16,
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let mut grid = drive(rows, cols, &bytes);
        let snapshot: Vec<_> = (0..rows)
            .flat_map(|r| (0..cols).map(move |c| (r, c)))
            .map(|(r, c)| grid.cell(r, c).copied().unwrap())
            .collect();

        let big_rows = rows.saturating_add(taller_extra);
        let big_cols = cols.saturating_add(wider_extra);
        grid.resize(big_rows, big_cols);
        prop_assert_eq!(grid.rows(), big_rows);
        prop_assert_eq!(grid.cols(), big_cols);

        grid.resize(rows, cols);
        prop_assert_eq!(grid.rows(), rows);
        prop_assert_eq!(grid.cols(), cols);

        for (i, (r, c)) in (0..rows)
            .flat_map(|r| (0..cols).map(move |c| (r, c)))
            .enumerate()
        {
            prop_assert_eq!(
                grid.cell(r, c).unwrap(),
                &snapshot[i],
                "cell ({},{}) diverged across an expand→contract round-trip",
                r,
                c
            );
        }
    }

    #[test]
    fn resize_marks_every_row_dirty(
        rows in 1u16..=24,
        cols in 1u16..=80,
        new_rows in 1u16..=24,
        new_cols in 1u16..=80,
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let mut grid = drive(rows, cols, &bytes);
        grid.damage_mut().clear();

        grid.resize(new_rows, new_cols);

        if (new_rows, new_cols) == (rows, cols) {
            prop_assert_eq!(grid.damage().dirty_rows().count(), 0);
        } else {
            let dirty: Vec<_> = grid.damage().dirty_rows().collect();
            prop_assert_eq!(
                dirty.len(),
                usize::from(new_rows),
                "expected every row dirty after resize, got {:?}",
                dirty
            );
            for r in 0..new_rows {
                prop_assert!(
                    grid.damage().dirty_rows().any(|d| d == usize::from(r)),
                    "row {} not in dirty set after resize",
                    r
                );
            }
        }
    }

    #[test]
    fn damage_indices_are_in_bounds(
        rows in 1u16..=32,
        cols in 1u16..=80,
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let grid = drive(rows, cols, &bytes);
        let limit = usize::from(grid.rows());
        for r in grid.damage().dirty_rows() {
            prop_assert!(r < limit);
        }
    }

    #[test]
    fn sized_runs_round_trip_through_expand_then_contract(
        rows in 1u16..=12,
        cols in 1u16..=24,
        wider_extra in 0u16..=16,
        taller_extra in 0u16..=12,
        stamps in proptest::collection::vec(
            (0u16..12, 0u16..24, 1u8..=7),
            0..32,
        ),
    ) {
        let mut grid = Grid::new(rows, cols);
        let handles: Vec<SizingHandle> = (1..=7u8)
            .map(|s| {
                grid.install_sizing(
                    Sizing::new(s, 0, 0, 0, felis_grid::VAlign::Top, felis_grid::HAlign::Left)
                        .unwrap(),
                )
                    .expect("registry has room")
            })
            .collect();

        for (r, c, s) in &stamps {
            if *r < rows && *c < cols {
                let h = handles[*s as usize - 1];
                grid.set_cell_sizing(*r, *c, Some(h));
            }
        }
        let snapshot: Vec<((u16, u16), Option<SizingHandle>)> = (0..rows)
            .flat_map(|r| (0..cols).map(move |c| (r, c)))
            .map(|(r, c)| ((r, c), grid.cell_sizing_handle(r, c)))
            .collect();

        let big_rows = rows.saturating_add(taller_extra);
        let big_cols = cols.saturating_add(wider_extra);
        grid.resize(big_rows, big_cols);
        grid.resize(rows, cols);

        for ((r, c), expected) in snapshot {
            prop_assert_eq!(
                grid.cell_sizing_handle(r, c),
                expected,
                "sizing handle at ({},{}) diverged across an \
                 expand→contract round-trip",
                r,
                c
            );
        }
    }

    #[test]
    fn set_cell_sizing_marks_damage_iff_effective_sizing_changed(
        rows in 1u16..=8,
        cols in 1u16..=24,
        ops in proptest::collection::vec(
            (0u16..8, 0u16..24, prop_oneof![Just(None), (1u8..=4).prop_map(Some)]),
            0..32,
        ),
    ) {
        let mut grid = Grid::new(rows, cols);
        let handles: Vec<SizingHandle> = (1..=4u8)
            .map(|s| {
                grid.install_sizing(
                    Sizing::new(s, 0, 0, 0, felis_grid::VAlign::Top, felis_grid::HAlign::Left)
                        .unwrap(),
                )
                    .unwrap()
            })
            .collect();

        for (r, c, s) in ops {
            if r >= rows || c >= cols {
                continue;
            }
            let new_handle = s.map(|s| handles[s as usize - 1]);
            let prior = grid.cell_sizing_handle(r, c);
            grid.damage_mut().clear();
            grid.set_cell_sizing(r, c, new_handle);

            let dirty: Vec<usize> = grid.damage().dirty_rows().collect();
            if prior == new_handle {
                prop_assert!(
                    dirty.is_empty(),
                    "no-op stamp at ({},{}) (prior={:?}, new={:?}) \
                     dirtied rows {:?}",
                    r, c, prior, new_handle, dirty
                );
            } else {
                prop_assert_eq!(
                    dirty,
                    vec![usize::from(r)],
                    "real mutation at ({},{}) (prior={:?}, new={:?}) \
                     should dirty exactly row {}",
                    r,
                    c,
                    prior,
                    new_handle,
                    r
                );
            }
        }
    }

    /// `docs/reference/ipc.md` mirror property: `Scrolled` + dirty-row
    /// `RowDelta`s replayed onto a fresh shadow reproduce the daemon's cells.
    /// The shadow starts from the daemon's cells after `prefix` with the
    /// damage cleared, so the `bytes` tick's directives and rows are all
    /// that bring it level.
    #[test]
    fn pending_events_replay_matches_daemon_cells(
        rows in 1u16..=24,
        cols in 1u16..=80,
        prefix in proptest::collection::vec(any::<u8>(), 0..1024),
        bytes in proptest::collection::vec(any::<u8>(), 0..4096),
    ) {
        let mut daemon = Grid::new(rows, cols);
        let mut parser = Parser::new();
        parser.advance(&mut daemon, &prefix);
        daemon.damage_mut().clear();
        drop(daemon.take_pty_effects());
        let before: Vec<Cell> = (0..daemon.rows())
            .flat_map(|r| (0..daemon.cols()).map(move |c| (r, c)))
            .map(|(r, c)| daemon.cell(r, c).copied().unwrap_or_default())
            .collect();
        parser.advance(&mut daemon, &bytes);

        let shadow = replay_events_onto_shadow(&mut daemon, before);

        let cols_u = usize::from(daemon.cols());
        for r in 0..daemon.rows() {
            for c in 0..daemon.cols() {
                let expected = daemon.cell(r, c).copied().unwrap_or_default();
                let actual = &shadow[usize::from(r) * cols_u + usize::from(c)];
                prop_assert_eq!(
                    actual,
                    &expected,
                    "shadow row {} col {} diverged from daemon",
                    r,
                    c
                );
            }
        }
    }

    #[test]
    fn osc_66_dispatcher_stamps_exactly_the_run_text(
        scale in 1u8..=7,
        text in proptest::collection::vec(0x20u8..=0x7E, 0..16),
    ) {
        // Wide enough that the scaled footprint fits, so the run is
        // stamped rather than discarded (REQ-406).
        let cols = u16::try_from(text.len().max(1)).unwrap()
            .saturating_mul(u16::from(scale))
            .saturating_add(8);
        let rows = u16::from(scale).saturating_add(1);
        let mut grid = Grid::new(rows, cols);
        let mut parser = Parser::new();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b]66;s=");
        bytes.push(b'0' + scale);
        bytes.push(b';');
        bytes.extend_from_slice(&text);
        bytes.push(0x07);

        parser.advance(&mut grid, &bytes);

        prop_assert_eq!(grid.sizing_count(), 1);

        // REQ-603: each ASCII char claims a `scale × scale` block.
        let scale_u16 = u16::from(scale);
        let scale_usize = usize::from(scale);
        prop_assert_eq!(
            grid.sized_cell_count(),
            text.len() * scale_usize * scale_usize
        );
        let handle: SizingHandle = grid
            .cell_sizing_handle(0, 0)
            .unwrap_or_else(|| SizingHandle::new(1).unwrap());
        let text_cells = u16::try_from(text.len()).unwrap().saturating_mul(scale_u16);
        for r in 0..scale_u16 {
            for c in 0..text_cells {
                prop_assert_eq!(
                    grid.cell_sizing_handle(r, c),
                    Some(handle),
                    "cell ({},{}) should carry the run's handle",
                    r,
                    c
                );
            }
        }
        if !text.is_empty() {
            prop_assert_eq!(grid.cell_sizing_handle(0, text_cells), None);
            prop_assert_eq!(grid.cell_sizing_handle(scale_u16, 0), None);
        }

        let registered = grid.sizing_by_handle(handle).unwrap();
        prop_assert_eq!(registered.scale(), scale);
    }

    /// REQ-604: canonically autowrapped plain text round-trips through
    /// any width.
    #[test]
    fn reflow_width_round_trip_restores_plain_text(
        rows in 2u16..=16,
        cols in 2u16..=40,
        alt_cols in 2u16..=40,
        bytes in proptest::collection::vec(
            prop_oneof![0x41u8..=0x5a, Just(b' '), Just(b'\r'), Just(b'\n')],
            0..400,
        ),
    ) {
        let mut grid = drive(rows, cols, &bytes);
        let before = reflow_visible_snapshot(&grid);

        grid.reflow(rows, alt_cols);
        prop_assert_eq!(grid.cols(), alt_cols);

        grid.reflow(rows, cols);
        prop_assert_eq!(grid.cols(), cols);
        prop_assert_eq!(grid.rows(), rows);

        let after = reflow_visible_snapshot(&grid);
        prop_assert_eq!(before, after);
    }

    #[test]
    fn reflow_keeps_invariants_on_arbitrary_input(
        rows in 1u16..=32,
        cols in 1u16..=80,
        new_rows in 1u16..=32,
        new_cols in 1u16..=80,
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let mut grid = drive(rows, cols, &bytes);
        grid.reflow(new_rows, new_cols);
        prop_assert_eq!(grid.rows(), new_rows);
        prop_assert_eq!(grid.cols(), new_cols);
        prop_assert!(grid.cursor().row < grid.rows());
        prop_assert!(grid.cursor().col < grid.cols());
        prop_assert!(grid.cell(new_rows - 1, new_cols - 1).is_some());
        prop_assert!(grid.cell(new_rows, 0).is_none());
    }

    #[test]
    fn leaving_the_alt_screen_after_a_resize_keeps_scrolling(
        rows in 1u16..=32,
        cols in 1u16..=80,
        new_rows in 1u16..=32,
        new_cols in 1u16..=80,
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let mut grid = drive(rows, cols, &bytes);
        let mut parser = Parser::new();
        parser.advance(&mut grid, b"\x1b[?1049h");
        grid.resize(new_rows, new_cols);
        parser.advance(&mut grid, b"\x1b[?1049l");
        prop_assert_eq!(grid.rows(), new_rows);
        prop_assert_eq!(grid.cols(), new_cols);
        prop_assert!(grid.cursor().row < grid.rows());
        prop_assert!(grid.cursor().col < grid.cols());
        for _ in 0..usize::from(new_rows) * 3 {
            parser.advance(&mut grid, b"x\r\n");
        }
        prop_assert!(grid.cell(new_rows - 1, new_cols - 1).is_some());
        prop_assert!(grid.cell(new_rows, 0).is_none());
    }
}

/// Writes, erases, repeats and cell moves that land on either half of a
/// wide pair, with and without DECSLRM margins and the alternate screen.
fn pair_editing_op() -> impl Strategy<Value = String> {
    let text = prop::sample::select(vec![
        "❤\u{fe0f}",
        "🇯🇵",
        "👩\u{200d}💻",
        "字",
        "1\u{fe0f}\u{20e3}",
        "e\u{301}",
        "❄",
        "\u{fe0f}",
        "x",
        "XY",
        "é",
        "\r",
    ])
    .prop_map(str::to_owned);
    prop_oneof![
        3 => text,
        1 => (1u16..=3, 1u16..=10).prop_map(|(r, c)| format!("\x1b[{r};{c}H")),
        1 => (0u16..=3).prop_map(|n| format!("\x1b[{n}X")),
        1 => (0u16..=2).prop_map(|n| format!("\x1b[{n}K")),
        1 => (0u16..=2).prop_map(|n| format!("\x1b[{n}J")),
        1 => (0u16..=2).prop_map(|n| format!("\x1b[?{n}K")),
        1 => (0u16..=2).prop_map(|n| format!("\x1b[?{n}J")),
        1 => (0u16..=1).prop_map(|n| format!("\x1b[{n}\"q")),
        1 => (1u16..=3, 1u16..=10, 0u16..=2, 0u16..=3).prop_map(|(t, l, h, w)| {
            format!("\x1b[{t};{l};{};{}$z", t + h, l + w)
        }),
        1 => (1u16..=3, 1u16..=10, 0u16..=2, 0u16..=3).prop_map(|(t, l, h, w)| {
            format!("\x1b[{t};{l};{};{}${{", t + h, l + w)
        }),
        1 => (1u16..=3, 1u16..=10, 0u16..=2, 0u16..=3).prop_map(|(t, l, h, w)| {
            format!("\x1b[42;{t};{l};{};{}$x", t + h, l + w)
        }),
        1 => (1u16..=3, prop::sample::select(vec!["A", "字"]))
            .prop_map(|(w, t)| format!("\x1b]66;w={w};{t}\x07")),
        1 => (1u16..=3).prop_map(|n| format!("\x1b[{n}b")),
        1 => (1u16..=3).prop_map(|n| format!("\x1b[{n}@")),
        1 => (1u16..=3).prop_map(|n| format!("\x1b[{n}P")),
        1 => prop::sample::select(vec!["\x1b[4h", "\x1b[4l"]).prop_map(str::to_owned),
        1 => (1u16..=3).prop_map(|n| format!("\x1b[{n} @")),
        1 => (1u16..=3).prop_map(|n| format!("\x1b[{n} A")),
        1 => (1u16..=3).prop_map(|n| format!("\x1b[{n}'}}")),
        1 => (1u16..=3).prop_map(|n| format!("\x1b[{n}'~")),
        1 => prop::sample::select(vec!["\x1b6", "\x1b9"]).prop_map(str::to_owned),
        1 => prop::sample::select(vec!["\x1b[?69h", "\x1b[?69l"]).prop_map(str::to_owned),
        1 => (1u16..=10, 0u16..=6).prop_map(|(l, w)| format!("\x1b[{l};{}s", l + w)),
        1 => (1u16..=2, 0u16..=4).prop_map(|(n, op)| {
            format!("\x1b[{n}{}", ["L", "M", "S", "T", "r"][usize::from(op)])
        }),
        1 => (1u16..=3, 1u16..=10, 0u16..=2, 0u16..=3, 1u16..=3, 1u16..=10).prop_map(
            |(t, l, h, w, dt, dl)| format!("\x1b[{t};{l};{};{};1;{dt};{dl}$v", t + h, l + w)
        ),
        1 => prop::sample::select(vec!["\x1b[?1049h", "\x1b[?1049l"]).prop_map(str::to_owned),
    ]
}

/// A byte chunk, or a resize as the daemon applies it: a reflow on the
/// primary screen, a truncating resize on the alternate one.
#[derive(Clone, Debug)]
enum PairStep {
    Bytes(String),
    Resize(u16),
}

fn pair_step() -> impl Strategy<Value = PairStep> {
    prop_oneof![
        12 => pair_editing_op().prop_map(PairStep::Bytes),
        1 => (2u16..=10).prop_map(PairStep::Resize),
    ]
}

proptest! {
    /// A `Spacer` is only ever the right half of a two-cell glyph, so the
    /// cell to its left must hold one; and a wide scalar is always
    /// printed with its `Spacer` (only a cluster whose widen was refused
    /// sits alone). Otherwise an edit split the pair and left text the
    /// renderer and the copy path disagree on.
    #[test]
    fn every_wide_pair_stays_whole(
        cols in 4u16..=10,
        steps in proptest::collection::vec(pair_step(), 0..40),
    ) {
        use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
        let mut grid = Grid::new(3, cols);
        let mut parser = Parser::new();
        for step in &steps {
            match step {
                PairStep::Bytes(bytes) => parser.advance(&mut grid, bytes.as_bytes()),
                PairStep::Resize(c) if grid.on_alternate_screen() => grid.resize(3, *c),
                PairStep::Resize(c) => {
                    grid.reflow(3, *c);
                }
            }
        }
        for r in 0..grid.rows() {
            for c in 0..grid.cols() {
                let g = grid.cell(r, c).unwrap().grapheme;
                if let Grapheme::Char(ch) = g
                    && ch.width() == Some(2)
                    && !grid.cell_sizing(r, c).is_some_and(|s| {
                        s.cell_width() > 0 && u16::from(s.cell_width()) * u16::from(s.scale().max(1)) < 2
                    })
                {
                    let right = (c + 1 < grid.cols()).then(|| grid.cell(r, c + 1).unwrap().grapheme);
                    prop_assert_eq!(right, Some(Grapheme::Spacer), "wide {:?} at ({}, {}) has no Spacer after {:?}", ch, r, c, steps);
                }
                if g != Grapheme::Spacer {
                    continue;
                }
                let left = (c > 0).then(|| grid.cell(r, c - 1).unwrap().grapheme);
                let left_width = match left {
                    Some(Grapheme::Char(ch)) => ch.width().unwrap_or(0),
                    Some(Grapheme::Cluster(id)) => grid.cluster_str(id).map_or(0, |s| s.width().min(2)),
                    _ => 0,
                };
                prop_assert_eq!(left_width, 2, "orphan Spacer at ({}, {}) after {:?}", r, c, steps);
            }
        }
    }
}

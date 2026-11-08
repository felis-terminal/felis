//! Shared helpers for the crate's per-module unit tests.

use felis_vt::Parser;

use crate::compact::{GapReuse, RingRef};
use crate::paged_cells::PagedCells;
use crate::{ApcBody, Grid, PtyEffect, ScreenSwitch, ScrollOp};

pub(crate) fn drive(parser: &mut Parser, grid: &mut Grid, bytes: &[u8]) {
    parser.advance(grid, bytes);
}

pub(crate) fn responses(grid: &mut Grid) -> Vec<Vec<u8>> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Response(bytes) => Some(bytes),
            _ => None,
        })
        .collect()
}

pub(crate) fn apc_bodies(grid: &mut Grid) -> Vec<ApcBody> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Apc(body) => Some(body),
            _ => None,
        })
        .collect()
}

pub(crate) fn scroll_ops(grid: &mut Grid) -> Vec<ScrollOp> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Scrolled { op, .. } => Some(op),
            _ => None,
        })
        .collect()
}

pub(crate) fn screen_switches(grid: &mut Grid) -> Vec<ScreenSwitch> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::ScreenSwitch(switch) => Some(switch),
            _ => None,
        })
        .collect()
}

/// `grid` as it would be held densely, with no cold tier and no vacant
/// rows: each ring laid out history-first from physical row 0, nothing
/// past a row's occupancy, the sizing hint recomputed. Two grids that
/// normalize equal are indistinguishable to every reader.
pub(crate) fn normalized(grid: &Grid) -> Grid {
    fn dense(ring: &RingRef<'_>) -> (PagedCells, Box<[bool]>, Box<[u16]>, usize) {
        let history_len = ring.cold.len() + ring.history_len;
        let phys_cap = history_len + ring.rows;
        let mut cells = PagedCells::with_reserve(phys_cap * ring.cols, 0);
        let mut soft_wrap = vec![false; phys_cap].into_boxed_slice();
        let mut occupancy = vec![0u16; phys_cap].into_boxed_slice();
        for (phys, (row, wrap)) in ring.canonical_rows().enumerate() {
            let start = phys * ring.cols;
            cells[start..start + row.len()].copy_from_slice(row);
            soft_wrap[phys] = wrap;
            occupancy[phys] = u16::try_from(row.len()).unwrap();
        }
        (cells, soft_wrap, occupancy, history_len)
    }
    let mut grid = grid.clone();
    let screen = &mut grid.screen;
    let (cells, soft_wrap, occupancy, history_len) = dense(&screen.ring());
    screen.phys_cap = history_len + usize::from(screen.rows);
    (screen.cells, screen.soft_wrap, screen.occupancy) = (cells, soft_wrap, occupancy);
    (screen.base, screen.history_len) = (history_len, history_len);
    screen.cold.clear();
    screen.gap = GapReuse::INTACT;
    for saved in [
        screen.saved_primary.as_mut(),
        screen.saved_alternate.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        let (cells, soft_wrap, occupancy, history_len) = dense(&saved.ring());
        saved.phys_cap = history_len + usize::from(saved.rows);
        (saved.cells, saved.soft_wrap, saved.occupancy) = (cells, soft_wrap, occupancy);
        (saved.base, saved.history_len) = (history_len, history_len);
        saved.cold.clear();
        saved.gap = GapReuse::INTACT;
    }
    screen.refresh_has_sized_cells();
    grid
}

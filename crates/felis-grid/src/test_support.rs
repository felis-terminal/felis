//! Shared helpers for the crate's per-module unit tests.

use felis_vt::Parser;

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

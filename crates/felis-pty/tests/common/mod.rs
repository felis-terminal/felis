//! Helpers shared by the esctest and vttest conformance harnesses.

use std::path::PathBuf;

use felis_grid::Grid;

pub(crate) const ROWS: u16 = 24;
pub(crate) const COLS: u16 = 80;

/// Forward the host-bound responses the parse queued. No graphics in
/// these suites; the other effect kinds have no consumer here.
pub(crate) fn host_replies(grid: &mut Grid) -> Vec<Vec<u8>> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            felis_grid::PtyEffect::Response(bytes) => Some(bytes),
            _ => None,
        })
        .collect()
}

pub(crate) fn resolve_binary(override_var: &str, name: &str) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var(override_var) {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

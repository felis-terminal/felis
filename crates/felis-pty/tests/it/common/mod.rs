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

pub(crate) fn require_binary(override_var: &str, name: &str) -> PathBuf {
    if let Some(explicit) = std::env::var_os(override_var) {
        let p = PathBuf::from(explicit);
        assert!(p.is_file(), "{override_var}={} is not a file", p.display());
        return p;
    }
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| panic!("{name} not on PATH; the dev shell provides it"))
}

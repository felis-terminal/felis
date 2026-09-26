//! Shared helpers for the daemon integration tests. Each test binary
//! pulls in only what it needs, so `dead_code` is expected here.
#![allow(dead_code)]

use std::path::PathBuf;
// Unix-gated with `wait_connectable`: the graphics suites sharing this
// module must still type-check for `just check-windows`.
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::time::Duration;

use felis_client_core::ShadowScreen;
use felis_grid::Grapheme;

pub(crate) fn daemon_bin() -> String {
    std::env::var("CARGO_BIN_EXE_felis-daemon").expect(
        "CARGO_BIN_EXE_felis-daemon is set by cargo when running \
         this integration test",
    )
}

/// Wait for the test-owned daemon before starting a relay; a relay
/// that connects first auto-spawns its own daemon, which outlives the
/// test as a detached process.
#[cfg(unix)]
pub(crate) async fn wait_connectable(socket: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "persistent daemon never came up on {}",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(crate) fn resolve_binary(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Clusters expand through the grid's cluster table; collapsing them
/// to spaces stops matching any marker that shapes into a cluster.
pub(crate) fn shadow_rows(shadow: &ShadowScreen) -> Vec<String> {
    let grid = shadow.screen();
    let mut rows = Vec::with_capacity(usize::from(grid.rows()));
    for r in 0..grid.rows() {
        let mut row = String::with_capacity(usize::from(grid.cols()));
        for c in 0..grid.cols() {
            let cell = grid.cell(r, c).unwrap();
            match &cell.grapheme {
                Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => row.push(' '),
                Grapheme::Ascii(b) => row.push(*b as char),
                Grapheme::Char(ch) => row.push(*ch),
                Grapheme::Cluster(id) => {
                    if let Some(s) = grid.cluster_str(*id) {
                        row.push_str(s);
                    }
                }
            }
        }
        rows.push(row);
    }
    rows
}

pub(crate) fn shadow_contains(shadow: &ShadowScreen, needle: &str) -> bool {
    shadow_rows(shadow).iter().any(|row| row.contains(needle))
}

/// The wire envelope the parser consumes; [`body`] is the inner body
/// the dispatcher sees.
pub(crate) fn apc(controls: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b_");
    out.extend_from_slice(controls.as_bytes());
    out.push(b';');
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\x1b\\");
    out
}

pub(crate) fn body(controls: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(controls.len() + 1 + payload.len());
    out.extend_from_slice(controls.as_bytes());
    out.push(b';');
    out.extend_from_slice(payload);
    out
}

/// Hand-rolled rather than round-tripped through
/// `felis_vt::kitty_graphics::base64`: an encoder built on the decoder
/// under test would hide a bidirectional regression.
pub(crate) fn b64(input: &[u8]) -> Vec<u8> {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHA[(b0 >> 2) as usize]);
        out.push(ALPHA[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize]);
        if chunk.len() == 1 {
            out.push(b'=');
            out.push(b'=');
        } else {
            out.push(ALPHA[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize]);
            if chunk.len() == 2 {
                out.push(b'=');
            } else {
                out.push(ALPHA[(b2 & 0x3F) as usize]);
            }
        }
    }
    out
}

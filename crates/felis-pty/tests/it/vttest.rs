//! Black-box harness: spawn `vttest` in a PTY, pipe output through
//! `Parser` and `Grid`, and reply to capability probes (DA1, DSR, etc.).
//! A missing `vttest` (`VTTEST_BIN` overrides `PATH`) fails the tests.

use std::time::Duration;

use felis_grid::{Grapheme, Grid, ReplyIdentity};
use tokio::io::AsyncWriteExt;

use crate::common::{COLS, Harness, ROWS, conformance_command, require_binary};

mod probes;
mod reports;
mod screens_vt100;
mod screens_vt220_iso6429;

struct VttestSession {
    harness: Harness,
}

impl VttestSession {
    fn open() -> Self {
        let mut grid = Grid::new(ROWS, COLS);
        grid.set_reply_identity(ReplyIdentity::new("felis 0.1.0", "felis-1"));
        let cmd = conformance_command(require_binary("VTTEST_BIN", "vttest"));
        Self {
            harness: Harness::spawn(cmd, grid),
        }
    }

    fn with_grid<T>(&self, f: impl FnOnce(&Grid) -> T) -> T {
        let g = self.harness.grid.lock().expect("grid lock");
        f(&g)
    }

    /// The PTY line discipline is in cooked mode, so a bare `\r` is
    /// what vttest's `gets`-style readers want.
    async fn send(&self, keys: &str) {
        let mut w = self.harness.writer.lock().await;
        w.write_all(keys.as_bytes()).await.expect("write keys");
        w.flush().await.expect("flush keys");
    }

    async fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            let hit = self.with_grid(|g| grid_contains(g, needle));
            if hit {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Wait for `needle`, failing with `what` and the grid if it never
    /// draws.
    async fn expect_text(&self, needle: &str, timeout: Duration, what: &str) {
        if !self.wait_for(needle, timeout).await {
            let dump = self.with_grid(dump_grid);
            panic!("{what}\nGrid:\n{dump}");
        }
    }

    #[track_caller]
    fn check_or_dump<F>(&self, predicate: F)
    where
        F: FnOnce(&Grid) -> Result<(), String>,
    {
        let outcome = self.with_grid(|g| match predicate(g) {
            Ok(()) => None,
            Err(why) => Some((why, dump_grid(g))),
        });
        if let Some((why, dump)) = outcome {
            panic!("{why}\nGrid:\n{dump}");
        }
    }

    /// Walk nested menus by label, resolving each index from the drawn
    /// screen (vttest's submenu numbering shifts between releases). The
    /// first label is matched against the main menu, so callers do not
    /// send the initial `\r`.
    async fn navigate(&self, labels: &[&str]) {
        self.expect_text(
            "VT100 test program",
            Duration::from_secs(3),
            "intro banner never reached the grid",
        )
        .await;
        self.send("\r").await;
        for label in labels {
            self.select(label).await;
        }
    }

    /// Choose the entry labeled `label` on the menu being drawn.
    async fn select(&self, label: &str) {
        self.expect_text(
            label,
            Duration::from_secs(3),
            &format!("menu '{label}' never drew"),
        )
        .await;
        let idx = self
            .with_grid(|g| find_menu_index(g, label))
            .unwrap_or_else(|| panic!("menu index for '{label}' not found"));
        self.send(&format!("{idx}\r")).await;
    }

    async fn shutdown(mut self) {
        drop(self.harness.writer.lock().await.write_all(b"0\r").await);
        let _exited = self.harness.wait_for_exit(Duration::from_millis(500)).await;
    }
}

/// The row as text; blank and spacer cells read as spaces.
fn row_text(grid: &Grid, r: u16) -> String {
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
    row
}

/// The single character a cell holds; `None` for a blank, spacer or
/// cluster cell.
fn char_at(grid: &Grid, r: u16, c: u16) -> Option<char> {
    match grid.cell(r, c).unwrap().grapheme {
        Grapheme::Ascii(b) => Some(char::from(b)),
        Grapheme::Char(ch) => Some(ch),
        _ => None,
    }
}

fn grid_contains(grid: &Grid, needle: &str) -> bool {
    (0..grid.rows()).any(|r| row_text(grid, r).contains(needle))
}

/// Find the menu index whose label contains `needle`. vttest formats
/// each menu line as `"  <N>. <Label>"`.
fn find_menu_index(grid: &Grid, needle: &str) -> Option<u32> {
    (0..grid.rows()).find_map(|r| {
        let row = row_text(grid, r);
        if !row.contains(needle) {
            return None;
        }
        let digits: String = row
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse::<u32>().ok()
    })
}

/// Render the grid for diagnostics; empty / spacer cells render as `.`
/// so the output is grep-friendly.
fn dump_grid(grid: &Grid) -> String {
    let mut out = String::with_capacity((usize::from(grid.cols()) + 1) * usize::from(grid.rows()));
    for r in 0..grid.rows() {
        for c in 0..grid.cols() {
            let cell = grid.cell(r, c).unwrap();
            match &cell.grapheme {
                Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => out.push('.'),
                Grapheme::Ascii(b) if (0x20..0x7f).contains(b) => out.push(*b as char),
                Grapheme::Ascii(_) => out.push('?'),
                Grapheme::Char(ch) => out.push(*ch),
                Grapheme::Cluster(id) => {
                    if let Some(s) = grid.cluster_str(*id) {
                        out.push_str(s);
                    }
                }
            }
        }
        out.push('\n');
    }
    out
}

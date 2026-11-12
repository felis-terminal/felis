//! Black-box harness: spawn `vttest` in a PTY, pipe output through
//! `Parser` and `Grid`, and reply to capability probes (DA1, DSR, etc.).
//! A missing `vttest` (`VTTEST_BIN` overrides `PATH`) fails the tests.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use felis_grid::{Grapheme, Grid, ReplyIdentity};
use felis_pty::spawn;
use felis_pty::{Command, PtyParts, Size};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;

use crate::common::{COLS, ROWS, host_replies, require_binary};

mod probes;
mod reports;
mod screens_vt100;
mod screens_vt220_iso6429;

struct VttestSession {
    read_task: Option<JoinHandle<()>>,
    grid: Arc<Mutex<Grid>>,
    writer: Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
}

impl VttestSession {
    fn spawn_for(binary: PathBuf) -> Self {
        let mut cmd = Command::new(binary);
        cmd.env_clear();
        // TERM=xterm so vttest's terminfo lookup succeeds; LC_ALL=C
        // avoids the UTF-8 startup path, whose charset-switch
        // sequences felis treats as no-ops.
        cmd.env("TERM", "xterm");
        cmd.env("LC_ALL", "C");
        cmd.env("PATH", "/usr/bin:/bin");

        let (chunks_tx, mut chunks_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let session = spawn(
            cmd,
            Size {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            },
            Box::new(move |bytes: &[u8]| drop(chunks_tx.send(bytes.to_vec()))),
        )
        .expect("spawn vttest");
        let PtyParts { writer, .. } = session;

        let mut grid = Grid::new(ROWS, COLS);
        grid.set_reply_identity(ReplyIdentity::new("felis 0.1.0", "felis-1"));
        let grid = Arc::new(Mutex::new(grid));
        let writer_arc: Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>> =
            Arc::new(tokio::sync::Mutex::new(Box::new(writer)));

        let grid_task = Arc::clone(&grid);
        let writer_task = Arc::clone(&writer_arc);
        let read_task = tokio::spawn(async move {
            let mut parser = felis_vt::Parser::new();
            // The channel closes when the parse thread exits (PTY EOF).
            while let Some(chunk) = chunks_rx.recv().await {
                let replies = {
                    let mut g = grid_task.lock().expect("grid lock");
                    parser.advance(&mut *g, &chunk);
                    host_replies(&mut g)
                };
                if !replies.is_empty() {
                    let mut w = writer_task.lock().await;
                    for reply in &replies {
                        if w.write_all(reply).await.is_err() {
                            return;
                        }
                    }
                    if w.flush().await.is_err() {
                        return;
                    }
                }
            }
        });

        Self {
            read_task: Some(read_task),
            grid,
            writer: writer_arc,
        }
    }

    fn open() -> Self {
        Self::spawn_for(require_binary("VTTEST_BIN", "vttest"))
    }

    fn with_grid<T>(&self, f: impl FnOnce(&Grid) -> T) -> T {
        let g = self.grid.lock().expect("grid lock");
        f(&g)
    }

    /// The PTY line discipline is in cooked mode, so a bare `\r` is
    /// what vttest's `gets`-style readers want.
    async fn send(&self, keys: &str) {
        let mut w = self.writer.lock().await;
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
        assert!(
            self.wait_for("VT100 test program", Duration::from_secs(3))
                .await,
            "intro banner never reached the grid"
        );
        self.send("\r").await;
        for label in labels {
            assert!(
                self.wait_for(label, Duration::from_secs(3)).await,
                "menu '{label}' never drew",
            );
            let idx = self
                .with_grid(|g| find_menu_index(g, label))
                .unwrap_or_else(|| panic!("menu index for '{label}' not found"));
            self.send(&format!("{idx}\r")).await;
        }
    }

    async fn shutdown(mut self) {
        drop(self.writer.lock().await.write_all(b"0\r").await);
        if let Some(handle) = self.read_task.take() {
            drop(tokio::time::timeout(Duration::from_millis(500), handle).await);
        }
    }
}

impl Drop for VttestSession {
    fn drop(&mut self) {
        if let Some(handle) = self.read_task.take() {
            handle.abort();
        }
    }
}

fn grid_contains(grid: &Grid, needle: &str) -> bool {
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
        if row.contains(needle) {
            return true;
        }
    }
    false
}

/// Find the menu index whose label contains `needle`. vttest formats
/// each menu line as `"  <N>. <Label>"`.
fn find_menu_index(grid: &Grid, needle: &str) -> Option<u32> {
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
        if !row.contains(needle) {
            continue;
        }
        let trimmed = row.trim_start();
        let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = digits.parse::<u32>() {
            return Some(n);
        }
    }
    None
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

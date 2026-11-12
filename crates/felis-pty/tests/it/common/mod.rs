//! Helpers shared by the esctest and vttest conformance harnesses.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use felis_grid::Grid;
use felis_pty::{Command, PtyParts, Size, spawn};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;

pub(crate) const ROWS: u16 = 24;
pub(crate) const COLS: u16 = 80;

pub(crate) type SharedWriter = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// Forward the host-bound responses the parse queued. No graphics in
/// these suites; the other effect kinds have no consumer here.
fn host_replies(grid: &mut Grid) -> Vec<Vec<u8>> {
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

pub(crate) fn conformance_command(binary: PathBuf) -> Command {
    let mut cmd = Command::new(binary);
    cmd.env_clear();
    // TERM=xterm so the suite's terminfo lookup succeeds; LC_ALL=C
    // avoids vttest's UTF-8 startup path, whose charset-switch
    // sequences felis treats as no-ops.
    cmd.env("TERM", "xterm");
    cmd.env("LC_ALL", "C");
    cmd.env("PATH", "/usr/bin:/bin");
    cmd
}

/// A conformance suite running on a `ROWS`×`COLS` PTY: its output is
/// parsed into `grid` and every host-bound reply is written back.
pub(crate) struct Harness {
    read_task: Option<JoinHandle<()>>,
    pub(crate) grid: Arc<Mutex<Grid>>,
    pub(crate) writer: SharedWriter,
}

impl Harness {
    pub(crate) fn spawn(cmd: Command, grid: Grid) -> Self {
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
        .expect("spawn the conformance suite");
        let PtyParts { writer, .. } = session;

        let grid = Arc::new(Mutex::new(grid));
        let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(Box::new(writer)));

        let grid_task = Arc::clone(&grid);
        let writer_task = Arc::clone(&writer);
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
            writer,
        }
    }

    /// Whether the suite reached PTY EOF within `timeout`.
    pub(crate) async fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let Some(handle) = self.read_task.take() else {
            return true;
        };
        tokio::time::timeout(timeout, handle).await.is_ok()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(handle) = self.read_task.take() {
            handle.abort();
        }
    }
}

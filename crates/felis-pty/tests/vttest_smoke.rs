//! Black-box harness: spawn `vttest` in a PTY, pipe output through
//! `Parser` and `Grid`, and reply to capability probes (DA1, DSR, etc.).
//! Skipped when no `vttest` binary is on `PATH` (`VTTEST_BIN` overrides).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use felis_grid::{Grapheme, Grid};
use felis_pty::spawn;
use felis_pty::{Command, Size};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;

mod common;
use common::{COLS, ROWS, host_replies, resolve_binary};

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
        let (_reader, writer, _child, _resizer) = session.split();

        let grid = Arc::new(Mutex::new(Grid::new(ROWS, COLS)));
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

    #[expect(
        clippy::print_stderr,
        reason = "the skip notice must reach the test log; allow-print-in-tests covers only #[test] fns, not this shared helper"
    )]
    fn open(what: &str) -> Option<Self> {
        let Some(binary) = resolve_binary("VTTEST_BIN", "vttest") else {
            eprintln!("skipping {what}: vttest binary not found in PATH or VTTEST_BIN");
            return None;
        };
        Some(Self::spawn_for(binary))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intro_banner_reaches_the_grid() {
    let Some(session) = VttestSession::open("vttest smoke") else {
        return;
    };

    // "VT100 test program" rather than the full banner, so a changed
    // version suffix still passes.
    let hit = session
        .wait_for("VT100 test program", Duration::from_secs(3))
        .await;
    assert!(hit, "expected vttest intro banner on the grid");

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn da1_reply_round_trips_through_terminal_reports_menu() {
    let Some(session) = VttestSession::open("vttest DA1 round-trip") else {
        return;
    };
    session
        .navigate(&["Test of terminal reports", "Primary Device Attributes"])
        .await;

    // The DA1 reply is `CSI ?64;1;2;…;29c`. Match vttest's
    // interpretation of the leading 64 ("VT400 family") rather than
    // its per-byte echo, whose whitespace decoration varies by version.
    let hit = session
        .wait_for("VT400 family", Duration::from_secs(5))
        .await;
    if !hit {
        let dump = session.with_grid(dump_grid);
        panic!("DA1 reply did not round-trip through vttest's display. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — dumps a four-level submenu screen N.M.K.L"]
async fn dump_four_level_screen() {
    let Some(session) = VttestSession::open("dump four level screen") else {
        return;
    };
    let path: Vec<String> = std::env::var("VTTEST_PATH")
        .unwrap_or_else(|_| "11.1.2.4".into())
        .split('.')
        .map(str::to_string)
        .collect();
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    for step in &path {
        session.send(&format!("{step}\r")).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Path {} screen ===\n{dump}", path.join("."));
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — dumps a sub-sub-menu screen N.M.K"]
async fn dump_subsubmenu_screen() {
    let Some(session) = VttestSession::open("dump subsubmenu screen") else {
        return;
    };
    let outer = std::env::var("VTTEST_OUTER").unwrap_or_else(|_| "11".into());
    let inner = std::env::var("VTTEST_INNER").unwrap_or_else(|_| "5".into());
    let leaf = std::env::var("VTTEST_LEAF").unwrap_or_else(|_| "7".into());
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    session.send(&format!("{outer}\r")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    session.send(&format!("{inner}\r")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    session.send(&format!("{leaf}\r")).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Submenu {outer}.{inner}.{leaf} screen ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — dumps a sub-menu screen N.M"]
async fn dump_submenu_screen() {
    let Some(session) = VttestSession::open("dump submenu screen") else {
        return;
    };
    let outer = std::env::var("VTTEST_OUTER").unwrap_or_else(|_| "11".into());
    let inner = std::env::var("VTTEST_INNER").unwrap_or_else(|_| "1".into());
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    session.send(&format!("{outer}\r")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    session.send(&format!("{inner}\r")).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Submenu {outer}.{inner} screen ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — walks menu N forward by RETURNs"]
async fn dump_menu_walk() {
    let Some(session) = VttestSession::open("dump menu walk") else {
        return;
    };
    let menu = std::env::var("VTTEST_MENU").unwrap_or_else(|_| "2".into());
    let steps: u32 = std::env::var("VTTEST_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    session.send(&format!("{menu}\r")).await;
    for i in 0..steps {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let dump = session.with_grid(dump_grid);
        eprintln!("=== Menu {menu} screen step {i} ===\n{dump}");
        session.send("\r").await;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Menu {menu} screen final ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — prints the main menu to stderr"]
async fn dump_main_menu() {
    let Some(session) = VttestSession::open("dump main menu") else {
        return;
    };
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Main menu ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — prints a menu screen to stderr"]
async fn dump_menu_screen() {
    let Some(session) = VttestSession::open("dump menu screen") else {
        return;
    };
    let menu = std::env::var("VTTEST_MENU").unwrap_or_else(|_| "1".into());
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Choose test type", Duration::from_secs(3))
            .await
    );
    session.send(&format!("{menu}\r")).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Menu {menu} screen ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic probe — prints the reports submenu to stderr"]
async fn dump_terminal_reports_submenu() {
    let Some(session) = VttestSession::open("dump terminal reports submenu") else {
        return;
    };
    assert!(
        session
            .wait_for("VT100 test program", Duration::from_secs(3))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Test of terminal reports", Duration::from_secs(3))
            .await
    );
    session.send("6\r").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let dump = session.with_grid(dump_grid);
    eprintln!("=== Terminal Reports submenu ===\n{dump}");
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn da2_reply_round_trips_through_terminal_reports_menu() {
    let Some(session) = VttestSession::open("vttest DA2 round-trip") else {
        return;
    };
    session
        .navigate(&["Test of terminal reports", "Secondary Device Attributes"])
        .await;

    // The DA2 reply is `CSI > 41 ; 400 ; 0 c`. Match vttest's decoded
    // `Pp=41 (VT420)` rather than a bare model name: the preceding
    // submenu annotates entries with "VT420 and up", so a bare-name
    // match can pass without any reply at all.
    let hit = session
        .wait_for("Pp=41 (VT420)", Duration::from_secs(5))
        .await;
    if !hit {
        let dump = session.with_grid(dump_grid);
        panic!("DA2 reply did not round-trip. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn da3_reply_round_trips_through_terminal_reports_menu() {
    let Some(session) = VttestSession::open("vttest DA3 round-trip") else {
        return;
    };
    session
        .navigate(&["Test of terminal reports", "Tertiary Device Attributes"])
        .await;

    // The DA3 reply is `DCS ! | 66656c69732d31 ST`, the hex of
    // "felis-1". vttest renders the per-byte echo space-separated;
    // match the spaced hex so its escape-byte decoration does not
    // matter.
    let hit = session
        .wait_for("6 6 6 5 6 c 6 9 7 3 2 d 3 1", Duration::from_secs(5))
        .await;
    if !hit {
        let dump = session.with_grid(dump_grid);
        panic!("DA3 reply did not round-trip. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decreqtparm_reply_round_trips_through_terminal_reports_menu() {
    let Some(session) = VttestSession::open("vttest DECREQTPARM round-trip") else {
        return;
    };
    session
        .navigate(&["Test of terminal reports", "Request Terminal Parameters"])
        .await;

    // The DECREQTPARM reply is `\e[2;1;1;120;120;1;0x`; vttest decodes
    // 120 as the baud code for 19200.
    let hit = session.wait_for("19200", Duration::from_secs(5)).await;
    if !hit {
        let dump = session.with_grid(dump_grid);
        panic!("DECREQTPARM reply did not round-trip. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_movements_draws_unbroken_frame() {
    let Some(session) = VttestSession::open("cursor-movements") else {
        return;
    };
    session.navigate(&["Test of cursor movements"]).await;

    // The first sub-test draws an unbroken `*` frame around the outer
    // border. Gate on the prompt so the full screen has drawn, then
    // check the corners.
    assert!(
        session
            .wait_for("unbroken bor-", Duration::from_secs(5))
            .await,
        "cursor-movements frame text never reached the grid",
    );
    let frame_ok = session.with_grid(|g| {
        let last_row = g.rows() - 1;
        let last_col = g.cols() - 1;
        let corner = |r, c| match &g.cell(r, c).unwrap().grapheme {
            Grapheme::Ascii(b) => *b == b'*',
            _ => false,
        };
        corner(0, 0) && corner(0, last_col) && corner(last_row, 0) && corner(last_row, last_col)
    });
    if !frame_ok {
        let dump = session.with_grid(dump_grid);
        panic!("cursor-movements outer frame is broken at the corners. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decrqss_decscusr_round_trips_blink_state() {
    let Some(session) = VttestSession::open("DECRQSS DECSCUSR test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test XTERM special features",
            "Test reporting functions",
            "Status-String Report",
            "Test VT520 features",
            "Test VT510 features",
            "Set Cursor Style",
        ])
        .await;

    // vttest renders "ok (valid request)" only when the DECRQSS reply
    // carries Ps=1 and the same body it sent.
    assert!(
        session
            .wait_for("ok (valid request)", Duration::from_secs(8))
            .await,
        "DECRQSS DECSCUSR round-trip did not report 'valid request'",
    );

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xtversion_round_trips_felis_name_and_version() {
    let Some(session) = VttestSession::open("XTVERSION test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test XTERM special features",
            "Test reporting functions",
            "Report version (XTVERSION)",
        ])
        .await;

    // vttest renders the reply payload with one space between bytes
    // ("f e l i s <32> 0 . 1 . 0"); match the name so version drift
    // does not matter.
    assert!(
        session.wait_for("f e l i s", Duration::from_secs(5)).await,
        "XTVERSION reply did not round-trip",
    );

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn window_state_report_returns_normal_in_menu_11_8_9() {
    let Some(session) = VttestSession::open("window-state test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test XTERM special features",
            "Window report-operations",
        ])
        .await;

    // For `CSI 11 t` felis replies `CSI 1 t` and vttest renders
    // "OK: normal" below the raw response.
    assert!(
        session.wait_for("OK: normal", Duration::from_secs(8)).await,
        "window state (11) report not OK",
    );

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decsca_leaves_protected_asterisk_box_after_decsed() {
    let Some(session) = VttestSession::open("DECSCA test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test of VT220 features",
            "Test screen-display functions",
            "Test Protected-Areas (DECSCA)",
        ])
        .await;

    // After DECSED only the DECSCA-protected ~12x40 centered box of `*`
    // survives; the instruction text draws after the pattern.
    assert!(
        session
            .wait_for("solid box made of *", Duration::from_secs(5))
            .await,
        "DECSCA test screen never drew",
    );
    let box_present = session.with_grid(|g| {
        // Center of the box, and a cell expected to be cleared.
        let center = g.cell(10, 40).unwrap();
        let outside = g.cell(1, 0).unwrap();
        let is_star = |c: &felis_grid::Cell| match &c.grapheme {
            Grapheme::Ascii(b) => *b == b'*',
            _ => false,
        };
        let is_blank = |c: &felis_grid::Cell| matches!(c.grapheme, Grapheme::Empty);
        is_star(center) && is_blank(outside)
    });
    if !box_present {
        let dump = session.with_grid(dump_grid);
        panic!("DECSCA box not formed correctly. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn su_lifts_asterisks_to_top_row() {
    let Some(session) = VttestSession::open("SU test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test other ISO-6429 features",
            "Test Scroll-Up",
        ])
        .await;

    // vttest SUs a row of `*` up to row 0; it spans at least 20 cells.
    assert!(
        session
            .wait_for("horizontal row of *", Duration::from_secs(5))
            .await,
        "SU test screen never drew",
    );
    session.check_or_dump(|g| {
        for c in 0u16..20 {
            if !matches!(g.cell(0, c).unwrap().grapheme, Grapheme::Ascii(b'*')) {
                return Err("SU did not lift asterisks to the top row".into());
            }
        }
        Ok(())
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sr_centers_asterisk_column_on_the_screen() {
    let Some(session) = VttestSession::open("SR test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test other ISO-6429 features",
            "Test Scroll-Right",
        ])
        .await;

    // vttest SRs a column of `*` in rows 0..=19 to column 39.
    assert!(
        session
            .wait_for("vertical column of *'s centered", Duration::from_secs(5))
            .await,
        "SR test screen never drew",
    );
    let centered = session.with_grid(|g| {
        for r in 0u16..20 {
            let cell = g.cell(r, 39).unwrap();
            let ch = match &cell.grapheme {
                Grapheme::Ascii(b) => *b as char,
                Grapheme::Char(c) => *c,
                _ => return false,
            };
            if ch != '*' {
                return false;
            }
        }
        true
    });
    if !centered {
        let dump = session.with_grid(dump_grid);
        panic!("SR did not center the asterisk column. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rep_draws_diagonal_of_plus_characters() {
    let Some(session) = VttestSession::open("REP test") else {
        return;
    };
    session
        .navigate(&["non-VT100", "Test other ISO-6429 features", "Test Repeat"])
        .await;

    // Row r holds 12 `+` starting at col r+1, so col 12 is `+` on rows
    // 0..=11 only if REP repeats.
    assert!(
        session
            .wait_for("diagonal of 2 +", Duration::from_secs(5))
            .await,
        "REP test screen never drew",
    );
    let diagonal_ok = session.with_grid(|g| {
        for r in 0u16..12 {
            let cell = g.cell(r, 12).unwrap();
            let ch = match &cell.grapheme {
                Grapheme::Ascii(b) => *b as char,
                Grapheme::Char(c) => *c,
                _ => return false,
            };
            if ch != '+' {
                return false;
            }
        }
        true
    });
    if !diagonal_ok {
        let dump = session.with_grid(dump_grid);
        panic!("REP diagonal is missing or incomplete. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cnl_numbers_lines_in_sequence_at_column_zero() {
    let Some(session) = VttestSession::open("CNL test") else {
        return;
    };
    session
        .navigate(&["non-VT100", "ISO-6429 cursor-movement", "Test Next-Line"])
        .await;

    // Row N begins with N+1 at column 0 (a CNL that fell through to
    // CUD would stair-step). Anchor on the explanation text: the menu
    // label "Test Next-Line (CNL)" is too close to the menu line.
    assert!(
        session
            .wait_for("should be numbered in sequence", Duration::from_secs(5))
            .await,
        "CNL test screen never drew",
    );
    let stacked_correctly = session.with_grid(|g| {
        for r in 0u16..19 {
            let expected = format!("{}", r + 1);
            let mut row_prefix = String::new();
            for c in 0..expected.len() as u16 {
                let cell = g.cell(r, c).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    Grapheme::Char(c) => *c,
                    _ => return false,
                };
                row_prefix.push(ch);
            }
            if row_prefix != expected {
                return false;
            }
        }
        true
    });
    if !stacked_correctly {
        let dump = session.with_grid(dump_grid);
        panic!("CNL did not stack numbers at column 0. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hpa_draws_box_outline_in_the_middle() {
    let Some(session) = VttestSession::open("HPA test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "ISO-6429 cursor-movement",
            "Test Character-Position-Absolute",
        ])
        .await;

    // vttest paints a `*` box outline via HPA: a top edge of ~40 cells
    // and side columns through rows 1..11.
    assert!(
        session
            .wait_for("box-outline made of *", Duration::from_secs(5))
            .await,
        "HPA test screen never drew",
    );
    // The prompt text arrives after the box and some releases
    // interleave the two; let the read task drain.
    tokio::time::sleep(Duration::from_millis(800)).await;
    session.check_or_dump(|g| {
        let is_star = |r: u16, c: u16| -> bool {
            matches!(g.cell(r, c).unwrap().grapheme, Grapheme::Ascii(b'*'))
        };
        // vttest centers the box vertically; find the top row.
        let Some(top) = (0..g.rows()).find(|r| (0..g.cols()).any(|c| is_star(*r, c))) else {
            return Err("HPA box outline broken: no '*' anywhere".into());
        };
        let start = (0..g.cols()).find(|c| is_star(top, *c)).unwrap();
        for off in 0..30u16 {
            if !is_star(top, start + off) {
                return Err(format!(
                    "HPA box outline broken: row {top} col {} is not '*'",
                    start + off
                ));
            }
        }
        let mut end = start + 29;
        while end + 1 < g.cols() && is_star(top, end + 1) {
            end += 1;
        }
        for r in (top + 1)..(top + 11) {
            if !is_star(r, start) {
                return Err(format!(
                    "HPA box outline broken: row {r} col {start} (left side) blank"
                ));
            }
            if !is_star(r, end) {
                return Err(format!(
                    "HPA box outline broken: row {r} col {end} (right side) blank"
                ));
            }
        }
        Ok(())
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ech_paints_clear_gap_diagonal() {
    let Some(session) = VttestSession::open("ECH test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test of VT220 features",
            "Test screen-display functions",
            "Test Erase Char",
        ])
        .await;

    // Row R: E's at cols 0..=(73-R), an ECH gap at 74-R, `**` at
    // 75-R..=76-R, then dots.
    assert!(
        session.wait_for("ECH test", Duration::from_secs(5)).await,
        "ECH screen never drew",
    );
    let ok = session.with_grid(|g| {
        // Scan for the stars: releases drift on the exact column, and
        // the pattern "many E's, one gap, two stars" is what matters.
        let row = 5u16;
        let mut e_count = 0u16;
        let mut col = 0u16;
        while col < g.cols() {
            if matches!(g.cell(row, col).unwrap().grapheme, Grapheme::Ascii(b'E')) {
                e_count += 1;
                col += 1;
            } else {
                break;
            }
        }
        if e_count == 0 {
            return false;
        }
        let gap = g.cell(row, col).unwrap();
        let star_l = g.cell(row, col + 1).unwrap();
        let star_r = g.cell(row, col + 2).unwrap();
        let blank = matches!(gap.grapheme, Grapheme::Empty | Grapheme::Spacer)
            || matches!(gap.grapheme, Grapheme::Ascii(b' '));
        let star = matches!(star_l.grapheme, Grapheme::Ascii(b'*'))
            && matches!(star_r.grapheme, Grapheme::Ascii(b'*'));
        blank && star
    });
    if !ok {
        let dump = session.with_grid(dump_grid);
        panic!("ECH diagonal not formed. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dectcem_hides_cursor_in_menu_11_1_2_2() {
    let Some(session) = VttestSession::open("DECTCEM test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test of VT220 features",
            "Test screen-display functions",
            "Test Visible/Invisible Cursor",
        ])
        .await;

    // vttest hides the cursor via `CSI ?25 l`; check the grid state.
    assert!(
        session
            .wait_for("cursor should be invisible", Duration::from_secs(5))
            .await,
        "DECTCEM screen never drew",
    );
    let visible = session.with_grid(|g| g.cursor().visible);
    assert!(!visible, "DECTCEM ?25l did not hide the cursor");

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s8c1t_round_trips_through_8bit_control_test() {
    let Some(session) = VttestSession::open("S8C1T test") else {
        return;
    };
    session
        .navigate(&["non-VT100", "Test of VT220 features", "Test 8-bit controls"])
        .await;

    // vttest sends a DSR in 8-bit mode and again in 7-bit mode; the
    // 8-bit reply renders as "<155> …". Anchor on the introductory
    // line ("emit 8-bit") so the matcher does not fire on the menu
    // item that contains "8-bit controls".
    assert!(session.wait_for("emit 8-bit", Duration::from_secs(5)).await);
    // The two `ok` lines may arrive in either order.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut enabled_ok = false;
    let mut disabled_ok = false;
    while tokio::time::Instant::now() < deadline {
        let dump = session.with_grid(dump_grid);
        enabled_ok = dump.contains("8-bit controls enabled:") && dump.contains("<155>");
        disabled_ok = dump.contains("8-bit controls disabled:") && dump.contains("<27> [");
        if enabled_ok && disabled_ok {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !(enabled_ok && disabled_ok) {
        let dump = session.with_grid(dump_grid);
        panic!("8-bit-control round-trip did not produce both ok lines. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_dsr_reports_no_printer_locked_udk_us_keyboard() {
    let Some(session) = VttestSession::open("private DSR test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test of VT220 features",
            "Test reporting functions",
            "Test Device Status Report",
        ])
        .await;
    assert!(
        session
            .wait_for("Test Keyboard Status", Duration::from_secs(3))
            .await
    );
    let idx = session
        .with_grid(|g| find_menu_index(g, "Test Keyboard Status"))
        .expect("keyboard status leaf");
    session.send(&format!("{idx}\r")).await;
    // "North American/ASCII" is the most distinctive of vttest's
    // interpretation strings.
    let hit = session
        .wait_for("North American", Duration::from_secs(5))
        .await;
    if !hit {
        let dump = session.with_grid(dump_grid);
        panic!("keyboard DSR ?26 n did not produce North American reply. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrap_around_cursor_addressing_pins_stars_to_last_column() {
    let Some(session) = VttestSession::open("wrap-around bug test") else {
        return;
    };
    session
        .navigate(&["Test of known bugs", "Wrap around with cursor"])
        .await;

    // vttest's "Bug 7": CUP-then-print at the rightmost column. On a
    // buggy terminal the `*`s land in column 0 instead of column 79.
    assert!(
        session
            .wait_for("wrap around bug", Duration::from_secs(5))
            .await
    );
    let stars_ok = session.with_grid(|g| {
        for r in 1u16..21 {
            let last = g.cell(r, 79).unwrap();
            let first = g.cell(r, 0).unwrap();
            let is_star = matches!(&last.grapheme, Grapheme::Ascii(b'*'));
            let first_blank = matches!(&first.grapheme, Grapheme::Empty | Grapheme::Spacer)
                || matches!(&first.grapheme, Grapheme::Ascii(b' '));
            if !is_star || !first_blank {
                return false;
            }
        }
        true
    });
    if !stars_ok {
        let dump = session.with_grid(dump_grid);
        panic!("VT100 wrap-around bug — stars leaked off column 79. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decstr_soft_reset_completes_in_menu_10() {
    let Some(session) = VttestSession::open("DECSTR") else {
        return;
    };
    session
        .navigate(&["Test of reset", "Soft Terminal Reset"])
        .await;

    // "Push <RETURN>" reappearing is the simplest cue that the reset
    // ran.
    let hit = session
        .wait_for("Push <RETURN>", Duration::from_secs(5))
        .await;
    if !hit {
        let dump = session.with_grid(dump_grid);
        panic!("DECSTR screen never settled. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bce_clear_paints_screen_background_blue() {
    let Some(session) = VttestSession::open("BCE test") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test ISO-6429 colors",
            "Test BCE-style clear line/display (ED, EL)",
        ])
        .await;

    // vttest sets bg=blue, ED-clears, then draws a box. With BCE the
    // blank cells outside the box carry bg=blue; row 0 col 0 is one.
    assert!(
        session
            .wait_for("background should be blue", Duration::from_secs(5))
            .await,
        "BCE test screen never drew",
    );
    session.check_or_dump(|g| {
        let cell = g.cell(0, 0).unwrap();
        if matches!(g.style(cell.style).bg, felis_grid::Color::Default) {
            Err("BCE did not paint screen bg on cleared cells".into())
        } else {
            Ok(())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iso_6429_colors_paints_named_palette() {
    let Some(session) = VttestSession::open("ISO-6429 colors") else {
        return;
    };
    session
        .navigate(&[
            "non-VT100",
            "Test ISO-6429 colors",
            "Display color test-pattern",
        ])
        .await;

    // The pattern screen prints a "bright off" matrix then a "bright
    // on" one; the second one's arrival proves both completed without
    // the parser dropping any SGR.
    if !session
        .wait_for("bright *on*", Duration::from_secs(5))
        .await
    {
        let dump = session.with_grid(dump_grid);
        panic!("color test-pattern never drew the 'bright on' matrix. Grid:\n{dump}");
    }
    let labeled = session.with_grid(|g| {
        grid_contains(g, "red")
            && grid_contains(g, "green")
            && grid_contains(g, "blue")
            && grid_contains(g, "magenta")
    });
    if !labeled {
        let dump = session.with_grid(dump_grid);
        panic!("ISO-6429 colors labels missing. Grid:\n{dump}");
    }

    // Text presence proves vttest spoke; a colored cell proves felis
    // honored the SGRs in between.
    let has_colored_cell = session.with_grid(|g| {
        for r in 0..g.rows() {
            for c in 0..g.cols() {
                let cell = g.cell(r, c).unwrap();
                let attrs = g.style(cell.style);
                if !matches!(attrs.fg, felis_grid::Color::Default)
                    || !matches!(attrs.bg, felis_grid::Color::Default)
                {
                    return true;
                }
            }
        }
        false
    });
    if !has_colored_cell {
        let dump = session.with_grid(dump_grid);
        panic!("colors test left no colored cells on the grid. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_features_autowrap_fills_three_rows_with_stars() {
    let Some(session) = VttestSession::open("autowrap test") else {
        return;
    };
    session.navigate(&["Test of screen features"]).await;

    // ~240 `*` into a fresh screen with default autowrap leaves three
    // identical 80-wide rows.
    assert!(
        session
            .wait_for("WRAP AROUND mode setting", Duration::from_secs(5))
            .await,
        "autowrap test screen never drew",
    );
    let three_full_rows = session.with_grid(|g| {
        for r in [0u16, 1, 2] {
            for c in 0..g.cols() {
                let cell = g.cell(r, c).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    _ => return false,
                };
                if ch != '*' {
                    return false;
                }
            }
        }
        true
    });
    if !three_full_rows {
        let dump = session.with_grid(dump_grid);
        panic!("autowrap did not fill three rows of '*'. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_features_tab_setting_lands_stars_on_custom_stops() {
    let Some(session) = VttestSession::open("tab-setting test") else {
        return;
    };
    session.navigate(&["Test of screen features"]).await;

    // Step past the autowrap screen to the tab-setting screen: TBC 3,
    // HTS every 6 columns, HT along row 0; row 1 uses literal spaces.
    // Both rows land `*` at columns 6, 12, …, 78.
    assert!(
        session
            .wait_for("WRAP AROUND mode", Duration::from_secs(5))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Test of TAB setting", Duration::from_secs(5))
            .await,
        "tab-setting screen never drew",
    );
    let stops_match = session.with_grid(|g| {
        let cols: Vec<u16> = (6..g.cols()).step_by(6).collect();
        for col in &cols {
            for row in [0u16, 1] {
                let cell = g.cell(row, *col).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    Grapheme::Char(c) => *c,
                    _ => return false,
                };
                if ch != '*' {
                    return false;
                }
            }
        }
        !cols.is_empty()
    });
    if !stops_match {
        let dump = session.with_grid(dump_grid);
        panic!("HT did not land `*` on the custom tab stops. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vt102_accordion_test_draws_per_row_letter_fill() {
    let Some(session) = VttestSession::open("VT102 accordion test") else {
        return;
    };
    session.navigate(&["Test of VT102 features"]).await;

    // The accordion seed fills row r with its own letter (A, B, …),
    // except row 3, which carries an inlined instruction.
    assert!(
        session
            .wait_for("Screen accordion test", Duration::from_secs(5))
            .await,
        "VT102 features test screen never drew",
    );
    let ok = session.with_grid(|g| {
        let letter_of = |r: u16| (b'A' + (r as u8)) as char;
        for r in [0u16, 1, 2] {
            for c in 0..g.cols() {
                let cell = g.cell(r, c).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    Grapheme::Char(c) => *c,
                    _ => continue,
                };
                if ch != letter_of(r) {
                    return false;
                }
            }
        }
        for r in 4u16..g.rows() {
            // Some vttest versions bleed the instruction text into the
            // first columns of row 4; sample the right half only.
            let half = g.cols() / 2;
            for c in half..g.cols() {
                let cell = g.cell(r, c).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    Grapheme::Char(c) => *c,
                    _ => continue,
                };
                if ch != letter_of(r) {
                    return false;
                }
            }
        }
        true
    });
    if !ok {
        let dump = session.with_grid(dump_grid);
        panic!("VT102 accordion seed letters are wrong. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dsr_cursor_position_round_trips() {
    let Some(session) = VttestSession::open("vttest DSR round-trip") else {
        return;
    };
    session
        .navigate(&["Test of terminal reports", "Device Status Report"])
        .await;

    // vttest CUPs to known positions and validates `CSI 6 n` replies,
    // printing "OK" per iteration and stopping at the first "Failed".
    let hit = session.wait_for("OK", Duration::from_secs(8)).await;
    if !hit {
        let dump = session.with_grid(dump_grid);
        panic!("DSR cursor position round-trip did not produce 'OK'. Grid:\n{dump}");
    }

    session.shutdown().await;
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

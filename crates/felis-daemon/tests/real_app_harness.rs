//! Real-binary integration harness: the daemon over an in-memory
//! duplex carrier, a real TUI as the session's child, and the
//! `GridMsg` stream mirrored into a `ShadowScreen` with per-variant
//! frame counters. Each test skips when its binary is not on `PATH`.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![expect(
    clippy::manual_let_else,
    clippy::print_stderr,
    reason = "integration harness: eprintln! is intentional test diagnostics; explicit match reads clearly"
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use felis_client_core::ShadowScreen;

mod common;
#[cfg(unix)]
use common::shadow_contains;
use common::{resolve_binary, shadow_rows};
use felis_daemon::pool::SessionPool;
use felis_daemon::serve::{DaemonCaps, SessionFactory, handle_stream};
use felis_grid::{Cell, Grapheme};
use felis_protocol::{
    ConnectionMode, MessageKind, codec,
    messages::{
        ConnToDaemonMsg, GridMsg, InputMsg, RequestedDims, SessionToClientMsg, SessionToDaemonMsg,
    },
    preface::ClientPreface,
};
use felis_pty::Command;
use felis_transport::framing::{FrameReader, FrameWriter};
use tokio::io::DuplexStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

const ROWS: u16 = 24;
const COLS: u16 = 80;

pub struct RealAppSession {
    server: Option<JoinHandle<Result<(), felis_daemon::serve::ConnError>>>,
    drain_task: Option<JoinHandle<()>>,
    writer: Arc<AsyncMutex<FrameWriter<DuplexStream>>>,
    shadow: Arc<Mutex<ShadowScreen>>,
    counters: Arc<Mutex<FrameCounters>>,
}

#[derive(Debug, Default, Clone)]
pub struct FrameCounters {
    pub by_variant: HashMap<&'static str, usize>,
    /// A `RowDelta` counts as `rows.len()`.
    pub total_dirty_rows: usize,
}

impl FrameCounters {
    pub fn count(&self, variant: &'static str) -> usize {
        self.by_variant.get(variant).copied().unwrap_or(0)
    }
}

impl RealAppSession {
    pub async fn spawn(command: Command, pull_paced: bool) -> Self {
        let pool = Arc::new(AsyncMutex::new(SessionPool::new()));
        // The daemon stamps `xterm-felis` on every session and
        // `SpawnArgs.env` is the override channel
        // (docs/reference/terminal-identity.md); under a cleared env
        // that terminfo entry does not resolve, so the command's TERM
        // must ride the override.
        let env: Vec<(String, String)> = command
            .get_envs()
            .filter(|(k, _)| *k == std::ffi::OsStr::new("TERM"))
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        let factory: SessionFactory = {
            let cmd = Mutex::new(Some(command));
            Arc::new(move |_| {
                cmd.lock()
                    .unwrap()
                    .take()
                    .expect("RealAppSession factory called more than once")
            })
        };

        let (client_to_daemon_w, daemon_read) = tokio::io::duplex(256 * 1024);
        let (daemon_write, client_from_daemon_r) = tokio::io::duplex(256 * 1024);

        let server_pool = Arc::clone(&pool);
        let server = tokio::spawn(async move {
            handle_stream(
                daemon_read,
                daemon_write,
                DaemonCaps::default(),
                server_pool,
                factory,
            )
            .await
        });

        // Preface before framing: a `FrameReader` built first would
        // read past the daemon's preface reply.
        let (mut client_to_daemon_w, mut client_from_daemon_r) =
            (client_to_daemon_w, client_from_daemon_r);
        felis_transport::preface::write_client_preface(
            &mut client_to_daemon_w,
            ClientPreface::CURRENT,
        )
        .await
        .unwrap();
        let accepted = felis_transport::preface::read_daemon_preface(&mut client_from_daemon_r)
            .await
            .unwrap();
        assert!(
            matches!(
                accepted,
                felis_protocol::preface::DaemonPreface::Accept { .. }
            ),
            "daemon must accept this build's protocol major, got {accepted:?}"
        );

        let mut writer = FrameWriter::at_build_minor(client_to_daemon_w);
        let mut reader = FrameReader::new(client_from_daemon_r);

        writer
            .send(&ConnToDaemonMsg::Hello {
                mode: ConnectionMode::Window,
                pull_paced,
            })
            .await
            .unwrap();
        let _welcome = reader.next_frame().await.unwrap().expect("welcome");

        writer
            .send(&SessionToDaemonMsg::Create {
                args: felis_protocol::messages::SpawnArgs {
                    env,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        let created = reader.next_frame().await.unwrap().expect("created");
        // The create ack means attached; no follow-up `Attach`.
        match codec::decode::<SessionToClientMsg>(&created.body).unwrap() {
            SessionToClientMsg::Created { .. } => {}
            other => panic!("expected SessionToClientMsg::Created, got {other:?}"),
        }

        // Mirrors felis-client, which resizes before any input.
        writer
            .send(&InputMsg::Resize {
                dims: RequestedDims {
                    rows: u32::from(ROWS),
                    cols: u32::from(COLS),
                    pixel_w: u32::from(COLS) * 8,
                    pixel_h: u32::from(ROWS) * 19,
                },
            })
            .await
            .unwrap();

        let shadow = Arc::new(Mutex::new(ShadowScreen::new(ROWS, COLS)));
        let counters = Arc::new(Mutex::new(FrameCounters::default()));

        let shadow_drain = Arc::clone(&shadow);
        let counters_drain = Arc::clone(&counters);
        let drain_task = tokio::spawn(async move {
            loop {
                let frame = match reader.next_frame().await {
                    Ok(Some(frame)) => frame,
                    _ => break,
                };
                if frame.kind != MessageKind::Grid.as_u16() {
                    continue;
                }
                let Ok(msg) = codec::decode::<GridMsg>(&frame.body) else {
                    continue;
                };
                {
                    let mut c = counters_drain.lock().unwrap();
                    let (name, dirty_inc) = match &msg {
                        GridMsg::RehydrateBegin => ("RehydrateBegin", 0),
                        GridMsg::RehydrateEnd => ("RehydrateEnd", 0),
                        GridMsg::RowDelta { rows } => ("RowDelta", rows.len()),
                        GridMsg::CursorState { .. } => ("CursorState", 0),
                        GridMsg::Title { .. } => ("Title", 0),
                        GridMsg::Cwd { .. } => ("Cwd", 0),
                        GridMsg::PromptMark { .. } => ("PromptMark", 0),
                        GridMsg::ThemeColor { .. } => ("ThemeColor", 0),
                        GridMsg::PaletteColor { .. } => ("PaletteColor", 0),
                        GridMsg::PaletteResetAll => ("PaletteResetAll", 0),
                        GridMsg::Hyperlink { .. } => ("Hyperlink", 0),
                        GridMsg::Cluster { .. } => ("Cluster", 0),
                        GridMsg::KittyKbdFlags { .. } => ("KittyKbdFlags", 0),
                        GridMsg::ClipboardSet { .. } => ("ClipboardSet", 0),
                        GridMsg::Attention { .. } => ("Attention", 0),
                        GridMsg::PointerShape { .. } => ("PointerShape", 0),
                        GridMsg::ModeFlags { .. } => ("ModeFlags", 0),
                        GridMsg::ViewportState { .. } => ("ViewportState", 0),
                        GridMsg::Scrolled { .. } => ("Scrolled", 0),
                        GridMsg::Size { .. } => ("Size", 0),
                        GridMsg::CycleEnd => ("CycleEnd", 0),
                    };
                    *c.by_variant.entry(name).or_insert(0) += 1;
                    c.total_dirty_rows += dirty_inc;
                }
                if let Ok(mut shadow) = shadow_drain.lock() {
                    drop(shadow.apply(&msg));
                }
            }
        });

        Self {
            server: Some(server),
            drain_task: Some(drain_task),
            writer: Arc::new(AsyncMutex::new(writer)),
            shadow,
            counters,
        }
    }

    pub async fn send_input(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().await;
        writer
            .send(&InputMsg::KeyBytes(bytes.to_vec()))
            .await
            .unwrap();
    }

    /// Under pull pacing the daemon ships the accumulated grid diff
    /// only in response to this (docs/explanation/rendering/pipeline.md
    /// "Demand-driven emission").
    pub async fn send_pull(&self) {
        let mut writer = self.writer.lock().await;
        writer.send(&InputMsg::NextGridFrame).await.unwrap();
    }

    pub fn server_finished(&self) -> bool {
        self.server.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub async fn server_outcome(&mut self) -> Option<Result<(), felis_daemon::serve::ConnError>> {
        match self.server.take() {
            Some(h) => Some(h.await.unwrap_or(Ok(()))),
            None => None,
        }
    }

    pub fn with_shadow<R>(&self, f: impl FnOnce(&ShadowScreen) -> R) -> R {
        let shadow = self.shadow.lock().expect("shadow lock");
        f(&shadow)
    }

    pub fn counters(&self) -> FrameCounters {
        self.counters.lock().expect("counters lock").clone()
    }

    pub async fn wait_for_shadow<F>(&self, mut predicate: F, timeout: Duration) -> bool
    where
        F: FnMut(&ShadowScreen) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if self.with_shadow(|s| predicate(s)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    pub async fn shutdown(mut self) {
        {
            let mut writer = self.writer.lock().await;
            drop(writer.flush().await);
        }
        if let Some(drain) = self.drain_task.take() {
            drop(tokio::time::timeout(Duration::from_millis(200), drain).await);
        }
        if let Some(server) = self.server.take() {
            drop(tokio::time::timeout(Duration::from_millis(200), server).await);
        }
    }
}

impl Drop for RealAppSession {
    fn drop(&mut self) {
        if let Some(handle) = self.drain_task.take() {
            handle.abort();
        }
        if let Some(handle) = self.server.take() {
            handle.abort();
        }
    }
}

fn skip_if_missing(binary: &str) -> Option<PathBuf> {
    if let Some(p) = resolve_binary(binary) {
        Some(p)
    } else {
        eprintln!("skipping: {binary} not found on PATH");
        None
    }
}

/// The real `less` paging through the pipeline without a panic or
/// stall (a `Scrolled` frame or a quiet burst are both
/// protocol-correct).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn less_pages_through_long_stdin() {
    let Some(less) = skip_if_missing("less") else {
        return;
    };

    // `-X` keeps less off the alt screen so it stays interactive.
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!("yes line | head -200 | {} -X -R", less.display()),
    ]);
    cmd.env_clear();
    // NixOS keeps coreutils off /usr/bin:/bin; without `yes`/`head`
    // the shadow assertion matches `sh`'s "command not found" lines.
    let coreutils_dir = resolve_binary("head")
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .expect("`head` must be on the test runner's PATH");
    cmd.env("PATH", format!("{}:/usr/bin:/bin", coreutils_dir.display()));
    cmd.env("TERM", "xterm-256color");
    cmd.env("LESSSECURE", "1");

    let session = RealAppSession::spawn(cmd, false).await;

    // Count rows rather than match once: `sh`'s "line 1: ... not
    // found" diagnostic also matches once.
    let drew = session
        .wait_for_shadow(
            |s| shadow_rows(s).iter().filter(|r| r.contains("line")).count() >= 10,
            Duration::from_secs(2),
        )
        .await;
    assert!(
        drew,
        "less did not page 'line' content into the shadow: {:?}",
        session.with_shadow(shadow_rows),
    );

    for _ in 0..3 {
        session.send_input(b" ").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = session.counters();

    // Exact counts vary by terminfo and less version.
    assert!(
        after.count("CursorState") >= 1,
        "expected ≥ 1 CursorState frame, got 0; counters: {after:?}",
    );
    assert!(
        after.count("RowDelta") >= 1,
        "expected ≥ 1 cell-bearing frame, got 0; counters: {after:?}",
    );

    eprintln!("less counters: {after:?}");

    session.send_input(b"q").await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    session.shutdown().await;
}

/// Flush coalescing (docs/reference/ipc.md): a many-KiB burst
/// collapses to few `compose_diffs` cycles. A ceiling rather than an
/// exact count, since PTY scheduling varies.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_burst_collapses_to_few_send_diffs_cycles() {
    // Pure-`sh` loop + blocking `read`, not `seq`/`sleep`: NixOS keeps
    // coreutils off /usr/bin, and a missing `seq` runs the loop zero
    // times, passing vacuously on rehydration frames alone.
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        "i=1; while [ $i -le 5000 ]; do echo line-$i; i=$((i+1)); done; read _keepalive",
    ]);
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("TERM", "xterm-256color");

    let session = RealAppSession::spawn(cmd, false).await;

    tokio::time::sleep(Duration::from_secs(1)).await;
    let counters = session.counters();
    let cycle_count = counters.count("RowDelta");
    eprintln!("large-burst counters: {counters:?} (cycle_count={cycle_count})");

    // Uncoalesced: ~5000/24 ≈ 208 cycles; coalesced: ~5–25 depending
    // on PTY scheduling.
    assert!(
        cycle_count > 0,
        "expected ≥ 1 cell-bearing frame from the burst; got {counters:?}",
    );
    assert!(
        cycle_count < 100,
        "flush coalescing (docs/reference/ipc.md) should collapse a 5000-line burst \
         well below 100 cycles; got {cycle_count}; counters: {counters:?}",
    );

    session.shutdown().await;
}

/// Malformed escape sequences arriving over the real daemon path (not
/// a direct `Parser::advance`) neither panic the session nor stop the
/// parser recovering to ground: a marker printed after the barrage
/// still renders.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pathological_escape_barrage_does_not_kill_the_session() {
    // Per iteration: a long ST-terminated OSC 52, an unterminated OSC 9
    // aborted by CAN, a junk DCS, a CSI past MAX_PARAMS, and a CSI with
    // embedded C0 bytes; each returns the parser to ground.
    let chunk = "\
\\033]52;c;AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\\033\\\\\
\\033]9;BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\\030\
\\033P1;2;3xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\033\\\\\
\\033[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20;21;22;23;24m\
\\033[1;\\001\\002\\0033m";
    // Pure-`sh` loop + blocking `read`, not `seq`/`sleep`: NixOS keeps
    // coreutils off /usr/bin, and a missing `seq` runs the barrage zero
    // times.
    let script = format!(
        "i=1; while [ $i -le 300 ]; do printf '{chunk}'; i=$((i+1)); done; \
         printf '\\033[2J\\033[H'; printf 'SURVIVED-MARKER\\r\\n'; read _keepalive"
    );

    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", &script]);
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("TERM", "xterm-256color");

    let session = RealAppSession::spawn(cmd, false).await;

    let survived = session
        .wait_for_shadow(
            |s| shadow_contains(s, "SURVIVED-MARKER"),
            Duration::from_secs(5),
        )
        .await;
    assert!(
        survived,
        "post-barrage marker never reached the shadow; counters: {:?}",
        session.counters(),
    );
    assert!(
        !session.server_finished(),
        "daemon connection died during the escape barrage",
    );

    session.shutdown().await;
}

/// Throughput probe for a real `cat <file>` burst through the daemon
/// pipeline (`compose_diffs` + frame + decode + `ShadowScreen::apply`),
/// minus the GUI render loop and socket syscalls. Reports MiB/s and
/// the frame breakdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FELIS_CAT_FILE=/path/to/big.txt; run with --ignored"]
async fn profile_cat_throughput() {
    use std::time::Instant;

    let Some(file) = std::env::var_os("FELIS_CAT_FILE") else {
        eprintln!("skipping: set FELIS_CAT_FILE=/path/to/big.txt");
        return;
    };
    let bytes = std::fs::metadata(&file).map_or(0, |m| m.len());
    if bytes == 0 {
        eprintln!("skipping: FELIS_CAT_FILE is empty or unreadable");
        return;
    }

    let cat = match skip_if_missing("cat") {
        Some(p) => p,
        None => return,
    };
    // `FELIS_CAT_ITERS` repeats the burst so a `samply` run gets enough
    // samples; one cat is ~50 ms.
    let iters: u32 = std::env::var("FELIS_CAT_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut best_mibs = 0.0_f64;
    for iter in 0..iters {
        let mut cmd = Command::new(&cat);
        cmd.arg(&file);
        cmd.env_clear();
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.env("TERM", "xterm-256color");

        let start = Instant::now();
        let session = RealAppSession::spawn(cmd, false).await;

        // Completion is the last-change time; the trailing idle window
        // is overhead, not work.
        let mut last_total = 0usize;
        let mut last_change = start;
        let mut stable = 0u32;
        let deadline = start + Duration::from_secs(60);
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let c = session.counters();
            let total = c.count("RowDelta") + c.total_dirty_rows;
            if total == last_total {
                stable += 1;
                if stable >= 6 {
                    break;
                }
            } else {
                last_total = total;
                last_change = Instant::now();
                stable = 0;
            }
            if Instant::now() >= deadline {
                eprintln!("profile_cat_throughput: hit 60s deadline, reporting partial");
                break;
            }
        }

        let elapsed = last_change.duration_since(start);
        let mib = bytes as f64 / (1024.0 * 1024.0);
        let secs = elapsed.as_secs_f64();
        let mibs = mib / secs.max(1e-9);
        best_mibs = best_mibs.max(mibs);
        if iter == 0 || iter + 1 == iters {
            let counters = session.counters();
            eprintln!(
                "profile_cat_throughput[{iter}]: {mib:.1} MiB in {secs:.3} s = {mibs:.1} MiB/s | frames: {counters:?}",
            );
        }
        session.shutdown().await;
    }
    eprintln!("profile_cat_throughput: best {best_mibs:.1} MiB/s over {iters} iter(s)");
}

/// `profile_cat_throughput` under pull pacing at one pull per
/// simulated vsync, the real GUI client's cadence (`Offer::window`).
/// The eager run weights the row codec per PTY chunk; here the codec
/// runs only ~vsync-many times, so a codec win need not move the
/// real-client wall-clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs FELIS_CAT_FILE=/path/to/big.txt; run with --ignored"]
async fn profile_cat_pull_paced() {
    use std::time::Instant;

    let Some(file) = std::env::var_os("FELIS_CAT_FILE") else {
        eprintln!("skipping: set FELIS_CAT_FILE=/path/to/big.txt");
        return;
    };
    let bytes = std::fs::metadata(&file).map_or(0, |m| m.len());
    if bytes == 0 {
        eprintln!("skipping: FELIS_CAT_FILE is empty or unreadable");
        return;
    }
    let cat = match skip_if_missing("cat") {
        Some(p) => p,
        None => return,
    };
    let iters: u32 = std::env::var("FELIS_CAT_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    // /usr/bin:/bin holds neither `cat` nor `sleep` on NixOS.
    let coreutils_dir = cat
        .parent()
        .map(std::path::Path::to_path_buf)
        .expect("a resolved `cat` has a parent directory");
    let file_str = file.to_string_lossy().into_owned();
    let mut best_mibs = 0.0_f64;
    for iter in 0..iters {
        // The `sleep` keeps the PTY open until the pull loop reaches
        // quiescence; a bare `cat` exits the instant the daemon drains
        // it.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(format!("cat {file_str}; sleep 2"));
        cmd.env_clear();
        cmd.env("PATH", format!("{}:/usr/bin:/bin", coreutils_dir.display()));
        cmd.env("TERM", "xterm-256color");

        let start = Instant::now();
        let session = RealAppSession::spawn(cmd, true).await;

        // `FELIS_PULL_MS`: 16 is a 60 Hz vsync-gated client; 0 pulls
        // as fast as the daemon answers (a pull not gated on
        // `redraw.flush()`).
        let pull_ms: u64 = std::env::var("FELIS_PULL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(16);
        let mut last_total = 0usize;
        let mut last_change = start;
        let deadline = start + Duration::from_secs(120);
        loop {
            if session.server_finished() {
                break;
            }
            session.send_pull().await;
            if pull_ms > 0 {
                tokio::time::sleep(Duration::from_millis(pull_ms)).await;
            } else {
                tokio::task::yield_now().await;
            }
            let c = session.counters();
            let total = c.count("RowDelta") + c.total_dirty_rows;
            if total == last_total {
                // Time-based, so the sweep is comparable across
                // `FELIS_PULL_MS`.
                if last_change.elapsed() >= Duration::from_millis(300) {
                    break;
                }
            } else {
                last_total = total;
                last_change = Instant::now();
            }
            if Instant::now() >= deadline {
                eprintln!("profile_cat_pull_paced: hit 120s deadline");
                break;
            }
        }

        let secs = last_change.duration_since(start).as_secs_f64();
        let mib = bytes as f64 / (1024.0 * 1024.0);
        let mibs = mib / secs.max(1e-9);
        best_mibs = best_mibs.max(mibs);
        if iter == 0 || iter + 1 == iters {
            let counters = session.counters();
            eprintln!(
                "profile_cat_pull_paced[{iter}]: {mib:.1} MiB in {secs:.3} s = {mibs:.1} MiB/s | frames: {counters:?}",
            );
        }
        session.shutdown().await;
    }
    eprintln!("profile_cat_pull_paced: best {best_mibs:.1} MiB/s over {iters} iter(s)");
}

/// Echo latency (`KeyBytes` sent → glyph visible in the shadow):
/// every per-keystroke cost except the GUI present/vsync.
/// `FELIS_ECHO_PULL=1` runs pull-paced at 60 Hz, adding the 0–16 ms
/// demand-pull wait to the eager-push floor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "timing-sensitive latency probe; run with --ignored"]
async fn profile_echo_latency() {
    use std::time::Instant;

    const SAMPLES: usize = 200;

    if skip_if_missing("sh").is_none() {
        return;
    }
    // /usr/bin:/bin holds neither `stty` nor `cat` on NixOS.
    let Some(coreutils_dir) = skip_if_missing("cat")
        .as_deref()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
    else {
        return;
    };
    let pull = std::env::var("FELIS_ECHO_PULL").is_ok_and(|v| v != "0");
    // `-icanon -echo` makes `cat`'s read→write the single per-char
    // echo; the line discipline would echo first.
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        "stty -icanon -echo min 1 time 0 2>/dev/null; exec cat",
    ]);
    cmd.env_clear();
    cmd.env("PATH", format!("{}:/usr/bin:/bin", coreutils_dir.display()));
    cmd.env("TERM", "xterm-256color");

    let session = RealAppSession::spawn(cmd, pull).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let is_glyph = |cell: Option<&Cell>| {
        matches!(
            cell.map(|c| c.grapheme),
            Some(Grapheme::Ascii(_) | Grapheme::Char(_))
        )
    };
    let frame_count = |s: &RealAppSession| {
        let c = s.counters();
        c.count("RowDelta")
    };

    let cols = COLS;
    let mut lat: Vec<Duration> = Vec::with_capacity(SAMPLES);
    let mut timeouts = 0usize;
    // Vsync model (when `pull`): one pull outstanding, re-armed on the
    // next 16 ms tick after the daemon answers, like the real client.
    let mut next_tick = Instant::now() + Duration::from_millis(16);
    let mut pull_outstanding = false;
    let mut last_frames = frame_count(&session);
    for i in 0..SAMPLES {
        let idx = i as u16;
        let row = idx / cols;
        let col = idx % cols;
        if row >= ROWS {
            break; // stay on-screen; 200 chars fit in 24×80
        }
        // Vary the keystroke phase so pull-paced samples span the
        // 0–16 ms wait instead of always landing just after a frame.
        if pull {
            tokio::time::sleep(Duration::from_millis((i % 16) as u64)).await;
        }
        // A distinct byte per sample, so a stale cell cannot mask the
        // echo.
        let ch = b'!' + u8::try_from(i % 90).unwrap_or(0);
        let t0 = Instant::now();
        session.send_input(&[ch]).await;
        let deadline = t0 + Duration::from_secs(2);
        loop {
            if pull {
                let now = Instant::now();
                if frame_count(&session) != last_frames {
                    last_frames = frame_count(&session);
                    pull_outstanding = false;
                }
                if now >= next_tick {
                    if !pull_outstanding {
                        session.send_pull().await;
                        pull_outstanding = true;
                    }
                    // Re-based on now, so a slow poll does not bunch
                    // missed ticks into a burst of pulls.
                    next_tick = now + Duration::from_millis(16);
                }
            }
            if session.with_shadow(|s| is_glyph(s.screen().cell(row, col))) {
                lat.push(t0.elapsed());
                break;
            }
            if Instant::now() >= deadline {
                timeouts += 1;
                break;
            }
            tokio::time::sleep(Duration::from_micros(200)).await;
        }
    }

    lat.sort_unstable();
    let pct = |p: f64| -> f64 {
        if lat.is_empty() {
            return f64::NAN;
        }
        let idx = ((lat.len() as f64 - 1.0) * p).round() as usize;
        lat[idx].as_secs_f64() * 1e3
    };
    let mean = if lat.is_empty() {
        f64::NAN
    } else {
        lat.iter().map(Duration::as_secs_f64).sum::<f64>() / lat.len() as f64 * 1e3
    };
    eprintln!(
        "profile_echo_latency ({}): n={} timeouts={} | min={:.3} p50={:.3} p99={:.3} max={:.3} mean={:.3} ms",
        if pull {
            "pull-paced 60Hz"
        } else {
            "eager-push floor"
        },
        lat.len(),
        timeouts,
        pct(0.0),
        pct(0.50),
        pct(0.99),
        pct(1.0),
        mean,
    );

    session.shutdown().await;
}

/// Under pull pacing the daemon withholds every grid diff until a
/// `NextGridFrame` pull, then ships the accumulated state in one cycle
/// (docs/explanation/rendering/pipeline.md "Demand-driven emission").
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_pacing_withholds_until_a_frame_is_requested() {
    // Pure-`sh` loop + blocking `read`, not `seq`/`sleep`: NixOS keeps
    // coreutils off /usr/bin, and an early exit would masquerade as a
    // withhold.
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        "read x; i=1; while [ $i -le 500 ]; do echo line-$i; i=$((i+1)); done; read y",
    ]);
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("TERM", "xterm-256color");

    let mut session = RealAppSession::spawn(cmd, true).await;

    // Rehydrate is eager (not pull-gated), so it lands in the baseline.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cells_of = |c: &FrameCounters| c.count("RowDelta") + c.count("Scrolled");
    let baseline = cells_of(&session.counters());

    session.send_input(b"\n").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let withheld = session.counters();
    assert_eq!(
        cells_of(&withheld),
        baseline,
        "pull pacing must withhold grid diffs until a pull; counters: {withheld:?}",
    );
    assert!(
        session.with_shadow(|s| !shadow_contains(s, "line-")),
        "no burst line should reach the shadow before a pull",
    );

    if session.server_finished() {
        let outcome = session.server_outcome().await;
        panic!("daemon exited during the withheld burst (before any pull): {outcome:?}");
    }

    session.send_pull().await;
    let landed = session
        .wait_for_shadow(|s| shadow_contains(s, "line-500"), Duration::from_secs(2))
        .await;
    assert!(
        landed,
        "a NextGridFrame pull must ship the accumulated grid; shadow rows: {:?}",
        session.with_shadow(shadow_rows),
    );
    assert!(
        cells_of(&session.counters()) > baseline,
        "the pull must emit at least one cell-bearing frame",
    );

    session.shutdown().await;
}

/// An explicit SU (`CSI S`) on a quiet region emits one `Scrolled`
/// directive instead of N `RowDelta`s (docs/reference/ipc.md).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_su_directive_emits_scrolled_frame() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        "printf 'AAAA\\nBBBB\\nCCCC\\n'; \
         sleep 0.3; \
         printf '\\033[S'; \
         sleep 0.3",
    ]);
    cmd.env_clear();
    // The `sleep`s keep the SU out of the cycles that restate the
    // rows it would move. NixOS keeps coreutils off /usr/bin:/bin.
    let coreutils_dir = resolve_binary("sleep")
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .expect("`sleep` must be on the test runner's PATH");
    cmd.env("PATH", format!("{}:/usr/bin:/bin", coreutils_dir.display()));
    cmd.env("TERM", "xterm-256color");

    let session = RealAppSession::spawn(cmd, false).await;

    tokio::time::sleep(Duration::from_millis(700)).await;
    let counters = session.counters();
    eprintln!("explicit-SU counters: {counters:?}");

    assert!(
        counters.count("Scrolled") >= 1,
        "explicit `CSI S` must emit ≥ 1 GridMsg::Scrolled; got {counters:?}",
    );

    session.shutdown().await;
}

/// nvim's TUI startup (`-c qall!`) runs through the cursor and cell
/// paths.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nvim_alt_screen_entry_is_observed() {
    let Some(nvim) = skip_if_missing("nvim") else {
        return;
    };

    let mut cmd = Command::new(nvim);
    cmd.args(["-u", "NONE", "-c", "qall!"]);
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", "/tmp");

    let session = RealAppSession::spawn(cmd, false).await;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let counters = session.counters();
    eprintln!("nvim counters: {counters:?}");

    // The alt-screen flip is version-dependent (some `nvim -u NONE`
    // builds skip ?1049), so it is logged, not asserted.
    assert!(
        counters.count("CursorState") >= 1,
        "expected ≥ 1 CursorState frame from nvim startup; got {counters:?}",
    );
    assert!(
        counters.count("RowDelta") >= 1,
        "expected ≥ 1 cell-bearing frame from nvim; got {counters:?}",
    );
    let alt = session.with_shadow(ShadowScreen::alt_screen);
    if !alt {
        eprintln!(
            "note: nvim startup did not flip alt_screen; this version may use a different \
             enter sequence (counters: {counters:?})",
        );
    }

    session.shutdown().await;
}

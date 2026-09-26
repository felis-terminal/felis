//! Conformance smoke: drive ThomasDickey/esctest2 against `felis-vt`
//! and `felis-grid` in a PTY and assert the summary meets a tracked baseline.
//! Skipped when no `esctest` binary is on `PATH` (`ESCTEST_BIN` overrides).

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![expect(
    clippy::used_underscore_binding,
    reason = "conformance harness: relaxed lint posture for test scaffolding"
)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use felis_grid::Grid;
use felis_pty::spawn;
use felis_pty::{Command, Size};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;

mod common;
use common::{COLS, ROWS, host_replies, resolve_binary};

/// Lower bound on passed count under `--expected-terminal=xterm --max-vt-level=4`.
/// Ratchet: a commit that flips cases to PASS bumps this floor.
/// Failures are tracked in `docs/reference/esctest-compatibility.md`.
const PASS_BASELINE: u32 = 491;

struct EsctestSession {
    read_task: Option<JoinHandle<()>>,
    _writer: Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
}

impl EsctestSession {
    fn spawn_for(binary: PathBuf, logfile: &std::path::Path) -> Self {
        let mut cmd = Command::new(binary);
        cmd.env_clear();
        cmd.env("TERM", "xterm");
        cmd.env("LC_ALL", "C");
        cmd.env("PATH", "/usr/bin:/bin");
        // Options configure xterm parity, VT420 cap, quiet output, 1s timeout,
        // strict per-row reverse wraparound (?45 / 383), and private logfile.
        let mut argv = vec![
            "--expected-terminal=xterm".to_string(),
            "--max-vt-level=4".to_string(),
            "--no-print-logs".to_string(),
            "--timeout=1".to_string(),
            "--xterm-reverse-wrap=383".to_string(),
            format!("--logfile={}", logfile.display()),
        ];
        if let Ok(pattern) = std::env::var("ESCTEST_INCLUDE") {
            argv.push(format!("--include={pattern}"));
        }
        cmd.args(argv);

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
        .expect("spawn esctest");
        let (_reader, writer, _child, _resizer) = session.split();
        let _child = _child;
        let _resizer = _resizer;

        let writer_arc: Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>> =
            Arc::new(tokio::sync::Mutex::new(Box::new(writer)));

        let writer_task = Arc::clone(&writer_arc);
        let read_task = tokio::spawn(async move {
            let mut parser = felis_vt::Parser::new();
            let mut grid = Grid::new(ROWS, COLS);
            // The channel closes when the parse thread exits (PTY EOF).
            while let Some(chunk) = chunks_rx.recv().await {
                parser.advance(&mut grid, &chunk);
                let replies = host_replies(&mut grid);
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
            _writer: writer_arc,
        }
    }

    async fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let Some(handle) = self.read_task.take() else {
            return true;
        };
        tokio::time::timeout(timeout, handle).await.is_ok()
    }
}

impl Drop for EsctestSession {
    fn drop(&mut self) {
        if let Some(handle) = self.read_task.take() {
            handle.abort();
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct EsctestSummary {
    passed: u32,
    known: u32,
    failed: u32,
}

impl EsctestSummary {
    /// Parse `*** N tests passed, M known bugs, K TESTS FAILED ***` (or
    /// the singular `1 TEST FAILED` / `1 test passed` variants). `None`
    /// when the line is missing (the suite never finished) or the
    /// format drifts.
    fn parse(text: &str) -> Option<Self> {
        // Match by infix: failing tests also emit lines like
        // `*** TEST FooTests.test_Bar FAILED:`, so a match on `***`
        // alone would catch the wrong one.
        let line = text.lines().find(|l| {
            (l.contains(" tests passed") || l.contains(" test passed"))
                && (l.contains(" TESTS FAILED") || l.contains(" TEST FAILED"))
        })?;
        let body = line.trim_start_matches('*').trim_end_matches('*').trim();
        let mut parts = body.split(',').map(str::trim);
        let passed = leading_u32(parts.next()?, &["tests passed", "test passed"])?;
        let known = leading_u32(parts.next()?, &["known bugs", "known bug"])?;
        let failed = leading_u32(parts.next()?, &["TESTS FAILED", "TEST FAILED"])?;
        Some(Self {
            passed,
            known,
            failed,
        })
    }
}

fn leading_u32(segment: &str, suffixes: &[&str]) -> Option<u32> {
    let trimmed = segment.trim();
    for suffix in suffixes {
        if let Some(rest) = trimmed.strip_suffix(suffix) {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parses_canonical_summary_line() {
        let body = "irrelevant preamble\n\
            *** TEST FooTests.test_Bar FAILED: assertion mismatch\n\
            *** 152 tests passed, 43 known bugs, 373 TESTS FAILED ***\n\
            trailing junk\n";
        let s = EsctestSummary::parse(body).expect("parse");
        assert_eq!(s.passed, 152);
        assert_eq!(s.known, 43);
        assert_eq!(s.failed, 373);
    }

    #[test]
    fn returns_none_when_summary_line_absent() {
        let body = "*** TEST FooTests.test_Bar FAILED:\n\
            *** TEST BarTests.test_Baz FAILED:\n";
        assert!(EsctestSummary::parse(body).is_none());
    }

    #[test]
    fn parses_singular_count_variants() {
        let body = "*** 1 test passed, 1 known bug, 1 TEST FAILED ***\n";
        let s = EsctestSummary::parse(body).expect("parse");
        assert_eq!(s.passed, 1);
        assert_eq!(s.known, 1);
        assert_eq!(s.failed, 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn esctest_suite_pass_count_meets_baseline() {
    let Some(binary) = resolve_binary("ESCTEST_BIN", "esctest") else {
        eprintln!("skipping esctest smoke: binary not found in PATH or ESCTEST_BIN");
        return;
    };

    // Per-invocation logfile so parallel test runs do not race over
    // esctest's default /tmp/esctest.log.
    let logfile: PathBuf = {
        let mut p = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        p.push(format!("esctest-{}.log", std::process::id()));
        p
    };
    drop(std::fs::remove_file(&logfile));

    let mut session = EsctestSession::spawn_for(binary, &logfile);

    // esctest exercises >500 tests, many of which spend the full
    // per-test timeout waiting for a reply felis does not send.
    assert!(
        session.wait_for_exit(Duration::from_secs(1800)).await,
        "esctest did not exit within 30 minutes"
    );

    let log_text = std::fs::read_to_string(&logfile)
        .unwrap_or_else(|e| panic!("read esctest logfile {}: {e}", logfile.display()));
    let summary = EsctestSummary::parse(&log_text).unwrap_or_else(|| {
        let tail_start = log_text.len().saturating_sub(2_000);
        panic!(
            "esctest logfile missing summary line; last 2 KiB:\n{}",
            &log_text[tail_start..]
        );
    });

    eprintln!(
        "esctest result: {} passed, {} known bugs, {} FAILED",
        summary.passed, summary.known, summary.failed
    );

    assert!(
        summary.passed >= PASS_BASELINE,
        "esctest passed count regressed: got {}, baseline {} (failed: {}, known: {})",
        summary.passed,
        PASS_BASELINE,
        summary.failed,
        summary.known
    );
}

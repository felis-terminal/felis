//! Conformance smoke: drive ThomasDickey/esctest2 against `felis-vt`
//! and `felis-grid` in a PTY and assert the summary meets a tracked baseline.
//! A missing `esctest` (`ESCTEST_BIN` overrides `PATH`) fails the test.

use std::path::PathBuf;
use std::time::Duration;

use felis_grid::Grid;

use crate::common::{COLS, Harness, ROWS, conformance_command, require_binary};

/// Lower bound on passed count under `--expected-terminal=xterm --max-vt-level=4`.
/// Ratchet: a commit that flips cases to PASS bumps this floor.
/// Failures are tracked in `docs/reference/esctest-compatibility.md`.
const PASS_BASELINE: u32 = 491;

fn spawn_esctest(binary: PathBuf, logfile: &std::path::Path) -> Harness {
    let mut cmd = conformance_command(binary);
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
    Harness::spawn(cmd, Grid::new(ROWS, COLS))
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
    let binary = require_binary("ESCTEST_BIN", "esctest");

    // Per-invocation logfile so parallel test runs do not race over
    // esctest's default /tmp/esctest.log.
    let logfile: PathBuf = {
        let mut p = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        p.push(format!("esctest-{}.log", std::process::id()));
        p
    };
    drop(std::fs::remove_file(&logfile));

    let mut session = spawn_esctest(binary, &logfile);

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

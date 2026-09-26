//! Synchronized-output gate (REQ-207 / REQ-708 / REQ-1004): BSU holds
//! presentation until ESU or until `Grid::SYNC_OUTPUT_TIMEOUT` elapses,
//! which also clears the mode flag. Grid never reads wall clocks; caller
//! supplies `Instant`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use felis_grid::Grid;
use felis_vt::Parser;

mod common;
use common::drive_with;

#[test]
fn no_sync_means_ready_to_present_and_no_deadline() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    let now = Instant::now();
    drive_with(&mut p, &mut g, b"hello");
    assert!(g.ready_to_present(now));
    assert!(g.synchronized_output_deadline(now).is_none());
}

#[test]
fn bsu_holds_presentation_and_anchors_deadline() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    let t0 = Instant::now();
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    let deadline = g
        .synchronized_output_deadline(t0)
        .expect("BSU sets deadline");
    assert_eq!(deadline, t0 + Grid::SYNC_OUTPUT_TIMEOUT);
    assert!(!g.ready_to_present(t0));
    // The anchor is the first observation, not the latest.
    let t_later = t0 + Duration::from_millis(50);
    assert_eq!(g.synchronized_output_deadline(t_later), Some(deadline));
}

#[test]
fn esu_clears_flag_and_deadline() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    let t0 = Instant::now();
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    let _ = g.synchronized_output_deadline(t0);
    drive_with(&mut p, &mut g, b"\x1b[?2026l");
    assert!(!g.synchronized_output());
    assert!(g.synchronized_output_deadline(t0).is_none());
    assert!(g.ready_to_present(t0));
}

#[test]
fn timeout_releases_gate_and_clears_flag() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    let t0 = Instant::now();
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    let _ = g.synchronized_output_deadline(t0);
    let elapsed = t0 + Grid::SYNC_OUTPUT_TIMEOUT;
    assert!(g.ready_to_present(elapsed));
    assert!(
        !g.synchronized_output(),
        "timeout fallback must clear the BSU flag — otherwise a subsequent BSU \
         would see stale state and not re-anchor the deadline"
    );
    assert!(g.synchronized_output_deadline(elapsed).is_none());
}

#[test]
fn one_ns_before_deadline_is_still_held() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    let t0 = Instant::now();
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    let _ = g.synchronized_output_deadline(t0);
    let almost = (t0 + Grid::SYNC_OUTPUT_TIMEOUT)
        .checked_sub(Duration::from_nanos(1))
        .unwrap();
    assert!(!g.ready_to_present(almost));
    assert!(g.synchronized_output());
}

#[test]
fn fresh_bsu_after_timeout_re_arms_window() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    let t0 = Instant::now();
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    let _ = g.synchronized_output_deadline(t0);
    let t1 = t0 + Grid::SYNC_OUTPUT_TIMEOUT;
    assert!(g.ready_to_present(t1));
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    let new_deadline = g.synchronized_output_deadline(t1).expect("re-armed");
    assert_eq!(new_deadline, t1 + Grid::SYNC_OUTPUT_TIMEOUT);
    assert!(!g.ready_to_present(t1));
}

#[test]
fn redundant_bsu_does_not_extend_deadline() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    let t0 = Instant::now();
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    let anchored = g.synchronized_output_deadline(t0).unwrap();
    let t1 = t0 + Duration::from_millis(100);
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    assert_eq!(
        g.synchronized_output_deadline(t1),
        Some(anchored),
        "duplicate BSU must keep the original anchor — otherwise a buggy \
         producer could pin emission indefinitely by spamming ?2026h"
    );
}

/// The gate hides emission timing, not which rows changed.
#[test]
fn damage_survives_through_bsu_until_release() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    g.damage_mut().clear();
    let t0 = Instant::now();
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    drive_with(&mut p, &mut g, b"abc\r\ndef");
    assert!(!g.ready_to_present(t0));
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert!(dirty.contains(&0));
    assert!(dirty.contains(&1));
    drive_with(&mut p, &mut g, b"\x1b[?2026l");
    assert!(g.ready_to_present(t0));
    let dirty_after: Vec<_> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, dirty_after);
}

#[test]
fn decstr_clears_sync_output_state() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    let t0 = Instant::now();
    drive_with(&mut p, &mut g, b"\x1b[?2026h");
    let _ = g.synchronized_output_deadline(t0);
    drive_with(&mut p, &mut g, b"\x1b[!p");
    assert!(!g.synchronized_output());
    assert!(g.synchronized_output_deadline(t0).is_none());
}

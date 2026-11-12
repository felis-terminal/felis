use crate::test_support::drive;
use crate::*;
use felis_vt::Parser;

#[test]
fn bracketed_paste_mode_toggles_via_2004() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    assert!(!g.bracketed_paste());
    drive(&mut p, &mut g, b"\x1b[?2004h");
    assert!(g.bracketed_paste());
    drive(&mut p, &mut g, b"\x1b[?2004l");
    assert!(!g.bracketed_paste());
}

#[test]
fn focus_reporting_mode_toggles_via_1004() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    assert!(!g.focus_reporting());
    drive(&mut p, &mut g, b"\x1b[?1004h");
    assert!(g.focus_reporting());
    drive(&mut p, &mut g, b"\x1b[?1004l");
    assert!(!g.focus_reporting());
}

#[test]
fn private_modes_compose_in_one_dispatch() {
    // ?25 + ?2004 + ?1004 in one CSI must all flip together.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b[?25;2004;1004h");
    assert!(g.cursor().visible);
    assert!(g.bracketed_paste());
    assert!(g.focus_reporting());
    drive(&mut p, &mut g, b"\x1b[?25;2004;1004l");
    assert!(!g.cursor().visible);
    assert!(!g.bracketed_paste());
    assert!(!g.focus_reporting());
}

/// REQ-902: edge-triggered, so a program spamming resets cannot flood
/// the daemon's log.
#[test]
fn disabling_bracketed_paste_queues_one_edge_triggered_warning() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"\x1b[?2004h");
    drop(g.take_pty_effects());
    drive(&mut p, &mut g, b"\x1b[?2004l\x1b[?2004l");
    let warnings = g
        .take_pty_effects()
        .iter()
        .filter(|e| matches!(e, PtyEffect::BracketedPasteDisabled))
        .count();
    assert_eq!(warnings, 1, "edge-triggered: on→off fires once");
    assert!(!g.bracketed_paste());
}

#[test]
fn synchronized_output_mode_toggles_via_2026() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    assert!(!g.synchronized_output());
    drive(&mut p, &mut g, b"\x1b[?2026h");
    assert!(g.synchronized_output());
    drive(&mut p, &mut g, b"\x1b[?2026l");
    assert!(!g.synchronized_output());
}

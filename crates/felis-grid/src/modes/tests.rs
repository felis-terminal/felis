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

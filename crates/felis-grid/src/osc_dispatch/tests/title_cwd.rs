use super::*;

#[test]
fn osc_2_sets_window_title() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]2;hello-felis\x1b\\");
    assert_eq!(g.title(), Some("hello-felis"));
    assert_eq!(g.take_title_dirty().as_deref(), Some("hello-felis"));
    assert!(g.take_title_dirty().is_none(), "second call clears flag");
}

#[test]
fn osc_1_sets_the_icon_name_but_not_the_title() {
    // The icon name is observable through `CSI 20 t` (reported as
    // `OSC L … ST`).
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]1;icon-only\x07");
    assert_eq!(g.title(), None);
    drive(&mut p, &mut g, b"\x1b[20t");
    let reply = responses(&mut g).into_iter().next().unwrap();
    assert_eq!(String::from_utf8(reply).unwrap(), "\x1b]Licon-only\x1b\\");
}

/// The icon half is observable through `CSI 20 t`.
#[test]
fn osc_0_sets_both_title_and_icon_name() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]0;both\x07");
    assert_eq!(g.title(), Some("both"));
    drive(&mut p, &mut g, b"\x1b[20t");
    assert_eq!(reply_text(&mut g), ["\x1b]Lboth\x1b\\"]);
}

#[test]
fn osc_title_aborted_by_can_does_not_install_partial_value() {
    // `docs/explanation/security-model.md`: a payload truncated by CAN
    // (0x18) or SUB (0x1A) must not install its partial value.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]2;original\x07");
    assert_eq!(g.title(), Some("original"));
    drive(&mut p, &mut g, b"\x1b]2;half-\x18");
    assert_eq!(g.title(), Some("original"));
    drive(&mut p, &mut g, b"\x1b]2;rogue\x1a");
    assert_eq!(g.title(), Some("original"));
}

#[test]
fn osc_title_only_marks_dirty_when_value_changes() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]2;same\x07");
    assert!(g.take_title_dirty().is_some());
    drive(&mut p, &mut g, b"\x1b]2;same\x07");
    assert!(g.take_title_dirty().is_none());
    drive(&mut p, &mut g, b"\x1b]2;changed\x07");
    assert_eq!(g.take_title_dirty().as_deref(), Some("changed"));
}

#[test]
fn osc_2_invalid_utf8_is_rejected() {
    // `sanitize_osc_str` returns `None` on `from_utf8` failure; a "lossy
    // from_utf8" refactor (replacement-char title) must surface here.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    // 0xC0 is invalid as a UTF-8 leading byte.
    drive(&mut p, &mut g, b"\x1b]2;valid\xc0tail\x07");
    assert_eq!(g.title(), None);
}

#[test]
fn osc_7_records_cwd_url() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(
        &mut p,
        &mut g,
        b"\x1b]7;file://localhost/home/user/proj\x1b\\",
    );
    assert_eq!(g.cwd(), Some("file://localhost/home/user/proj"));
    assert_eq!(
        g.take_cwd_dirty().as_deref(),
        Some("file://localhost/home/user/proj"),
    );
    assert!(g.take_cwd_dirty().is_none(), "second take clears");
}

#[test]
fn osc_7_with_control_byte_is_rejected() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]7;file://localhost/oops\x01tail\x07");
    assert_eq!(g.cwd(), None);
}

#[test]
fn osc_7_idempotent_on_same_value() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]7;file:///a\x07");
    assert!(g.take_cwd_dirty().is_some());
    drive(&mut p, &mut g, b"\x1b]7;file:///a\x07");
    assert!(g.take_cwd_dirty().is_none());
    drive(&mut p, &mut g, b"\x1b]7;file:///b\x07");
    assert_eq!(g.take_cwd_dirty().as_deref(), Some("file:///b"));
}

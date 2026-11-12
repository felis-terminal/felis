use super::*;

/// The race `InputMsg::Key` exists for: the daemon parses a mode
/// change, the subscriber has not been handed the `ModeFlags` frame
/// that mirrors it, and the very next key must still encode under
/// the new mode.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_encodes_against_a_mode_the_subscriber_has_not_been_told_about() {
    let tmp = tempfile::TempDir::new().unwrap();
    let received = tmp.path().join("keys");
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!("stty raw -echo; exec cat > {}", received.display()),
    ]);
    cmd.env_clear();
    cmd.env("PATH", crate::fixture_path());
    let mut task = SessionTask::for_tests(Session::from_spawned(
        crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"),
    ));

    let (sub, mut rx) = attach_sub(&mut task, false);
    drain_events(&mut rx);

    // Straight into the grid, so nothing is fanned out: the
    // subscriber's shadow still has DECCKM off when the key arrives.
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut task.session.lock_core().grid, b"\x1b[?1h");
    assert!(
        task.session
            .lock_core()
            .grid
            .mode_snapshot()
            .application_cursor,
        "fixture must leave DECCKM set daemon-side",
    );

    let end = task.handle_input(
        sub,
        InputMsg::Key(KeyEvent {
            key: felis_protocol::messages::Key::Named(felis_protocol::messages::NamedKey::ArrowUp),
            text: None,
            mods: felis_protocol::messages::KeyMods::empty(),
            kind: KeyEventKind::Press,
            location: felis_protocol::messages::KeyLocation::Standard,
        }),
    );
    assert!(end.is_none(), "Key is a continuing input");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::read(&received).is_ok_and(|bytes| bytes.ends_with(b"\x1bOA")) {
            break;
        }
        let seen = std::fs::read(&received).unwrap_or_default();
        assert!(
            tokio::time::Instant::now() < deadline,
            "the arrow reached the child as {seen:?}, not the DECCKM form ESC O A",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let told_after = drain_events(&mut rx).iter().any(|ev| {
        matches!(
            ev,
            OutEvent::Grid(GridMsg::ModeFlags {
                application_cursor: true,
                ..
            })
        )
    });
    assert!(
        told_after,
        "the mode mirror must still have been undelivered when the key was encoded",
    );
}

/// Repeat is the one event kind with no `KeyBytes` equivalent: it
/// reports Kitty event type 2 while event types are on, and as a
/// press otherwise.
#[test]
fn a_repeat_reports_kitty_event_type_two_only_while_event_types_are_on() {
    use crate::serve::key_encode::{KittyKbdFlags, encode};
    use felis_protocol::messages::{Key, KeyLocation, KeyMods, ModifyOtherKeys};

    let at = |flags: KittyKbdFlags, kind: KeyEventKind| {
        encode(
            &Key::Named(felis_protocol::messages::NamedKey::Escape),
            None,
            KeyMods::empty(),
            kind,
            flags,
            ModifyOtherKeys::Off,
            false,
            false,
            false,
            KeyLocation::Standard,
        )
    };
    let with_events = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
    assert_eq!(
        at(with_events, KeyEventKind::Repeat).as_deref(),
        Some(&b"\x1b[27;1:2u"[..]),
    );
    assert_eq!(
        at(KittyKbdFlags::DISAMBIGUATE, KeyEventKind::Repeat),
        at(KittyKbdFlags::DISAMBIGUATE, KeyEventKind::Press),
        "without event types a repeat has no distinct report",
    );
    assert_eq!(
        at(KittyKbdFlags::empty(), KeyEventKind::Repeat).as_deref(),
        Some(&b"\x1b"[..]),
        "legacy encodings have no event field at all",
    );
}

/// Input fans in: every subscriber's `KeyBytes` reaches the one PTY
/// writer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_bytes_from_any_subscriber_reach_the_pty() {
    let mut task = SessionTask::for_tests(session_with("read x; printf 'got=%s' \"$x\""));
    let (_sub_a, _rx_a) = attach_sub(&mut task, false);
    let (sub_b, _rx_b) = attach_sub(&mut task, false);

    let end = task.handle_input(sub_b, InputMsg::KeyBytes(b"HELLO\n".to_vec()));
    assert!(end.is_none());
    wait_for_grid_text(&task.session.core, "got=HELLO").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_feeds_bytes_back_as_input() {
    let mut task = SessionTask::for_tests(session_with("read x; printf 'got=%s' \"$x\""));
    let (sub, _rx) = attach_sub(&mut task, false);

    task.handle_input(sub, InputMsg::Paste(b"PASTED\n".to_vec()));

    wait_for_grid_text(&task.session.core, "got=PASTED").await;
}

#[test]
fn a_mouse_event_without_pixels_reports_its_cell_center_under_sgr_pixels() {
    use felis_protocol::messages::{InputMods, MouseAction, MouseButton, MouseEvent};

    let mut core = ParseCore::new(24, 80);
    core.grid.set_winsize_pixels((809, 384));
    core.parser
        .advance(&mut core.grid, b"\x1b[?1000h\x1b[?1016h");
    let event = |px, py| {
        InputMsg::Mouse(MouseEvent {
            button: Some(MouseButton::Left),
            action: MouseAction::Press,
            mods: InputMods::empty(),
            x: 80,
            y: 1,
            px,
            py,
        })
    };
    assert_eq!(
        encode_input(&core, &event(0, 0)).as_deref(),
        Some(&b"\x1b[<0;804;8M"[..]),
    );
    assert_eq!(
        encode_input(&core, &event(805, 3)).as_deref(),
        Some(&b"\x1b[<0;805;3M"[..]),
        "a window's pixels pass through",
    );
}

/// Pointer motion over a mirror does not transfer size ownership
/// while the running program has no mouse protocol.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mouse_motion_without_mouse_protocol_does_not_transfer_ownership() {
    use felis_protocol::messages::{InputMods, MouseAction, MouseEvent};

    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, _rx_a) = attach_sub(&mut task, false);
    let (sub_b, _rx_b) = attach_sub(&mut task, false);

    task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
    assert_eq!(task.active_sub, Some(sub_a));

    task.handle_input(
        sub_b,
        InputMsg::Mouse(MouseEvent {
            button: None,
            action: MouseAction::Motion,
            mods: InputMods::empty(),
            x: 3,
            y: 3,
            px: 3,
            py: 3,
        }),
    );
    assert_eq!(
        task.active_sub,
        Some(sub_a),
        "hover must not steal the PTY size while no mouse protocol is active",
    );
}

/// A key release from a mirror does not transfer size ownership while
/// the running program has not asked for release events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreported_key_release_does_not_transfer_ownership() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, _rx_a) = attach_sub(&mut task, false);
    let (sub_b, _rx_b) = attach_sub(&mut task, false);

    task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
    assert_eq!(task.active_sub, Some(sub_a));

    task.handle_input(
        sub_b,
        InputMsg::Key(KeyEvent {
            key: felis_protocol::messages::Key::Character("b".into()),
            text: None,
            mods: felis_protocol::messages::KeyMods::empty(),
            kind: KeyEventKind::Release,
            location: felis_protocol::messages::KeyLocation::Standard,
        }),
    );
    assert_eq!(
        task.active_sub,
        Some(sub_a),
        "a release that encodes to no bytes must not steal the PTY size",
    );
}

const fn click(action: felis_protocol::messages::MouseAction, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        button: Some(felis_protocol::messages::MouseButton::Left),
        action,
        mods: felis_protocol::messages::InputMods::empty(),
        x,
        y,
        px: 0,
        py: 0,
    }
}

#[test]
fn the_last_cell_of_the_grid_is_in_bounds_and_the_next_is_not() {
    use felis_protocol::messages::MouseAction::Press;
    let any = MouseProtocol::AnyMotion;
    assert_eq!(unreported_mouse(click(Press, 80, 24), any, 24, 80), None);
    for (x, y) in [(81, 24), (80, 25), (0, 1), (1, 0)] {
        assert_eq!(
            unreported_mouse(click(Press, x, y), any, 24, 80).as_deref(),
            Some("the cell is outside the 80x24 grid (COL and ROW are 0-based)"),
            "{x},{y}",
        );
    }
}

#[test]
fn an_event_the_protocol_filters_is_refused() {
    use felis_protocol::messages::MouseAction::{Drag, Press};
    let refused = |action, protocol| unreported_mouse(click(action, 1, 1), protocol, 24, 80);
    assert_eq!(
        refused(Press, MouseProtocol::Off).as_deref(),
        Some("the program has no mouse reporting on; nothing was sent"),
    );
    assert_eq!(
        refused(Drag, MouseProtocol::ButtonEvents).as_deref(),
        Some("the program reports clicks but not drags (?1000 without ?1002); nothing was sent"),
    );
    assert_eq!(refused(Press, MouseProtocol::ButtonEvents), None);
    assert_eq!(refused(Drag, MouseProtocol::ButtonAndDrag), None);
}

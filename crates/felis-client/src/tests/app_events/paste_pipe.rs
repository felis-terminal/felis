use super::*;

/// The boundary of the window's paste admission (REQ-105a), which every
/// producer of an `InputMsg::Paste` shares: the clipboard, a file drop,
/// and the pipe chord's `paste` sink, whose region the daemon admits up
/// to twice `MAX_PASTE_BYTES`, so it is the one that can reach the cap
/// without a human holding a 16 MiB clipboard.
#[test]
fn a_paste_at_the_limit_is_admitted_and_one_past_it_names_its_size() {
    use felis_protocol::messages::MAX_PASTE_BYTES;

    assert_eq!(app_methods::paste_refusal(MAX_PASTE_BYTES), None);
    let notice =
        app_methods::paste_refusal(MAX_PASTE_BYTES + 1).expect("one byte past the cap is refused");
    assert!(
        notice.contains(&(MAX_PASTE_BYTES + 1).to_string())
            && notice.contains(&MAX_PASTE_BYTES.to_string()),
        "the notice names both numbers, got {notice}",
    );
}

fn press(key: PressedKey, text: Option<&str>, mods: Modifiers) -> KeyPress {
    KeyPress {
        key,
        text: text.map(str::to_owned),
        mods,
        kind: felis_protocol::messages::KeyEventKind::Press,
        location: felis_protocol::messages::KeyLocation::Standard,
    }
}

/// Pins `input::snaps_to_live` at the window: Cmd alone must not
/// drop a scrolled-back view before Cmd+C can copy from it, and the
/// next typed key must.
#[test]
fn a_bare_modifier_keeps_the_scrollback_view_and_a_typed_key_leaves_it() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    let live = harness.app.conn_gen;
    harness.app.on_app_event(AppEvent::GridFrame {
        msg: GridMsg::ViewportState {
            lines_from_bottom: 5,
            max: 100,
            anchor: None,
        },
        conn_gen: live,
    });
    assert!(!harness.app.viewport_live());
    harness.sent();

    let cmd = press(PressedKey::Modifier, None, Modifiers::SUPER);
    harness.app.on_window_input(WindowInput::Key(cmd.clone()));
    assert_eq!(
        harness.sent(),
        vec![input_frame(&InputMsg::Key(cmd.to_wire()))]
    );

    let a = press(
        PressedKey::Character("a".into()),
        Some("a"),
        Modifiers::empty(),
    );
    harness.app.on_window_input(WindowInput::Key(a.clone()));
    assert_eq!(
        harness.sent(),
        vec![
            input_frame(&InputMsg::Viewport {
                lines_from_bottom: 0,
                anchor: None,
            }),
            input_frame(&InputMsg::Key(a.to_wire())),
        ]
    );
}

/// Pins the confirmation bar's key contract (docs/explanation/input.md
/// "Confirmation bar"): Shift on the way to `Y` leaves the question
/// open, and `Y` sends the paste it held.
#[test]
fn a_held_paste_waits_through_a_bare_modifier_and_goes_out_on_yes() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    let paste = b"rm -rf build\n".to_vec();

    harness.app.ferry_paste(&paste);
    assert!(harness.app.pending_confirm.is_some());
    assert!(
        harness.sent().is_empty(),
        "nothing goes out before the answer"
    );

    let shift = press(PressedKey::Modifier, None, Modifiers::SHIFT);
    harness.app.on_window_input(WindowInput::Key(shift));
    assert!(harness.app.pending_confirm.is_some());
    assert!(harness.sent().is_empty(), "the modifier is not forwarded");

    let yes = press(
        PressedKey::Character("Y".into()),
        Some("Y"),
        Modifiers::SHIFT,
    );
    harness.app.on_window_input(WindowInput::Key(yes));
    assert!(harness.app.pending_confirm.is_none());
    assert_eq!(harness.sent(), vec![input_frame(&InputMsg::Paste(paste))]);
}

/// The other half of the contract: any key but a bare modifier
/// answers, a dead key and an unbound named key included, and the
/// answering key is consumed rather than typed.
#[test]
fn a_dead_key_or_an_unbound_key_cancels_a_held_paste() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    for answer in [
        press(PressedKey::Other, None, Modifiers::empty()),
        press(
            PressedKey::Named(felis_client_core::NamedKey::Escape),
            None,
            Modifiers::empty(),
        ),
    ] {
        harness.app.ferry_paste(b"make\n");
        assert!(harness.app.pending_confirm.is_some());

        harness
            .app
            .on_window_input(WindowInput::Key(answer.clone()));
        assert!(harness.app.pending_confirm.is_none(), "{answer:?} answers");
        assert!(
            harness.sent().is_empty(),
            "{answer:?} cancels and is consumed"
        );
    }
}

#[test]
fn a_pipe_paste_sink_holds_control_bytes_for_confirmation_like_ctrl_v() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    let live = ViewportPosition {
        offset: 0,
        anchor: None,
    };

    harness
        .app
        .finish_pipe(b"rm -rf build\n", None, PipeTarget::Paste, live);
    assert!(harness.app.pending_confirm.is_some());
    assert!(
        harness.sent().is_empty(),
        "nothing goes out before the answer"
    );
    harness.app.pending_confirm = None;

    harness
        .app
        .finish_pipe(b"make", None, PipeTarget::Paste, live);
    assert!(harness.app.pending_confirm.is_none());
    assert_eq!(
        harness.sent(),
        vec![input_frame(&InputMsg::Paste(b"make".to_vec()))]
    );

    let conn_gen = harness.app.conn_gen;
    harness.app.on_app_event(AppEvent::GridFrame {
        msg: mode_flags(true, false, felis_protocol::messages::MouseProtocol::Off),
        conn_gen,
    });
    harness
        .app
        .finish_pipe(b"rm -rf build\n", None, PipeTarget::Paste, live);
    assert!(
        harness.app.pending_confirm.is_none(),
        "bracketed paste frames the line break, so nothing to confirm"
    );
    assert_eq!(
        harness.sent(),
        vec![input_frame(&InputMsg::Paste(b"rm -rf build\n".to_vec()))]
    );
}

#[test]
fn the_osc_52_pipe_source_replays_the_last_clipboard_write_a_program_made() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    let live = harness.app.conn_gen;

    harness
        .app
        .start_pipe(PipeRegionSource::Osc52, PipeTarget::Paste, false);
    assert!(harness.sent().is_empty(), "no write yet, nothing to pipe");

    harness.app.on_app_event(clipboard_frame(
        ClipboardSelection::CLIPBOARD,
        b"yanked",
        live,
    ));
    harness.app.on_app_event(clipboard_frame(
        ClipboardSelection::PRIMARY,
        b"primary only",
        live,
    ));
    harness
        .app
        .start_pipe(PipeRegionSource::Osc52, PipeTarget::Paste, false);
    assert_eq!(
        harness.sent(),
        vec![input_frame(&InputMsg::Paste(b"yanked".to_vec()))]
    );
}

/// The window's last `c` write: a later one replaces it, a frame from
/// a superseded connection does not, and a session switch keeps it.
#[cfg(unix)]
#[test]
fn the_osc_52_pipe_value_follows_the_latest_live_write_across_a_switch() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    let write =
        |data: &[u8], conn_gen| clipboard_frame(ClipboardSelection::CLIPBOARD, data, conn_gen);
    let live = harness.app.conn_gen;

    harness.app.on_app_event(write(b"first", live));
    harness.app.on_app_event(write(b"second", live));
    assert_eq!(
        harness.app.last_osc_52_write.as_deref(),
        Some(&b"second"[..])
    );

    harness.app.on_app_event(write(b"stale", live + 1));
    assert_eq!(
        harness.app.last_osc_52_write.as_deref(),
        Some(&b"second"[..])
    );

    let _far = land_switch(&runtime, &mut harness, 0xb);
    assert_eq!(
        harness.app.last_osc_52_write.as_deref(),
        Some(&b"second"[..])
    );
}

/// The `clipboard.osc_52 = "mirror"` gate as a backend sees it:
/// program writes are dropped, user writes land.
#[derive(Default)]
struct MirrorGatedClipboard(InMemoryClipboard);

impl Clipboard for MirrorGatedClipboard {
    fn read(&self) -> Result<Vec<u8>, felis_client_core::clipboard::ClipboardError> {
        self.0.read()
    }
    fn write(&self, _bytes: &[u8]) -> Result<(), felis_client_core::clipboard::ClipboardError> {
        Ok(())
    }
    fn write_user(&self, bytes: &[u8]) -> Result<(), felis_client_core::clipboard::ClipboardError> {
        self.0.write(bytes)
    }
}

#[test]
fn the_osc_52_pipe_chord_promotes_a_gated_program_write_to_the_clipboard() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("home"), 0xa);
    let clipboard = Arc::new(MirrorGatedClipboard::default());
    harness.app.clipboard = clipboard.clone();
    let conn_gen = harness.app.conn_gen;

    harness.app.on_app_event(clipboard_frame(
        ClipboardSelection::CLIPBOARD,
        b"yanked",
        conn_gen,
    ));
    assert_eq!(
        clipboard.read().unwrap(),
        b"",
        "the gate kept the program out"
    );

    harness
        .app
        .start_pipe(PipeRegionSource::Osc52, PipeTarget::Clipboard, false);
    assert_eq!(clipboard.read().unwrap(), b"yanked");
}

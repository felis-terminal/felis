use super::*;

/// Pins: an assistive technology's first query on an idle window
/// still reaches the redraw that carries the initial tree.
#[test]
fn an_initial_tree_request_on_an_idle_window_requests_a_redraw() {
    let runtime = runtime();
    let mut harness = app_on(&runtime, absent_daemon("a11y"), 0xa);
    let _ = harness.app.redraw.flush();
    assert!(!harness.app.redraw.flush());

    harness.app.on_app_event(AppEvent::Accessibility(
        accesskit_winit::WindowEvent::InitialTreeRequested,
    ));
    assert!(harness.app.redraw.flush());
}

fn announce_minor() -> u16 {
    use felis_protocol::minor::MinorGated as _;
    InputMsg::AnnounceOutput { enabled: true }.requires().minor
}

/// A harness on a platform that announces, against a daemon that
/// knows the opt-in.
fn announcing_app(runtime: &tokio::runtime::Runtime) -> AppHarness {
    let mut harness = app_on(runtime, absent_daemon("announce"), 0xa);
    harness.app.announce_output = true;
    harness.app.effective_minor = announce_minor();
    harness
}

fn command_output(text: &str) -> GridMsg {
    GridMsg::CommandOutput {
        text: text.to_owned(),
        omitted: felis_protocol::messages::OmittedOutput::None,
    }
}

#[test]
fn the_announcement_opt_in_follows_the_tree_activity() {
    let runtime = runtime();
    let mut harness = announcing_app(&runtime);
    harness.sent();

    harness.app.on_app_event(AppEvent::Accessibility(
        accesskit_winit::WindowEvent::InitialTreeRequested,
    ));
    assert_eq!(
        harness.sent(),
        vec![input_frame(&InputMsg::AnnounceOutput { enabled: true })]
    );

    harness.app.on_app_event(AppEvent::Accessibility(
        accesskit_winit::WindowEvent::AccessibilityDeactivated,
    ));
    assert_eq!(
        harness.sent(),
        vec![input_frame(&InputMsg::AnnounceOutput { enabled: false })]
    );
}

#[test]
fn a_platform_without_announcements_never_opts_in() {
    let runtime = runtime();
    let mut harness = announcing_app(&runtime);
    harness.app.announce_output = false;
    harness.sent();

    harness.app.on_app_event(AppEvent::Accessibility(
        accesskit_winit::WindowEvent::InitialTreeRequested,
    ));
    assert_eq!(harness.sent(), vec![]);
}

#[test]
fn a_new_app_announces_output_as_the_platform_constant_says() {
    let runtime = runtime();
    let harness = app_on(&runtime, absent_daemon("platform"), 0xa);
    assert_eq!(harness.app.announce_output, a11y::ANNOUNCES_OUTPUT);
}

/// Lands while the tree is active, so the new attachment, which
/// starts disabled, must be told again.
#[cfg(unix)]
#[test]
fn a_landing_while_active_sends_the_opt_in_again() {
    let runtime = runtime();
    let mut harness = announcing_app(&runtime);
    harness.app.on_app_event(AppEvent::Accessibility(
        accesskit_winit::WindowEvent::InitialTreeRequested,
    ));

    let far = land_switch_at(&runtime, &mut harness, 0xb, announce_minor());

    let expected = input_frame(&InputMsg::AnnounceOutput { enabled: true });
    let mut reader = FrameReader::new(far);
    let found = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Ok(Some(frame)) = reader.next_frame().await {
                if frame.kind == expected.kind() && frame.body[..] == *expected.body() {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    });
    assert!(found, "the new connection carries the opt-in");
}

#[test]
fn command_output_is_announced_only_in_a_focused_window() {
    let runtime = runtime();
    let mut harness = announcing_app(&runtime);
    harness.app.on_app_event(AppEvent::Accessibility(
        accesskit_winit::WindowEvent::InitialTreeRequested,
    ));
    let live = harness.app.conn_gen;

    harness.app.on_app_event(AppEvent::GridFrame {
        msg: command_output("a\n\nb"),
        conn_gen: live,
    });
    assert_eq!(harness.app.a11y.pending_announcements(), ["a\nb"]);

    harness.app.on_window_input(WindowInput::Focused(false));
    assert!(
        harness.app.a11y.pending_announcements().is_empty(),
        "a focus loss drops what was not yet flushed"
    );

    harness.app.on_app_event(AppEvent::GridFrame {
        msg: command_output("c"),
        conn_gen: live,
    });
    assert_eq!(
        harness.app.a11y.pending_announcements(),
        <&[String]>::default()
    );
}

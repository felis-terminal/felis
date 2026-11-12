use super::*;

fn report_theme(task: &mut SessionTask, sub: SubscriberId, bg: Option<(u8, u8, u8)>) {
    report_theme_and_palette(task, sub, bg, &[]);
}

fn report_theme_and_palette(
    task: &mut SessionTask,
    sub: SubscriberId,
    bg: Option<(u8, u8, u8)>,
    palette: &[(u8, (u8, u8, u8))],
) {
    let end = task.handle_cmd(SessionCmd::ConfigureTheme {
        sub,
        fg: Some((0xE5, 0xE5, 0xE5)),
        bg,
        cursor: None,
        palette: palette.iter().copied().collect(),
    });
    assert!(end.is_none(), "ConfigureTheme is a continuing command");
}

fn effective_palette(task: &SessionTask, index: u8) -> Option<(u8, u8, u8)> {
    task.session.lock_core().grid.palette_config_default(index)
}

fn report_scheme(task: &mut SessionTask, sub: SubscriberId, dark: bool) {
    let end = task.handle_input(sub, InputMsg::ColorScheme { dark });
    assert!(end.is_none(), "ColorScheme is a continuing input");
}

fn effective_bg(task: &SessionTask) -> Option<(u8, u8, u8)> {
    task.session
        .lock_core()
        .grid
        .theme_config_default(ThemeChannel::Background)
}

fn effective_dark(task: &SessionTask) -> bool {
    task.session.lock_core().grid.os_dark() == Some(true)
}

fn stored_of(task: &SessionTask, sub: SubscriberId) -> (Option<(u8, u8, u8)>, Option<bool>) {
    let s = task
        .subs
        .iter()
        .find(|s| s.id == sub)
        .expect("subscriber still attached");
    (
        s.presentation
            .theme
            .as_ref()
            .and_then(|t| t.channels[ThemeChannel::Background as usize]),
        s.presentation.os_dark,
    )
}

/// The last window's theme and OS scheme outlive it, and an `Ops`
/// reader still attached never takes them over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_window_detach_keeps_the_last_resolved_theme_and_scheme() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (win, _rx) = attach_sub(&mut task, false);
    let (ops, _rx_ops) = attach_ops_sub(&mut task);
    report_theme(&mut task, win, Some((0x0D, 0x0D, 0x12)));
    report_scheme(&mut task, win, true);
    report_theme(&mut task, ops, Some((0xFF, 0xFF, 0xFF)));
    report_scheme(&mut task, ops, false);
    assert_eq!(effective_bg(&task), Some((0x0D, 0x0D, 0x12)));
    assert!(effective_dark(&task));

    task.remove_sub(win);

    assert_eq!(effective_bg(&task), Some((0x0D, 0x0D, 0x12)));
    assert_eq!(
        task.session.lock_core().grid.color_scheme_report_bytes(),
        b"\x1b[?997;1n",
        "the parked session keeps answering the departed window's dark",
    );
}

/// A window that has attached but not yet reported resolves nothing,
/// so the parked scheme holds until its report lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reattach_to_a_parked_session_notifies_nothing_before_or_after_its_report() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    {
        let mut core = task.session.lock_core();
        let mut parser = felis_vt::Parser::new();
        parser.advance(&mut core.grid, b"\x1b[?2031h");
    }
    let (a, _rx_a) = attach_sub(&mut task, false);
    report_scheme(&mut task, a, true);
    assert_eq!(task.pty_steps, [PtyStep::SchemeReport]);

    task.remove_sub(a);
    assert!(task.sync_color_scheme_to_pty().is_none());
    let (b, _rx_b) = attach_sub(&mut task, false);
    assert!(task.sync_color_scheme_to_pty().is_none());
    assert!(effective_dark(&task), "the unreported window moved nothing");
    report_scheme(&mut task, b, true);

    assert_eq!(
        task.pty_steps,
        [PtyStep::SchemeReport],
        "detach, attach, and a same-scheme report owe the program nothing: {:?}",
        task.pty_steps,
    );
}

/// A mirror's later report is stored but changes nothing a query
/// can see while the owner has reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mirrors_report_is_stored_but_the_owner_still_answers() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (a, mut rx_a) = attach_sub(&mut task, false);
    let (b, mut rx_b) = attach_sub(&mut task, false);
    take_input_ownership(&mut task, a);
    // The echo has to be on the grid and shipped before the reports,
    // or a report's cycle carries it and the last assertion measures
    // the keystroke.
    wait_for_grid_text(&task.session.core, "k").await;
    task.after_drive();
    drain_events(&mut rx_a);
    drain_events(&mut rx_b);
    report_theme(&mut task, a, Some((0x11, 0x11, 0x11)));
    report_scheme(&mut task, a, false);
    report_theme(&mut task, a, Some((0x11, 0x11, 0x11)));
    report_scheme(&mut task, a, false);

    report_theme(&mut task, b, Some((0x22, 0x22, 0x22)));
    report_scheme(&mut task, b, true);

    assert_eq!(
        effective_bg(&task),
        Some((0x11, 0x11, 0x11)),
        "the owner keeps the trio",
    );
    assert!(!effective_dark(&task), "the owner keeps the OS scheme");
    assert_eq!(
        stored_of(&task, b),
        (Some((0x22, 0x22, 0x22)), Some(true)),
        "the mirror's report is kept for when it wins the chain",
    );
    assert!(
        drain_events(&mut rx_a).is_empty() && drain_events(&mut rx_b).is_empty(),
        "presentation reports are query-answering state: nothing is pushed to any window",
    );
}

/// The palette travels inside the theme report: the owner's palette
/// answers whole, and a slot it leaves out is not filled from a
/// mirror's report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_palette_resolves_with_the_owners_report_and_never_stitches() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);
    report_theme_and_palette(&mut task, a, None, &[(1, (0x11, 0x00, 0x00))]);
    report_theme_and_palette(
        &mut task,
        b,
        None,
        &[(1, (0x22, 0x00, 0x00)), (2, (0x00, 0x22, 0x00))],
    );
    take_input_ownership(&mut task, a);
    assert_eq!(effective_palette(&task, 1), Some((0x11, 0x00, 0x00)));
    assert_eq!(effective_palette(&task, 2), None, "no slot from the mirror");

    take_input_ownership(&mut task, b);
    assert_eq!(effective_palette(&task, 1), Some((0x22, 0x00, 0x00)));
    assert_eq!(effective_palette(&task, 2), Some((0x00, 0x22, 0x00)));

    task.remove_sub(a);
    task.remove_sub(b);
    assert_eq!(
        effective_palette(&task, 1),
        Some((0x22, 0x00, 0x00)),
        "the last resolved palette outlives the last window",
    );
    assert_eq!(effective_palette(&task, 2), Some((0x00, 0x22, 0x00)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ownership_transfer_flips_both_facets_with_no_new_report() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);
    report_theme(&mut task, a, Some((0x11, 0x11, 0x11)));
    report_scheme(&mut task, a, false);
    report_theme(&mut task, b, Some((0x22, 0x22, 0x22)));
    report_scheme(&mut task, b, true);
    take_input_ownership(&mut task, a);
    assert_eq!(effective_bg(&task), Some((0x11, 0x11, 0x11)));
    assert!(!effective_dark(&task));

    take_input_ownership(&mut task, b);

    assert_eq!(effective_bg(&task), Some((0x22, 0x22, 0x22)));
    assert!(effective_dark(&task));
}

/// The two facets resolve apart: the trio and the OS scheme may come
/// from different windows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_trio_and_the_os_scheme_can_come_from_different_windows() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);
    take_input_ownership(&mut task, a);
    report_scheme(&mut task, a, true);
    report_theme(&mut task, b, Some((0x22, 0x22, 0x22)));

    assert!(effective_dark(&task), "the owner supplies the scheme");
    assert_eq!(
        effective_bg(&task),
        Some((0x22, 0x22, 0x22)),
        "the only window that reported a trio supplies it",
    );
}

/// An `Ops` attach's reports never enter the chain, even as the sole
/// subscriber or size owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scripted_subscriber_is_never_a_presentation_candidate() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (ops, _rx_ops) = attach_ops_sub(&mut task);
    report_theme(&mut task, ops, Some((0x22, 0x22, 0x22)));
    report_scheme(&mut task, ops, true);

    assert_eq!(effective_bg(&task), None, "no window has reported");
    assert!(!effective_dark(&task));

    take_input_ownership(&mut task, ops);
    assert_eq!(effective_bg(&task), None);
    assert!(!effective_dark(&task));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_detach_falls_back_to_the_newest_remaining_window() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);
    let (c, _rx_c) = attach_sub(&mut task, false);
    report_theme(&mut task, b, Some((0x22, 0x22, 0x22)));
    report_scheme(&mut task, b, false);
    report_theme(&mut task, c, Some((0x33, 0x33, 0x33)));
    report_scheme(&mut task, c, true);
    take_input_ownership(&mut task, a);
    report_theme(&mut task, a, Some((0x11, 0x11, 0x11)));
    assert_eq!(effective_bg(&task), Some((0x11, 0x11, 0x11)));

    task.remove_sub(a);

    assert_eq!(
        effective_bg(&task),
        Some((0x33, 0x33, 0x33)),
        "the newest surviving window, not the oldest",
    );
    assert!(effective_dark(&task), "and its scheme with it");
}

/// The mode-2031 notification tracks the effective scheme, not each
/// report, and a transfer writes scheme report, then resize, then
/// keystroke, in that order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_2031_reports_effective_scheme_changes_before_the_keystroke() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    {
        let mut core = task.session.lock_core();
        let mut parser = felis_vt::Parser::new();
        parser.advance(&mut core.grid, b"\x1b[?2031h");
        assert!(core.grid.color_scheme_notify(), "the program opted in");
    }
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);

    press_key(&mut task, a, b"A");
    report_scheme(&mut task, a, false);
    report_scheme(&mut task, b, true);
    let end = task.handle_input(
        b,
        InputMsg::Resize {
            dims: RequestedDims {
                rows: 30,
                cols: 120,
                pixel_w: 0,
                pixel_h: 0,
            },
        },
    );
    assert!(end.is_none(), "resize is a continuing input");
    assert_eq!(
        task.session.lock_core().grid.rows(),
        crate::DEFAULT_ROWS,
        "a non-active mirror's resize is stored, not applied",
    );
    assert!(
        task.pty_steps.is_empty(),
        "nothing PTY-visible has happened yet: {:?}",
        task.pty_steps,
    );

    press_key(&mut task, b, b"B");

    assert_eq!(
        task.pty_steps,
        [PtyStep::SchemeReport, PtyStep::Resize],
        "the transfer's scheme report must precede the resize it also triggers",
    );
    assert_eq!(task.session.lock_core().grid.rows(), 30);

    report_scheme(&mut task, b, true);
    let (_c, _rx_c) = attach_sub(&mut task, false);
    press_key(&mut task, b, b"Z");

    let seen = capture_through(&path, b"Z").await;
    let report = find_bytes(&seen, b"\x1b[?997;1n").expect("the transfer notified");
    let keystroke = find_bytes(&seen, b"B").expect("the keystroke reached the PTY");
    assert!(
        report < keystroke,
        "the new owner's scheme must land before the bytes of the event that transferred it: {seen:?}",
    );
    assert_eq!(
        count_bytes(&seen, b"\x1b[?997;1n"),
        1,
        "a same-value re-report and an attach notify nobody: {seen:?}",
    );
    assert_eq!(
        count_bytes(&seen, b"\x1b[?997;2n"),
        0,
        "nothing moved the answer back to light: {seen:?}",
    );
}

/// A detach that moves the effective scheme owes a report, written
/// by the run loop's reconcile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_detach_that_moves_the_effective_scheme_notifies_mode_2031() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    {
        let mut core = task.session.lock_core();
        let mut parser = felis_vt::Parser::new();
        parser.advance(&mut core.grid, b"\x1b[?2031h");
    }
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);
    report_scheme(&mut task, a, false);
    press_key(&mut task, b, b"B");
    report_scheme(&mut task, b, true);
    assert!(effective_dark(&task));

    task.remove_sub(b);

    assert!(!effective_dark(&task), "the survivor's light report wins");
    assert!(
        task.scheme_dirty,
        "the sync detach path leaves the reconcile to the run loop",
    );
    let end = task.sync_color_scheme_to_pty();
    assert!(end.is_none());
    let seen = capture_through(&path, b"\x1b[?997;2n").await;
    assert_eq!(
        count_bytes(&seen, b"\x1b[?997;2n"),
        1,
        "one report per effective change: {seen:?}",
    );

    task.remove_sub(a);
    let end = task.sync_color_scheme_to_pty();
    assert!(end.is_none());
    assert_eq!(
        task.pty_steps,
        [PtyStep::SchemeReport, PtyStep::SchemeReport],
        "the last window leaving keeps the light the program was already told: {:?}",
        task.pty_steps,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_that_returns_to_the_reported_scheme_notifies_nobody() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    {
        let mut core = task.session.lock_core();
        let mut parser = felis_vt::Parser::new();
        parser.advance(&mut core.grid, b"\x1b[?2031h");
    }
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);
    let (c, _rx_c) = attach_sub(&mut task, false);
    take_input_ownership(&mut task, c);
    report_scheme(&mut task, c, false);
    report_scheme(&mut task, a, false);
    report_scheme(&mut task, b, true);
    assert!(!effective_dark(&task), "the owner answers light");
    assert!(task.pty_steps.is_empty(), "light was already the answer");

    task.remove_sub(c);
    assert!(effective_dark(&task), "the latest attach answers dark");
    task.remove_sub(b);

    let end = task.sync_color_scheme_to_pty();
    assert!(end.is_none());
    assert!(
        task.pty_steps.is_empty(),
        "the program was never told dark, so it is owed no return to light: {:?}",
        task.pty_steps,
    );
}

/// No daemon-initiated report (`DECSET 2031` scheme, `DECSET 1004`
/// focus) reaches a PTY at EOF. Both the silence and the session
/// staying alive are asserted: on the BSDs the write would fail
/// with `EIO` and the next one tear the session down; on Linux only
/// the silence is observable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_report_after_pty_eof_never_reaches_the_dead_pty() {
    let mut task = SessionTask::for_tests(session_with("exit 0"));
    {
        let mut core = task.session.lock_core();
        let mut parser = felis_vt::Parser::new();
        parser.advance(&mut core.grid, b"\x1b[?2031h\x1b[?1004h");
    }
    tokio::time::timeout(Duration::from_secs(10), task.session.eof.eof())
        .await
        .expect("the shell exited within the deadline")
        .expect("the end of output is not an error");
    task.pty_eof = true;
    task.evict_all(None);

    let (win, _rx) = attach_sub(&mut task, false);
    // Looped over a sleep: the writer thread fails asynchronously,
    // and it is the write after the failed one that reports dead.
    for _ in 0..5 {
        for on in [true, false] {
            assert!(
                task.handle_input(win, InputMsg::ColorScheme { dark: on })
                    .is_none(),
                "a presentation report must not end a session whose PTY is at EOF",
            );
            assert!(
                task.handle_input(win, InputMsg::FocusChange { focused: on })
                    .is_none(),
                "a focus report must not end a session whose PTY is at EOF",
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task.subs[0].focused = true;
    task.focus_dirty = true;
    assert!(task.sync_focus_to_pty().is_none());
    assert!(
        task.pty_steps.is_empty(),
        "no daemon-initiated report reaches a PTY at EOF: {:?}",
        task.pty_steps,
    );
}

/// Session focus is the OR over window subscribers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_window_blur_does_not_unfocus_the_session() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);

    let end = task.handle_input(a, InputMsg::FocusChange { focused: true });
    assert!(end.is_none());
    assert!(task.reported_focus, "first focus is a session edge");

    let end = task.handle_input(b, InputMsg::FocusChange { focused: false });
    assert!(end.is_none());
    assert!(
        task.reported_focus,
        "a background blur must not un-focus the session",
    );

    task.remove_sub(a);
    assert!(task.focus_dirty);
    let end = task.sync_focus_to_pty();
    assert!(end.is_none());
    assert!(!task.reported_focus, "the focused window left");
}

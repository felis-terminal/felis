use super::*;

/// `InputMsg::Resize` resizes the daemon-side grid and the shared
/// meta, not just the PTY.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_resize_resizes_pty_grid_and_meta() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    assert_eq!(task.session.lock_core().grid.rows(), crate::DEFAULT_ROWS);
    assert_eq!(task.session.lock_core().grid.cols(), crate::DEFAULT_COLS);
    let (sub, _rx) = attach_sub(&mut task, false);

    let end = task.handle_input(
        sub,
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
    assert_eq!(task.session.lock_core().grid.rows(), 30);
    assert_eq!(task.session.lock_core().grid.cols(), 120);
    let meta = task.meta.lock().unwrap().clone();
    assert_eq!((meta.rows, meta.cols), (30, 120));
}

/// A geometry change under `DECSET 2048` writes exactly one
/// `CSI 48 ; rows ; cols ; h ; w t` to the PTY; the same resize
/// with the mode unset writes nothing, and a same-dims one repeats
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_2048_reports_a_resize_only_while_the_program_opted_in() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    let (sub, _rx) = attach_sub(&mut task, false);

    assert!(resize_to(&mut task, sub, 30, 120).is_none());
    assert!(task.sync_resize_notify_to_pty().is_none());
    assert!(
        !task.pty_steps.contains(&PtyStep::ResizeReport),
        "the program never opted in, so the resize owes no report: {:?}",
        task.pty_steps,
    );

    set_mode_2048(&task, true);
    assert!(task.sync_resize_notify_to_pty().is_none());
    assert!(resize_to(&mut task, sub, 31, 121).is_none());
    assert!(task.resize_notify_dirty, "the run loop owes the report");
    assert!(task.sync_resize_notify_to_pty().is_none());
    assert!(resize_to(&mut task, sub, 31, 121).is_none());
    assert!(task.sync_resize_notify_to_pty().is_none());

    let sentinel = b"\x1b[48;31;121;496;968t";
    let seen = capture_through(&path, sentinel).await;
    assert_eq!(
        count_bytes(&seen, b"\x1b[48;30;120;480;960t"),
        1,
        "the set answers the geometry it landed on: {seen:?}",
    );
    assert_eq!(
        count_bytes(&seen, sentinel),
        1,
        "one report per geometry change, and none for a same-dims resize: {seen:?}",
    );
}

/// A program converting SGR-pixel reports back to cells divides by
/// the reported extents, so a font change that keeps the cell count
/// still owes a report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_2048_reports_a_change_of_the_pixel_extents_alone() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    let (sub, _rx) = attach_sub(&mut task, false);

    assert!(resize_to(&mut task, sub, 30, 120).is_none());
    set_mode_2048(&task, true);
    assert!(task.sync_resize_notify_to_pty().is_none());
    assert!(resize_px(&mut task, sub, (30, 120), (1200, 600)).is_none());
    assert!(task.resize_notify_dirty, "the run loop owes the report");
    assert!(task.sync_resize_notify_to_pty().is_none());

    let sentinel = b"\x1b[48;30;120;600;1200t";
    let seen = capture_through(&path, sentinel).await;
    assert_eq!(
        count_bytes(&seen, b"\x1b[48;30;120;480;960t"),
        1,
        "the set answers the nominal extents of a session no window sized: {seen:?}",
    );
    assert_eq!(count_bytes(&seen, sentinel), 1, "{seen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_without_pixels_keeps_the_cell_a_window_supplied() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, _rx) = attach_sub(&mut task, false);
    assert!(resize_px(&mut task, sub, (30, 120), (1200, 600)).is_none());
    assert!(resize_to(&mut task, sub, 40, 100).is_none());
    assert_eq!(pty_pixels(&task), (1000, 800));
}

/// A supplied width under one pixel per column floors the cell to
/// `0`, which a later resize without pixels must not multiply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_without_pixels_after_a_sub_pixel_cell_fills_the_nominal_cell() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, _rx) = attach_sub(&mut task, false);
    assert!(resize_px(&mut task, sub, (30, 80), (1, 600)).is_none());
    assert!(resize_to(&mut task, sub, 30, 80).is_none());
    assert_eq!(pty_pixels(&task), (80 * crate::NOMINAL_CELL_PIXELS.0, 600));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retained_cell_fills_an_unknown_axis_no_further_than_the_pixel_bound() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, _rx) = attach_sub(&mut task, false);
    assert!(resize_px(&mut task, sub, (30, 100), (32_000, 600)).is_none());
    assert!(resize_to(&mut task, sub, 30, 2048).is_none());
    assert_eq!(
        pty_pixels(&task).0,
        felis_protocol::messages::MAX_GRID_PIXELS
    );
}

/// The new owner supplied no pixels, which fill to the previous owner's
/// extents: the PTY stays put while the roster states what the owner
/// supplied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn size_ownership_moves_the_supplied_pixels_when_the_filled_ones_match() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, mut rx_b) = attach_sub(&mut task, false);
    press_key(&mut task, a, b"A");
    assert!(resize_px(&mut task, a, (30, 120), (960, 480)).is_none());
    assert!(resize_to(&mut task, b, 30, 120).is_none());
    drain_events(&mut rx_b);
    task.pty_steps.clear();

    press_key(&mut task, b, b"B");
    assert_eq!(pty_pixels(&task), (960, 480));
    assert!(
        !task.pty_steps.contains(&PtyStep::Resize),
        "{:?}",
        task.pty_steps
    );
    let meta = task.meta.lock().unwrap().clone();
    assert_eq!((meta.pixel_w, meta.pixel_h), (0, 0));
    assert!(drain_events(&mut rx_b).iter().any(|ev| matches!(
        ev,
        OutEvent::Grid(GridMsg::Size {
            dims: GridDims {
                pixel_w: 0,
                pixel_h: 0,
                ..
            }
        })
    )));
}

/// Two windows with one cell count but different fonts: the PTY,
/// and the report, follow the window that takes the size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn size_ownership_moves_the_pixel_extents_at_an_unchanged_cell_count() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);
    press_key(&mut task, a, b"A");
    assert!(resize_px(&mut task, a, (30, 120), (960, 480)).is_none());
    assert!(resize_px(&mut task, b, (30, 120), (1200, 600)).is_none());
    assert_eq!(pty_pixels(&task), (960, 480), "b does not own the size yet");

    press_key(&mut task, b, b"B");
    assert_eq!(pty_pixels(&task), (1200, 600));
}

/// A `DECSET 2048` that lands on a resize the reconcile has not
/// drained yet answers once, with the geometry that is live when
/// the report is written, never the pre-set geometry after the
/// post-set one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_2048_collapses_a_pending_resize_and_the_set_into_one_report() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    let (sub, _rx) = attach_sub(&mut task, false);

    assert!(resize_to(&mut task, sub, 30, 120).is_none());
    assert!(task.resize_notify_dirty, "the resize left the edge");
    set_mode_2048(&task, true);
    assert!(resize_to(&mut task, sub, 31, 121).is_none());

    assert!(task.sync_resize_notify_to_pty().is_none());
    assert!(task.sync_resize_notify_to_pty().is_none());
    assert_eq!(
        task.pty_steps
            .iter()
            .filter(|step| **step == PtyStep::ResizeReport)
            .count(),
        1,
        "one report for one geometry: {:?}",
        task.pty_steps,
    );

    let sentinel = b"\x1b[48;31;121;496;968t";
    let seen = capture_through(&path, sentinel).await;
    assert_eq!(
        count_bytes(&seen, b"\x1b[48;30;120;480;960t"),
        0,
        "the geometry the set left behind is never reported: {seen:?}",
    );
}

/// A `DECRST 2048` coalesced with a re-`DECSET` in one parse burst
/// still answers the fresh set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_2048_answers_a_set_that_follows_a_coalesced_reset() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    let (sub, _rx) = attach_sub(&mut task, false);

    assert!(resize_to(&mut task, sub, 30, 120).is_none());
    set_mode_2048(&task, true);
    assert!(task.sync_resize_notify_to_pty().is_none());

    set_mode_2048(&task, false);
    set_mode_2048(&task, true);
    assert!(task.sync_resize_notify_to_pty().is_none());
    press_key(&mut task, sub, b"Z");

    let seen = capture_through(&path, b"Z").await;
    assert_eq!(
        count_bytes(&seen, b"\x1b[48;30;120;480;960t"),
        2,
        "each set answers, even at an unchanged geometry: {seen:?}",
    );
}

/// The shell's repaint after SIGWINCH must meet the new grid, so the
/// PTY resizes only once the grid has reflowed to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pty_resizes_after_the_grid_takes_the_new_geometry() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, _rx) = attach_sub(&mut task, false);
    assert!(resize_to(&mut task, sub, 30, 120).is_none());
    write_to_grid(&task, b"\x1b[?1049h");
    assert!(resize_to(&mut task, sub, 20, 60).is_none());
    assert_eq!(task.pty_resized_over, [(30, 120), (20, 60)]);
}

/// A resize triggered by size-ownership transfer reports before the
/// bytes of the input that transferred it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_2048_reports_a_transfer_before_the_triggering_keystroke() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    set_mode_2048(&task, true);
    let (a, _rx_a) = attach_sub(&mut task, false);
    let (b, _rx_b) = attach_sub(&mut task, false);

    press_key(&mut task, a, b"A");
    assert!(resize_to(&mut task, b, 30, 120).is_none());
    task.pty_steps.clear();

    press_key(&mut task, b, b"B");

    assert_eq!(
        task.pty_steps,
        [PtyStep::Resize, PtyStep::ResizeReport],
        "the transfer's resize report must precede the keystroke that triggered it",
    );
    let seen = capture_through(&path, b"B").await;
    let report = find_bytes(&seen, b"\x1b[48;30;120;480;960t").expect("the transfer notified");
    let keystroke = find_bytes(&seen, b"B").expect("the keystroke reached the PTY");
    assert!(
        report < keystroke,
        "the new geometry must land before the bytes of the event that transferred it: {seen:?}",
    );
}

/// The production trigger, with no helper standing in for the
/// wiring: a `DECSET 2048` written by the child as PTY output is
/// parsed by the real sink, drained by the run loop's own
/// `drain_effects`, and answered exactly once by the reconcile at
/// the bottom of that loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_2048_set_over_the_pty_is_answered_by_the_run_loop() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let task = SessionTask::for_tests(capture_session(&path, "\\033[?2048h", false));
    let (rows, cols) = {
        let core = task.session.lock_core();
        (core.grid.rows(), core.grid.cols())
    };
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let loop_done = tokio::spawn(run_session(task, cmd_rx));

    let report = format!(
        "\x1b[48;{rows};{cols};{};{}t",
        rows * crate::NOMINAL_CELL_PIXELS.1,
        cols * crate::NOMINAL_CELL_PIXELS.0
    );
    let seen = capture_through(&path, report.as_bytes()).await;
    assert_eq!(
        count_bytes(&seen, report.as_bytes()),
        1,
        "the set owes one report, and the loop's repeated reconciles owe no more: {seen:?}",
    );

    drop(cmd_tx);
    loop_done
        .await
        .expect("the run loop ends on a dropped pool");
}

/// Active-owns-winsize: a non-owner's resize is not applied; the
/// requester gets a `Size` correction plus a full row replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_active_mirror_resize_is_corrected_not_applied() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, mut rx_a) = attach_sub(&mut task, false);
    let (sub_b, mut rx_b) = attach_sub(&mut task, false);

    task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
    assert_eq!(task.active_sub, Some(sub_a));
    drain_events(&mut rx_a);

    task.handle_input(
        sub_b,
        InputMsg::Resize {
            dims: RequestedDims {
                rows: 50,
                cols: 200,
                pixel_w: 0,
                pixel_h: 0,
            },
        },
    );
    assert_eq!(
        {
            let core = task.session.lock_core();
            (core.grid.rows(), core.grid.cols())
        },
        (crate::DEFAULT_ROWS, crate::DEFAULT_COLS),
        "non-active resize must not change the authoritative grid",
    );
    let b_events = drain_events(&mut rx_b);
    assert!(
        b_events.iter().any(|ev| matches!(
            ev,
            OutEvent::Grid(GridMsg::Size {
                dims: GridDims {
                    rows: crate::DEFAULT_ROWS,
                    cols: crate::DEFAULT_COLS,
                    ..
                },
            })
        )),
        "the overruled requester must receive the authoritative dims",
    );
    let b_rows = b_events
        .iter()
        .filter_map(|ev| match ev {
            OutEvent::Grid(GridMsg::RowDelta { rows }) => Some(rows.len()),
            _ => None,
        })
        .sum::<usize>();
    assert_eq!(
        b_rows,
        usize::from(crate::DEFAULT_ROWS),
        "the correction must replay every row at the authoritative width",
    );
    assert!(
        drain_events(&mut rx_a).is_empty(),
        "the active client must not be disturbed by an ignored resize",
    );
}

/// Active-owns-winsize: typing transfers size ownership, the PTY
/// reflows to the new owner's last requested size, and every mirror
/// hears the new dims.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typing_transfers_size_ownership_and_reflows() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, mut rx_a) = attach_sub(&mut task, false);
    let (sub_b, mut rx_b) = attach_sub(&mut task, false);

    task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
    task.handle_input(
        sub_b,
        InputMsg::Resize {
            dims: RequestedDims {
                rows: 30,
                cols: 120,
                pixel_w: 0,
                pixel_h: 0,
            },
        },
    );
    assert_eq!(task.session.lock_core().grid.cols(), crate::DEFAULT_COLS);
    drain_events(&mut rx_a);
    drain_events(&mut rx_b);

    task.handle_input(sub_b, InputMsg::KeyBytes(b"b".to_vec()));
    assert_eq!(task.active_sub, Some(sub_b));
    assert_eq!(
        {
            let core = task.session.lock_core();
            (core.grid.rows(), core.grid.cols())
        },
        (30, 120),
        "the new owner's last requested size must apply on transfer",
    );
    for rx in [&mut rx_a, &mut rx_b] {
        assert!(
            drain_events(rx).iter().any(|ev| matches!(
                ev,
                OutEvent::Grid(GridMsg::Size {
                    dims: GridDims {
                        rows: 30,
                        cols: 120,
                        ..
                    }
                })
            )),
            "every mirror must hear the new authoritative dims",
        );
    }
}

/// An authoritative resize must answer every armed pull, not only
/// the requester's: nothing else would, and the mirror would paint
/// rows composed at dimensions the daemon has overruled. It owes
/// each of them one replay, so the resize consumes its own damage
/// rather than leaving the caller's fan-out to re-arm a second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_answers_an_idle_mirrors_armed_pull_exactly_once() {
    const ROWS: u16 = 30;

    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub_a, mut rx_a) = attach_sub(&mut task, false);
    let (sub_b, mut rx_b) = attach_sub(&mut task, true);

    task.handle_input(sub_a, InputMsg::KeyBytes(b"a".to_vec()));
    // The echo has to be on the grid and shipped before the resize,
    // or it rides the replay and the row counts below measure it.
    wait_for_grid_text(&task.session.core, "a").await;
    task.after_drive();
    drain_events(&mut rx_a);
    // One pull hands the mirror that echo, the next finds it clean
    // and stays armed, which is the state the resize has to answer.
    task.handle_input(sub_b, InputMsg::NextGridFrame);
    drain_events(&mut rx_b);
    task.handle_input(sub_b, InputMsg::NextGridFrame);
    assert!(
        drain_events(&mut rx_b).is_empty(),
        "the mirror is caught up, so its pull stays armed",
    );

    task.handle_input(
        sub_a,
        InputMsg::Resize {
            dims: RequestedDims {
                rows: u32::from(ROWS),
                cols: 120,
                pixel_w: 0,
                pixel_h: 0,
            },
        },
    );

    let events = drain_events(&mut rx_b);
    let size_at = events
        .iter()
        .position(|ev| matches!(ev, OutEvent::Grid(GridMsg::Size { .. })))
        .expect("the mirror hears the authoritative dims");
    let rows_at = events
        .iter()
        .position(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. })))
        .expect("and the replay that fills them, with no further activity");
    assert!(size_at < rows_at, "the rows follow the dimensions");
    assert_eq!(
        shipped_rows(&events),
        usize::from(ROWS),
        "one replay of the new grid, not two",
    );
    assert_eq!(
        shipped_rows(&drain_events(&mut rx_a)),
        usize::from(ROWS),
        "the eager requester is owed one replay too",
    );

    task.handle_input(sub_b, InputMsg::NextGridFrame);
    assert!(
        shipped_rows(&drain_events(&mut rx_b)) == 0,
        "the mirror's next clean pull finds nothing left over",
    );
}

fn full_screen_scroll(task: &SessionTask) -> streaming::QueuedScroll {
    let core = task.session.lock_core();
    streaming::QueuedScroll {
        geometry_gen: core.grid.geometry_gen(),
        first_seq: core.grid.scroll_seq() + 1,
        last_seq: core.grid.scroll_seq() + 1,
        op: felis_grid::ScrollOp {
            region_top: 0,
            region_bottom: core.grid.rows() - 1,
            n_rows: 1,
            direction: felis_grid::ScrollDirection::Up,
        },
    }
}

const fn shrunk() -> PtySize {
    PtySize {
        rows: 10,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// A directive queued for a pull-paced subscriber names the geometry
/// it was recorded under, so a resize must retire it: shipping it
/// after the `Size` would name rows the new grid does not have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_retires_a_pending_scroll_directive() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_sub, mut rx) = attach_sub(&mut task, true);
    drain_events(&mut rx);

    task.session.scroll_ops.push(full_screen_scroll(&task));
    task.fan_out_grid_state();
    assert_eq!(
        task.subs[0].stream.pending_scrolls.len(),
        1,
        "a clean subscriber accepts the directive",
    );

    task.apply_size(shrunk());
    assert!(
        task.subs[0].stream.pending_scrolls.is_empty(),
        "the pre-resize directive must not survive the new geometry",
    );
    task.ship_all();
    assert!(
        !drain_events(&mut rx)
            .iter()
            .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Scrolled { .. }))),
        "no directive may follow the Size announcement",
    );
}

/// The same directive drained after the resize instead of before it:
/// its generation is stale, so it downgrades to a row replay rather
/// than shifting rows the reflow already moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scroll_drained_after_a_resize_downgrades_to_a_row_replay() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_sub, mut rx) = attach_sub(&mut task, true);
    drain_events(&mut rx);

    let queued = full_screen_scroll(&task);
    task.apply_size(shrunk());
    task.subs[0].stream.damage.clear();
    task.session.scroll_ops.push(queued);
    task.fan_out_grid_state();

    assert!(
        task.subs[0].stream.pending_scrolls.is_empty(),
        "a directive from the previous geometry is never admitted",
    );
    assert_eq!(
        task.subs[0].stream.damage.dirty_rows().count(),
        10,
        "it downgrades to a replay of every row of the new grid",
    );
}

/// A scroll the parse thread produces between the resize fan-out and
/// the replay is already in the rows that replay carries: shipping
/// the directive as well would shift them a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scroll_raced_with_the_resize_replay_is_not_applied_twice() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_sub, mut rx) = attach_sub(&mut task, false);
    let mut shadow = {
        let core = task.session.lock_core();
        felis_client_core::ShadowScreen::new(core.grid.rows(), core.grid.cols())
    };
    {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser
            .advance(&mut core.grid, b"\x1b[H\x1b[2Jone\r\ntwo\r\nthree");
    }
    task.drain_and_fan().expect("effects drain");
    task.ship_all();
    mirror_apply(&mut shadow, &drain_events(&mut rx));

    // The replay `apply_size` arms, fanned out before it is encoded.
    task.subs[0].stream.damage.mark_all();
    task.fan_out_grid_state();
    {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser.advance(&mut core.grid, b"\x1b[S");
    }
    task.drain_effects().expect("effects drain");
    task.ship_all();
    task.fan_out_grid_state();
    assert!(
        task.subs[0].stream.pending_scrolls.is_empty(),
        "a directive the replay already carries must not be admitted",
    );
    task.ship_all();

    let events = drain_events(&mut rx);
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::Scrolled { .. }))),
        "no directive may follow a replay of the shifted rows",
    );
    mirror_apply(&mut shadow, &events);
    assert_eq!(
        mirror_rows(&shadow),
        grid_rows(&task),
        "the mirror must hold exactly the daemon's rows",
    );
}

fn scroll_the_grid(task: &SessionTask) {
    let mut core = task.session.lock_core();
    let core = &mut *core;
    core.parser.advance(&mut core.grid, b"\x1b[S");
}

/// Two compositions under separate guards within one row-cache
/// cycle: a scroll between them must not hand the second mirror rows
/// from the grid the first one read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_mirrors_composed_across_a_scroll_both_hold_the_grids_rows() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_a, mut rx_a) = attach_sub(&mut task, false);
    let (_b, mut rx_b) = attach_sub(&mut task, false);
    let mut shadow_a = new_mirror(&task);
    let mut shadow_b = new_mirror(&task);

    write_to_grid(&task, b"\x1b[H\x1b[2Jone\r\ntwo\r\nthree");
    task.drain_and_fan().expect("effects drain");
    task.ship_all();
    mirror_apply(&mut shadow_a, &drain_events(&mut rx_a));
    mirror_apply(&mut shadow_b, &drain_events(&mut rx_b));

    write_to_grid(&task, b"\r\nfour");
    task.drain_effects().expect("effects drain");
    task.fan_out_grid_state();
    // One ship cycle, with the parse thread scrolling in the middle.
    task.row_cache.begin_cycle(task.subs.len());
    task.ship_sub_at(1);
    let mid = drain_events(&mut rx_b);
    assert!(
        shipped_rows(&mid) > 0,
        "the first mirror must fill the cache this cycle",
    );
    mirror_apply(&mut shadow_b, &mid);
    scroll_the_grid(&task);
    task.ship_sub_at(0);
    task.drain_effects().expect("effects drain");
    task.fan_out_grid_state();
    task.ship_all();

    mirror_apply(&mut shadow_a, &drain_events(&mut rx_a));
    mirror_apply(&mut shadow_b, &drain_events(&mut rx_b));
    let grid = grid_rows(&task);
    assert_eq!(mirror_rows(&shadow_a), grid, "the first mirror");
    assert_eq!(mirror_rows(&shadow_b), grid, "the second mirror");
}

/// A scroll between the fan-out and the composition leaves the
/// partial replay carrying post-scroll cells; the directive drained
/// afterwards would shift them a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scroll_racing_a_partial_replay_is_not_applied_twice() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_sub, mut rx) = attach_sub(&mut task, false);
    let mut shadow = new_mirror(&task);

    write_to_grid(&task, b"\x1b[H\x1b[2Jone\r\ntwo\r\nthree");
    task.drain_and_fan().expect("effects drain");
    task.ship_all();
    mirror_apply(&mut shadow, &drain_events(&mut rx));

    // One dirty row inside the region, fanned out before the scroll.
    write_to_grid(&task, b"\x1b[2;1Htwo!");
    task.drain_effects().expect("effects drain");
    task.fan_out_grid_state();
    scroll_the_grid(&task);
    task.ship_all();
    task.drain_effects().expect("effects drain");
    task.fan_out_grid_state();
    task.ship_all();

    mirror_apply(&mut shadow, &drain_events(&mut rx));
    assert_eq!(
        mirror_rows(&shadow),
        grid_rows(&task),
        "the mirror must hold exactly the daemon's rows",
    );
}

/// Both authoritative-dims paths (applied resize, overruled-mirror
/// correction) announce to every subscriber.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_announces_authoritative_dims_to_every_subscriber() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (owner, mut rx_owner) = attach_sub(&mut task, false);
    let (mirror, mut rx_mirror) = attach_sub(&mut task, false);

    task.handle_input(owner, InputMsg::KeyBytes(b"a".to_vec()));
    assert_eq!(task.active_sub, Some(owner));
    drain_events(&mut rx_owner);
    drain_events(&mut rx_mirror);

    task.handle_input(
        owner,
        InputMsg::Resize {
            dims: RequestedDims {
                rows: 30,
                cols: 120,
                pixel_w: 0,
                pixel_h: 0,
            },
        },
    );
    let hears_new_dims = |evs: &[OutEvent]| {
        evs.iter().any(|ev| {
            matches!(
                ev,
                OutEvent::Grid(GridMsg::Size {
                    dims: GridDims {
                        rows: 30,
                        cols: 120,
                        ..
                    }
                })
            )
        })
    };
    assert!(
        hears_new_dims(&drain_events(&mut rx_owner)),
        "the owner must hear the new authoritative dims",
    );
    assert!(
        hears_new_dims(&drain_events(&mut rx_mirror)),
        "a mirror must hear the new authoritative dims too",
    );

    task.handle_input(
        mirror,
        InputMsg::Resize {
            dims: RequestedDims {
                rows: 50,
                cols: 200,
                pixel_w: 0,
                pixel_h: 0,
            },
        },
    );
    assert!(
        hears_new_dims(&drain_events(&mut rx_mirror)),
        "an overruled mirror gets the authoritative dims as a correction",
    );
}

use super::*;

/// Real time, counted in flood chunks: under chunks that hold the
/// lock past `DRAIN_COALESCE`, the run loop still yields between
/// drains, so a pull arriving through the runtime is answered. On
/// this single-threaded runtime the test body's timer fires only
/// when the loop yields.
#[tokio::test(flavor = "current_thread")]
async fn a_pull_sent_mid_flood_is_answered_within_a_few_chunks_real_time() {
    use std::sync::atomic::{AtomicBool, AtomicU32};

    const HOLD: Duration = Duration::from_millis(6);
    const FLOOD_CHUNKS: u32 = 600;
    const MAX_LEAD_CHUNKS: u32 = 5;
    const MAX_LAG_CHUNKS: u32 = 20;
    const HANG_GUARD: Duration = Duration::from_secs(30);
    assert!(HOLD > DRAIN_COALESCE, "each drain must outwait the window");

    let mut task = SessionTask::for_tests(session_with("read -r _"));
    let (sub, mut rx) = attach_sub(&mut task, true);
    let core = Arc::clone(&task.session.core);
    let signals = Arc::clone(&task.session.signals);
    let chunks = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    // Stands in for the parse sink with chunks queued behind the
    // one in hand: it retakes the lock the moment the task drops
    // it, and the edge for the next chunk is already raised when
    // the lock changes hands, so every drain waits out a whole
    // chunk and finds more to drain behind it.
    let flood = std::thread::spawn({
        let chunks = Arc::clone(&chunks);
        let stop = Arc::clone(&stop);
        move || {
            let mut lead = 0;
            while !stop.load(Ordering::Relaxed) && chunks.load(Ordering::Relaxed) < FLOOD_CHUNKS {
                if !signals.is_dirty() {
                    lead = 0;
                }
                // A task the host does not schedule must not run up the lag.
                if lead >= MAX_LEAD_CHUNKS {
                    std::thread::yield_now();
                    continue;
                }
                let n = chunks.load(Ordering::Relaxed);
                {
                    let mut guard = loop {
                        if let Some(guard) = core.try_lock() {
                            break guard;
                        }
                        std::hint::spin_loop();
                    };
                    let core = &mut *guard;
                    core.parser
                        .advance(&mut core.grid, format!("\r{n}").as_bytes());
                    std::thread::sleep(HOLD);
                    signals.mark_dirty();
                }
                lead += 1;
                chunks.store(n.wrapping_add(1), Ordering::Relaxed);
            }
            stop.store(true, Ordering::Relaxed);
        }
    });

    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
    let loop_done = tokio::spawn(run_session(task, cmd_rx));
    let mut lags = Vec::new();
    for _ in 0..3 {
        let before = chunks.load(Ordering::Relaxed);
        // About five chunks' worth when the loop yields.
        tokio::time::sleep(HOLD * 5).await;
        cmd_tx
            .send(SessionCmd::Input {
                sub,
                msg: InputMsg::NextGridFrame,
                reservation: None,
            })
            .await
            .map_err(|_| ())
            .expect("the session task accepts the pull");
        // A pull that lands after the flood finds nothing to ship
        // and stays armed, so the cycle it waits for never comes.
        let answered = tokio::time::timeout(HANG_GUARD, async {
            while !matches!(
                rx.recv().await.expect("the outbox stays open"),
                OutEvent::Grid(GridMsg::CycleEnd)
            ) {}
        })
        .await
        .is_ok();
        if !answered {
            lags.push(u32::MAX);
            break;
        }
        lags.push(chunks.load(Ordering::Relaxed).wrapping_sub(before));
    }
    let flood_outlived_the_pulls = !stop.swap(true, Ordering::Relaxed);

    flood.join().expect("the flood thread ends");
    drop(cmd_tx);
    loop_done
        .await
        .expect("the run loop ends on a dropped pool");
    assert!(
        lags.iter().all(|&lag| lag <= MAX_LAG_CHUNKS),
        "a pull waited out {lags:?} flood chunks: the run loop drained back to back without yielding",
    );
    assert!(
        flood_outlived_the_pulls,
        "the pulls must be measured under the flood"
    );
}

/// Demand-driven pacing (docs/explanation/rendering/pipeline.md): a
/// pull with nothing dirty ships nothing and leaves the pull armed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_with_nothing_dirty_emits_no_redundant_frame() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (sub, mut rx) = attach_sub(&mut task, true);
    drain_events(&mut rx);

    let end = task.handle_input(sub, InputMsg::NextGridFrame);
    assert!(end.is_none(), "NextGridFrame is a continuing input");

    assert!(
        task.subs[0].pull_pending,
        "a clean pull stays pending — nothing was emitted to consume it"
    );
    let events = drain_events(&mut rx);
    let grid_frames = events
        .iter()
        .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. })))
        .count();
    assert_eq!(
        grid_frames, 0,
        "a pull with nothing new must not force a redundant frame"
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::CycleEnd))),
        "an empty cycle terminates nothing: there was no cycle"
    );
}

/// A window is sent no prompt marks, so a pull whose cycle would
/// have carried only a mark is an empty cycle: nothing ships, the
/// marker included, and the pull stays armed. An `Ops` pull in the
/// same pass is answered with the mark and closed by the marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mark_only_cycle_leaves_a_window_pull_armed_and_answers_an_ops_pull() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (window, mut window_rx) = attach_sub(&mut task, true);
    let (ops, mut ops_rx) = attach_sub_as(&mut task, ConnectionMode::Ops, true);
    drain_events(&mut window_rx);
    drain_events(&mut ops_rx);

    felis_vt::Parser::new().advance(&mut task.session.lock_core().grid, b"\x1b]133;A\x07");
    task.handle_input(window, InputMsg::NextGridFrame);
    task.handle_input(ops, InputMsg::NextGridFrame);
    task.ship_all();

    let window_events = drain_events(&mut window_rx);
    assert!(
        !window_events.iter().any(|ev| matches!(
            ev,
            OutEvent::Grid(GridMsg::PromptMark { .. } | GridMsg::CycleEnd)
        )),
        "a mark-only cycle is empty for a window: {window_events:?}"
    );
    let i = task.sub_index(window).expect("window still attached");
    assert!(task.subs[i].pull_pending, "the window's pull stays armed");

    let ops_grid: Vec<GridMsg> = drain_events(&mut ops_rx)
        .into_iter()
        .filter_map(|ev| match ev {
            OutEvent::Grid(msg) => Some(msg),
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            ops_grid.as_slice(),
            [GridMsg::PromptMark { .. }, GridMsg::CycleEnd]
        ),
        "the ops pull is answered and terminated: {ops_grid:?}"
    );
}

/// The cycle terminator closes a non-empty pull-paced cycle: one
/// marker, after every message that cycle composed, whether the
/// rows arrived in one `RowDelta` frame or several.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_cycle_ends_with_one_marker_after_its_last_frame() {
    let mut task = SessionTask::for_tests(session_with("printf 'cycle-end-probe'; read _x"));
    let (sub, mut rx) = attach_sub(&mut task, true);
    wait_for_grid_text(&task.session.core, "cycle-end-probe").await;
    drain_events(&mut rx);

    task.handle_input(sub, InputMsg::NextGridFrame);

    let grid: Vec<GridMsg> = drain_events(&mut rx)
        .into_iter()
        .filter_map(|ev| match ev {
            OutEvent::Grid(msg) => Some(msg),
            _ => None,
        })
        .collect();
    assert!(
        grid.iter().any(|m| matches!(m, GridMsg::RowDelta { .. })),
        "the probe must produce a cycle to terminate: {grid:?}"
    );
    assert_eq!(
        grid.iter()
            .filter(|m| matches!(m, GridMsg::CycleEnd))
            .count(),
        1,
        "exactly one marker per cycle: {grid:?}"
    );
    assert_eq!(
        grid.last(),
        Some(&GridMsg::CycleEnd),
        "the marker follows every message of its cycle: {grid:?}"
    );
}

/// A cycle whose rows outrun the frame budget splits across
/// consecutive `RowDelta` frames (REQ-105); the marker closes the
/// cycle, not each frame, so it lands once, after the last split.
/// It drives the two halves `ship_sub_at` composes, since no
/// admitted geometry encodes to the budget.
#[test]
fn the_marker_follows_the_last_frame_of_a_split_cycle() {
    let oversized = streaming::OUTBOUND_BODY_BUDGET_BYTES / 2 + 1;
    let mut out = Vec::new();
    streaming::push_row_deltas(
        (0..4)
            .map(|i| (i, felis_protocol::RowPayload(vec![0u8; oversized])))
            .collect(),
        &mut out,
    );
    push_cycle_end(&mut out, true);

    let frames = out
        .iter()
        .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::RowDelta { .. })))
        .count();
    assert!(frames >= 2, "the batch must have split: {frames} frame(s)");
    assert_eq!(
        out.iter()
            .filter(|ev| matches!(ev, OutEvent::Grid(GridMsg::CycleEnd)))
            .count(),
        1,
        "one marker per cycle, not per frame"
    );
    assert!(
        matches!(out.last(), Some(OutEvent::Grid(GridMsg::CycleEnd))),
        "the marker follows every split frame"
    );
}

/// An eager-push peer never pulls, so it has no cycle to close and
/// a marker would be traffic it cannot act on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_eager_push_peer_gets_no_cycle_marker() {
    let mut task = SessionTask::for_tests(session_with("printf 'eager-probe'; read _x"));
    let (_sub, mut rx) = attach_sub(&mut task, false);
    wait_for_grid_text(&task.session.core, "eager-probe").await;
    task.ship_all();

    assert!(
        !drain_events(&mut rx)
            .iter()
            .any(|ev| matches!(ev, OutEvent::Grid(GridMsg::CycleEnd))),
        "no pull gate, no cycle"
    );
}

/// A row edited in place (no scroll, no resize) after a drain cycle
/// cached its encoding must reach a mirror whose pull the command
/// arm answers, not the row as the earlier cycle encoded it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_after_an_in_place_edit_ships_the_edited_row_to_the_second_mirror() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (_a, mut rx_a) = attach_sub(&mut task, true);
    let (b, mut rx_b) = attach_sub(&mut task, true);
    let mut shadow_b = new_mirror(&task);

    write_to_grid(&task, b"\x1b[H\x1b[2Jone");
    for sub in &mut task.subs {
        sub.pull_pending = true;
    }
    task.drain_and_fan().expect("effects drain");
    drain_events(&mut rx_a);
    mirror_apply(&mut shadow_b, &drain_events(&mut rx_b));
    assert_eq!(mirror_rows(&shadow_b)[0], "one");

    write_to_grid(&task, b"\x1b[Htwo");
    task.handle_input(b, InputMsg::NextGridFrame);

    mirror_apply(&mut shadow_b, &drain_events(&mut rx_b));
    assert_eq!(
        mirror_rows(&shadow_b),
        grid_rows(&task),
        "the second mirror must hold the edited row",
    );
}

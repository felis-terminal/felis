//! The Kitty graphics decode the session task runs off its own thread
//! while the parse waits (docs/explanation/protocols/kitty-graphics.md
//! "Dispatcher architecture").

use felis_grid::images::ImageId;

use super::*;

fn base64(data: &[u8]) -> String {
    String::from_utf8(crate::graphics::b64(data)).expect("base64 is ASCII")
}

fn png(width: u32, height: u32, seed: u8) -> Vec<u8> {
    let pixels: Vec<u8> = (0..width * height * 4)
        .map(|i| (i as u8).wrapping_mul(seed | 1))
        .collect();
    crate::graphics::encode_png(width, height, png::ColorType::Rgba, &pixels)
}

/// Holds a decode on the blocking pool until the test lets it
/// go, or drops its handle, so a failed assertion cannot park it.
#[derive(Default)]
struct DecodeGate {
    /// Whether the decode has reached the gate, and whether it may pass.
    state: parking_lot::Mutex<(bool, bool)>,
    changed: parking_lot::Condvar,
}

impl DecodeGate {
    fn hold(&self) {
        let mut state = self.state.lock();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            self.changed.wait(&mut state);
        }
    }

    fn reached(&self, timeout: Duration) -> bool {
        let mut state = self.state.lock();
        let deadline = Instant::now() + timeout;
        while !state.0 {
            if self.changed.wait_until(&mut state, deadline).timed_out() {
                return state.0;
            }
        }
        true
    }

    fn release(&self) {
        self.state.lock().1 = true;
        self.changed.notify_all();
    }
}

/// The test's end of a [`DecodeGate`]: the first [`meet`] waits for the
/// decode to reach it, the second lets the decode go, and a drop lets it
/// go too.
struct Gate {
    gate: Arc<DecodeGate>,
    meets: std::sync::atomic::AtomicU8,
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.gate.release();
    }
}

async fn meet(gate: &Gate) {
    if gate.meets.fetch_add(1, Ordering::Relaxed) % 2 == 1 {
        gate.gate.release();
        return;
    }
    let held = Arc::clone(&gate.gate);
    let reached = tokio::task::spawn_blocking(move || held.reached(Duration::from_secs(10)))
        .await
        .expect("the gate thread ends");
    assert!(reached, "the decode reaches the gate");
}

async fn send(cmd_tx: &mpsc::Sender<SessionCmd>, cmd: SessionCmd) {
    cmd_tx
        .send(cmd)
        .await
        .map_err(|_| ())
        .expect("the session task takes the command");
}

fn paste(sub: SubscriberId, bytes: &[u8]) -> SessionCmd {
    SessionCmd::Input {
        sub,
        msg: InputMsg::Paste(bytes.to_vec()),
        reservation: None,
    }
}

/// A started run loop over a session whose next decode waits at the
/// returned gate, with one pull-paced window attached.
struct Running {
    core: Arc<parking_lot::Mutex<ParseCore>>,
    quiescer: felis_pty::Quiescer,
    gate: Gate,
    shutdown: Arc<crate::pool::ShutdownSignal>,
    sub: SubscriberId,
    rx: mpsc::UnboundedReceiver<OutEvent>,
    cmd_tx: mpsc::Sender<SessionCmd>,
    done: tokio::task::JoinHandle<()>,
}

async fn start(session: Session) -> Running {
    start_with(session, |_| ()).await
}

async fn start_with(session: Session, setup: impl FnOnce(&mut SessionTask)) -> Running {
    let mut task = SessionTask::for_tests(session);
    setup(&mut task);
    let gate = Arc::new(DecodeGate::default());
    let held = Arc::clone(&gate);
    task.before_decode = Some(Box::new(move || held.hold()));
    let gate = Gate {
        gate,
        meets: std::sync::atomic::AtomicU8::new(0),
    };
    let core = Arc::clone(&task.session.core);
    let shutdown = Arc::clone(&task.shutdown);
    let quiescer = task.session.quiescer.clone();
    wait_for_grid_text(&core, "READY").await;
    let (sub, rx) = attach_sub(&mut task, true);
    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
    let done = tokio::spawn(run_session(task, cmd_rx));
    Running {
        core,
        quiescer,
        gate,
        shutdown,
        sub,
        rx,
        cmd_tx,
        done,
    }
}

impl Running {
    async fn stop(self) {
        drop(self.cmd_tx);
        self.done
            .await
            .expect("the run loop ends on a dropped pool");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn while_an_image_decodes_pulls_and_keystrokes_are_served_and_the_parse_waits() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let query = format!(
        "BEFORE\\033_Ga=q,i=1,f=100;{}\\033\\\\\\033[cAFTER",
        base64(&png(2, 2, 1))
    );
    let mut run = start(capture_session(&path, &query, true)).await;
    send(&run.cmd_tx, paste(run.sub, b"go\n")).await;
    meet(&run.gate).await;
    wait_for_grid_text(&run.core, "BEFORE").await;

    send(&run.cmd_tx, paste(run.sub, b"typed")).await;
    let seen = capture_through(&path, b"typed").await;
    assert!(
        find_bytes(&seen, b"\x1b_G").is_none(),
        "the query is answered only at its commit: {seen:?}"
    );
    send(
        &run.cmd_tx,
        SessionCmd::Input {
            sub: run.sub,
            msg: InputMsg::NextGridFrame,
            reservation: None,
        },
    )
    .await;
    let answered = tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            run.rx.recv().await.expect("the outbox stays open"),
            OutEvent::Grid(GridMsg::CycleEnd)
        ) {}
    })
    .await;
    assert!(answered.is_ok(), "a pull waits out the decode");
    {
        let core = run.core.lock();
        let clusters = core.grid.cluster_table();
        assert!(
            (0..core.grid.rows()).all(|r| !felis_grid::row_text_trim(
                core.grid.row_cells(r).unwrap_or(&[]),
                clusters
            )
            .contains("AFTER")),
            "the parse stays stopped on the image until its commit"
        );
    }

    meet(&run.gate).await;
    let seen = capture_through(&path, b"\x1b[?64;").await;
    let reply = find_bytes(&seen, b"\x1b_Gi=1;OK\x1b\\").expect("the query is answered");
    let da1 = find_bytes(&seen, b"\x1b[?64;").expect("DA1 is answered");
    assert!(
        find_bytes(&seen, b"typed").unwrap() < reply && reply < da1,
        "the replies keep the order of their requests: {seen:?}"
    );
    wait_for_grid_text(&run.core, "AFTER").await;
    run.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_park_waits_out_the_decode_and_the_dump_holds_its_image() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let transmit = format!(
        "\\033_Ga=t,i=1,f=100,q=2;{}\\033\\\\",
        base64(&png(2, 2, 2))
    );
    let run = start(capture_session(&path, &transmit, true)).await;
    send(&run.cmd_tx, paste(run.sub, b"go\n")).await;
    meet(&run.gate).await;

    let (settled, mut settle_rx) = oneshot::channel();
    send(&run.cmd_tx, SessionCmd::Settle(settled)).await;
    stats(&run.cmd_tx).await;
    assert!(
        settle_rx.try_recv().is_err(),
        "the barrier waits for the commit like every command it follows"
    );
    let quiescer = run.quiescer.clone();
    let parked = tokio::task::spawn_blocking(move || quiescer.park(Duration::from_millis(300)))
        .await
        .unwrap();
    assert!(
        parked.is_err(),
        "the parse thread waits on the decode, so nothing parks"
    );

    meet(&run.gate).await;
    settle_rx.await.expect("the session settles");
    let quiescer = run.quiescer.clone();
    tokio::task::spawn_blocking(move || quiescer.park(Duration::from_secs(5)))
        .await
        .unwrap()
        .expect("the session parks once the decode commits");
    let (reply, state) = oneshot::channel();
    send(&run.cmd_tx, SessionCmd::Capture(reply)).await;
    let state = state.await.expect("the session captures");
    assert!(
        state.images.get(ImageId(1)).is_some(),
        "the dump holds the image"
    );
    send(&run.cmd_tx, SessionCmd::Resume).await;
    run.quiescer.resume().unwrap();
    run.stop().await;
}

/// Returns once the task has taken every command sent before it: a
/// stats read runs during a decode.
async fn stats(cmd_tx: &mpsc::Sender<SessionCmd>) -> SessionStats {
    let (reply, stats) = oneshot::channel();
    send(cmd_tx, SessionCmd::Stats { reply }).await;
    tokio::time::timeout(Duration::from_secs(10), stats)
        .await
        .expect("a stats read is answered during the decode")
        .expect("the task answers")
}

fn resize(sub: SubscriberId, size: (u16, u16)) -> SessionCmd {
    SessionCmd::Input {
        sub,
        msg: InputMsg::Resize {
            dims: resize_dims(size),
        },
        reservation: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_during_the_decode_applies_before_the_output_after_the_image_parses() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let emit = format!(
        "\\033[?1049h\\033_Ga=t,i=1,f=100,q=2;{}\\033\\\\\\033[30;1HTAIL",
        base64(&png(2, 2, 3))
    );
    let run = start(capture_session(&path, &emit, true)).await;
    send(&run.cmd_tx, resize(run.sub, (40, 80))).await;
    send(&run.cmd_tx, paste(run.sub, b"go\n")).await;
    meet(&run.gate).await;
    send(&run.cmd_tx, resize(run.sub, (20, 80))).await;
    stats(&run.cmd_tx).await;
    assert_eq!(
        run.core.lock().grid.rows(),
        40,
        "the resize waits for the commit"
    );

    meet(&run.gate).await;
    wait_for_grid_text(&run.core, "TAIL").await;
    assert_eq!(run.core.lock().grid.rows(), 20);
    run.stop().await;
}

fn input(task: &mut SessionTask, sub: SubscriberId, msg: InputMsg) {
    let ended = task.handle_cmd(SessionCmd::Input {
        sub,
        msg,
        reservation: None,
    });
    assert!(ended.is_none(), "the input ends nothing");
}

fn resize_msg(size: (u16, u16)) -> InputMsg {
    InputMsg::Resize {
        dims: resize_dims(size),
    }
}

/// Two windows, the first owning input at the default size and the
/// second asking for `second`.
fn two_windows(
    task: &mut SessionTask,
    second: (u16, u16),
) -> ([SubscriberId; 2], [mpsc::UnboundedReceiver<OutEvent>; 2]) {
    let (first, rx) = attach_sub(task, false);
    let (other, other_rx) = attach_sub(task, false);
    input(
        task,
        first,
        resize_msg((crate::DEFAULT_ROWS, crate::DEFAULT_COLS)),
    );
    input(task, first, InputMsg::KeyBytes(Vec::new()));
    input(task, other, resize_msg(second));
    ([first, other], [rx, other_rx])
}

fn grid_size(task: &SessionTask) -> (u16, u16) {
    let core = task.session.lock_core();
    (core.grid.rows(), core.grid.cols())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_window_owning_input_at_the_commit_sets_the_size_it_applies() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut task = capture_task(&dir.path().join("pty-in")).await;
    let ([a, b], _rx) = two_windows(&mut task, (crate::DEFAULT_ROWS, 40));
    let default = (crate::DEFAULT_ROWS, crate::DEFAULT_COLS);
    assert_eq!(grid_size(&task), default);

    let image = command("a=t,i=1,f=100,q=2", &png(2, 2, 4), false);
    let mut decoded = false;
    feed(&mut task, &image, |task, _| {
        input(task, b, InputMsg::KeyBytes(Vec::new()));
        input(task, a, InputMsg::KeyBytes(Vec::new()));
        assert_eq!(
            grid_size(task),
            default,
            "both transfers wait for the commit"
        );
        decoded = true;
    })
    .await;
    assert!(decoded, "the PNG decodes off the task");
    assert_eq!(task.active_sub, Some(a));
    assert_eq!(
        grid_size(&task),
        default,
        "the size of the window that took input back applies, not the one it took it from"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_moving_size_ownership_reaches_the_program_after_its_resize() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    let ([_, b], _rx) = two_windows(&mut task, (12, 40));

    let mut script = b"\x1b[?2048h".to_vec();
    script.extend(command("a=t,i=1,f=100,q=2", &png(2, 2, 5), false));
    let mut decoded = false;
    feed(&mut task, &script, |task, _| {
        input(task, b, InputMsg::KeyBytes(b"typed".to_vec()));
        write_pty(&task.session.writer, b"during").expect("a write mid-decode");
        assert_eq!(grid_size(task).1, crate::DEFAULT_COLS);
        decoded = true;
    })
    .await;
    assert!(decoded, "the PNG decodes off the task");
    assert_eq!(grid_size(&task), (12, 40));
    let seen = capture_through(&path, b"typed").await;
    let during = find_bytes(&seen, b"during").expect("the mid-decode write");
    let report = find_bytes(&seen, b"\x1b[48;12;40;").expect("the resize report");
    let typed = find_bytes(&seen, b"typed").unwrap();
    assert!(
        during < report && report < typed,
        "the keystroke waits for the resize its window's ownership makes: {seen:?}"
    );
}

/// The alternate screen's text after a shrink and a grow during the
/// decode, or after the same resizes once the image is placed.
async fn alternate_screen_after_shrink_and_grow(
    inline: bool,
    path: &std::path::Path,
) -> Vec<String> {
    let mut task = capture_task(path).await;
    let (sub, _rx) = attach_sub(&mut task, false);
    let resizes = [
        (4, crate::DEFAULT_COLS),
        (crate::DEFAULT_ROWS, crate::DEFAULT_COLS),
    ];
    let mut script = b"\x1b[?1049h\x1b[10;1HLOW".to_vec();
    script.extend(command("a=t,i=1,f=100,q=2", &png(2, 2, 6), false));
    if inline {
        feed_in_place(&mut task, &script);
        for size in resizes {
            input(&mut task, sub, resize_msg(size));
        }
    } else {
        feed(&mut task, &script, |task, _| {
            for size in resizes {
                input(task, sub, resize_msg(size));
            }
        })
        .await;
    }
    let core = task.session.lock_core();
    let clusters = core.grid.cluster_table();
    (0..core.grid.rows())
        .map(|r| felis_grid::row_text_trim(core.grid.row_cells(r).unwrap_or(&[]), clusters))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shrink_and_grow_during_the_decode_trim_the_alternate_screen_as_in_place() {
    let dir = tempfile::TempDir::new().unwrap();
    let offloaded =
        alternate_screen_after_shrink_and_grow(false, &dir.path().join("offloaded")).await;
    let in_place = alternate_screen_after_shrink_and_grow(true, &dir.path().join("in-place")).await;
    assert!(
        !in_place.iter().any(|row| row.contains("LOW")),
        "the shrink trims the row in place"
    );
    assert_eq!(offloaded, in_place);
}

fn input_accepted(rx: &mut mpsc::UnboundedReceiver<OutEvent>) -> bool {
    drain_events(rx).iter().any(|ev| {
        matches!(
            ev,
            OutEvent::Session {
                msg: SessionToClientMsg::InputAccepted,
                ..
            }
        )
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fences_behind_held_input_are_answered_once_it_reaches_the_program() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    let ([_, b], [_rx_a, mut rx_b]) = two_windows(&mut task, (12, 40));
    drain_events(&mut rx_b);

    let image = command("a=t,i=1,f=100,q=2", &png(2, 2, 7), false);
    let mut send_fence = None;
    feed(&mut task, &image, |task, _| {
        input(task, b, InputMsg::KeyBytes(b"held".to_vec()));
        assert!(
            task.handle_cmd(SessionCmd::InputFence {
                sub: b,
                request: test_request(),
            })
            .is_none()
        );
        let (reply, answer) = oneshot::channel();
        assert!(
            task.handle_cmd(SessionCmd::SendFence {
                watch: None,
                id: 0,
                reply,
            })
            .is_none()
        );
        write_pty(&task.session.writer, b"during").expect("a write mid-decode");
        assert!(
            !input_accepted(&mut rx_b),
            "the input fence waits for the commit"
        );
        send_fence = Some(answer);
    })
    .await;
    let mut send_fence = send_fence.expect("the PNG decodes off the task");
    assert!(
        input_accepted(&mut rx_b),
        "the input fence is answered at the commit"
    );
    assert_eq!(
        send_fence.try_recv(),
        Ok(true),
        "the send fence is answered at the commit"
    );
    let seen = capture_through(&path, b"held").await;
    assert!(
        find_bytes(&seen, b"during").unwrap() < find_bytes(&seen, b"held").unwrap(),
        "the fences' input reaches the program at the commit: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_leaving_during_the_decode_still_resizes_and_types() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    let (sub, _rx) = attach_sub(&mut task, false);
    input(&mut task, sub, InputMsg::KeyBytes(Vec::new()));

    let mut script = b"\x1b[?2048h".to_vec();
    script.extend(command("a=t,i=1,f=100,q=2", &png(2, 2, 8), false));
    let mut decoded = false;
    feed(&mut task, &script, |task, _| {
        input(task, sub, resize_msg((12, 40)));
        input(task, sub, InputMsg::KeyBytes(b"bye".to_vec()));
        assert!(task.handle_cmd(SessionCmd::Unsubscribe { sub }).is_none());
        decoded = true;
    })
    .await;
    assert!(decoded, "the PNG decodes off the task");
    assert_eq!(grid_size(&task), (12, 40));
    assert!(task.sub_index(sub).is_none(), "the window left");
    let seen = capture_through(&path, b"bye").await;
    assert!(
        find_bytes(&seen, b"\x1b[48;12;40;").unwrap() < find_bytes(&seen, b"bye").unwrap(),
        "the resize reaches the program before the keystroke: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_evicted_during_the_decode_still_has_its_held_keystrokes_written() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let mut task = capture_task(&path).await;
    let ([_, b], _rx) = two_windows(&mut task, (12, 40));

    let image = command("a=t,i=1,f=100,q=2", &png(2, 2, 9), false);
    let mut decoded = false;
    feed(&mut task, &image, |task, _| {
        input(task, b, InputMsg::KeyBytes(b"stranded".to_vec()));
        // What a push the window's outbox refuses does.
        task.remove_sub(b);
        decoded = true;
    })
    .await;
    assert!(decoded, "the PNG decodes off the task");
    capture_through(&path, b"stranded").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_exiting_during_the_decode_ends_its_wait_with_the_mark_after_the_image() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!(
            "stty raw -echo; printf READY; read -r _go; printf '\\033_Ga=t,i=1,f=100,q=2;{}\\033\\\\\\033]133;D;7\\007'",
            base64(&png(2, 2, 10))
        ),
    ]);
    cmd.env_clear();
    cmd.env("PATH", crate::fixture_path());
    let session = Session::from_spawned(crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"));
    let mut watcher = None;
    let run = start_with(session, |task| {
        let (conn, rx) = requester(task);
        let watch = Some((conn, test_stream(1)));
        assert_eq!(task.prepare_send(watch, &[]), Prepared::Ready);
        assert!(task.send_fence(watch, 1));
        watcher = Some(rx);
    })
    .await;
    let mut watcher = watcher.unwrap();
    drain_events(&mut watcher);
    send(&run.cmd_tx, paste(run.sub, b"go\n")).await;
    meet(&run.gate).await;
    // Past several exit polls with the child gone.
    tokio::time::sleep(Duration::from_millis(400)).await;
    stats(&run.cmd_tx).await;
    let early = drain_events(&mut watcher);
    assert!(early.is_empty(), "the wait outlives the decode: {early:?}");

    meet(&run.gate).await;
    let first = tokio::time::timeout(Duration::from_secs(10), watcher.recv())
        .await
        .expect("the wait ends")
        .expect("the requester stays open");
    assert!(
        matches!(
            &first,
            OutEvent::Ops { msg, .. } if matches!(**msg, OpsToClientMsg::Mark { exit_code: Some(7) })
        ),
        "the mark after the image ends the wait, not the exit: {first:?}"
    );
    run.stop().await;
}

/// The exit poll can see the child gone before any drain has met the
/// image its last output stopped on; the poll's own drain then starts
/// the decode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_exit_seen_before_its_image_decodes_waits_for_the_mark_after_the_image() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (conn, mut watcher) = requester(&mut task);
    let watch = Some((conn, test_stream(1)));
    assert_eq!(task.prepare_send(watch, &[]), Prepared::Ready);
    assert!(task.send_fence(watch, 1));
    drain_events(&mut watcher);

    let mut output = command("a=t,i=1,f=100,q=2", &png(2, 2, 10), false);
    output.extend_from_slice(b"\x1b]133;D;7\x07");
    let stopped = {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        core.parser.advance_until_yield(&mut core.grid, &output)
    }
    .expect("the parse stops on the image");

    task.on_child_exit(Some(ExitStatus::Code(0)));
    assert!(task.decode.is_some(), "the exit's drain starts the decode");
    task.poll_child_exit();
    let early = drain_events(&mut watcher);
    assert!(
        early.is_empty(),
        "the exit waits behind the image: {early:?}"
    );
    assert!(!task.child_exited);

    let joined = (&mut task.decode.as_mut().unwrap().handle).await;
    assert!(task.commit_decode(joined).expect("the commit").is_none());
    {
        let mut core = task.session.lock_core();
        let core = &mut *core;
        let rest = core
            .parser
            .advance_until_yield(&mut core.grid, &output[stopped..]);
        assert_eq!(rest, None, "nothing after the image stops the parse");
    }
    task.drain_and_fan().expect("the drain");
    let first = drain_events(&mut watcher);
    assert!(
        matches!(
            first.first(),
            Some(OutEvent::Ops { msg, .. })
                if matches!(**msg, OpsToClientMsg::Mark { exit_code: Some(7) })
        ),
        "the mark after the image ends the wait, not the exit: {first:?}"
    );
    task.poll_child_exit();
    assert!(task.child_exited, "the exit lands once its output drained");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_report_mode_set_before_a_query_reports_after_its_reply() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let emit = format!(
        "\\033[?2048h\\033_Ga=q,i=1,f=100;{}\\033\\\\",
        base64(&png(2, 2, 11))
    );
    let run = start(capture_session(&path, &emit, true)).await;
    send(&run.cmd_tx, paste(run.sub, b"go\n")).await;
    meet(&run.gate).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    stats(&run.cmd_tx).await;
    meet(&run.gate).await;
    let seen = capture_through(&path, b"\x1b[48;").await;
    let reply = find_bytes(&seen, b"\x1b_Gi=1;OK\x1b\\").expect("the query is answered");
    assert!(
        reply < find_bytes(&seen, b"\x1b[48;").unwrap(),
        "the report follows the reply to what was parsed before it: {seen:?}"
    );
    run.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_going_quiet_after_the_image_still_gets_a_later_resize_report() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let emit = format!(
        "\\033[?2048h\\033_Ga=t,i=1,f=100,q=2;{}\\033\\\\",
        base64(&png(2, 2, 15))
    );
    let run = start(capture_session(&path, &emit, true)).await;
    send(&run.cmd_tx, paste(run.sub, b"go\n")).await;
    meet(&run.gate).await;
    meet(&run.gate).await;
    stats(&run.cmd_tx).await;
    send(
        &run.cmd_tx,
        SessionCmd::Input {
            sub: run.sub,
            msg: resize_msg((10, 40)),
            reservation: None,
        },
    )
    .await;
    capture_through(&path, b"\x1b[48;10;40;").await;
    run.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_during_the_decode_ends_the_session_without_waiting_for_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let transmit = format!(
        "\\033_Ga=t,i=1,f=100,q=2;{}\\033\\\\",
        base64(&png(2, 2, 12))
    );
    let run = start(capture_session(&dir.path().join("pty-in"), &transmit, true)).await;
    send(&run.cmd_tx, paste(run.sub, b"go\n")).await;
    meet(&run.gate).await;
    assert!(run.shutdown.send(&run.cmd_tx).await);
    tokio::time::timeout(Duration::from_secs(10), run.done)
        .await
        .expect("the session ends with the decode still held")
        .expect("the run loop ends cleanly");
    meet(&run.gate).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keystroke_owed_a_resize_report_follows_the_reply_and_the_report() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("pty-in");
    let emit = format!(
        "\\033[?2048h\\033_Ga=q,i=1,f=100;{}\\033\\\\",
        base64(&png(2, 2, 13))
    );
    let run = start(capture_session(&path, &emit, true)).await;
    send(&run.cmd_tx, paste(run.sub, b"go\n")).await;
    meet(&run.gate).await;
    send(&run.cmd_tx, paste(run.sub, b"typed")).await;
    stats(&run.cmd_tx).await;
    meet(&run.gate).await;
    let seen = capture_through(&path, b"typed").await;
    let reply = find_bytes(&seen, b"\x1b_Gi=1;OK\x1b\\").expect("the query is answered");
    let report = find_bytes(&seen, b"\x1b[48;").expect("the resize report");
    let typed = find_bytes(&seen, b"typed").unwrap();
    assert!(
        reply < report && report < typed,
        "the keystroke waits behind the report it would have written first: {seen:?}"
    );
    run.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_reaches_a_session_whose_decode_held_commands_to_the_cap() {
    let dir = tempfile::TempDir::new().unwrap();
    let transmit = format!(
        "\\033_Ga=t,i=1,f=100,q=2;{}\\033\\\\",
        base64(&png(2, 2, 14))
    );
    let run = start(capture_session(&dir.path().join("pty-in"), &transmit, true)).await;
    send(&run.cmd_tx, paste(run.sub, b"go\n")).await;
    meet(&run.gate).await;
    let sent = Arc::new(AtomicUsize::new(0));
    let flood = {
        let cmd_tx = run.cmd_tx.clone();
        let sub = run.sub;
        let sent = Arc::clone(&sent);
        tokio::spawn(async move {
            for rows in (0..).map(|n| 10 + n % 20) {
                if cmd_tx.send(resize(sub, (rows, 80))).await.is_err() {
                    return;
                }
                sent.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    // An empty queue slot says nothing about the hold: a task the host
    // has not scheduled yet takes more, and the shutdown's send lands.
    let full = tokio::time::timeout(Duration::from_secs(30), async {
        while sent.load(Ordering::Relaxed) < HELD_CMDS_CAP + CMD_CHANNEL_CAPACITY {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(full.is_ok(), "the held commands reach the cap and back up");
    let delivered = tokio::time::timeout(Duration::from_secs(10), run.shutdown.send(&run.cmd_tx))
        .await
        .expect("the shutdown returns with the queue still full");
    assert!(
        !delivered,
        "the session ends before its queue takes the command"
    );
    tokio::time::timeout(Duration::from_secs(10), run.done)
        .await
        .expect("the session ends with its command intake stopped")
        .expect("the run loop ends cleanly");
    flood.await.expect("the flood ends with the session");
}

fn command(controls: &str, payload: &[u8], chunked: bool) -> Vec<u8> {
    let encoded = base64(payload);
    let split = (encoded.len() / 8) * 4;
    if chunked && split > 0 {
        let (head, tail) = encoded.split_at(split);
        format!("\x1b_G{controls},m=1;{head}\x1b\\\x1b_Gm=0;{tail}\x1b\\").into_bytes()
    } else {
        format!("\x1b_G{controls};{encoded}\x1b\\").into_bytes()
    }
}

/// A PNG decodes off the task; the raw and the small zlib payloads in
/// place.
fn image_source(frame: bool) -> impl Strategy<Value = (String, Vec<u8>)> {
    let (w, h) = if frame { (1, 1) } else { (2, 2) };
    let raw = vec![0x55; (w * h * 4) as usize];
    let zlib = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
    prop_oneof![
        3 => (0u8..4).prop_map(move |seed| ("f=100".to_owned(), png(w, h, seed))),
        2 => Just((format!("f=32,s={w},v={h}"), raw)),
        1 => Just(("f=100".to_owned(), b"not a png".to_vec())),
        1 => Just((format!("f=32,s={w},v={h},o=z"), zlib)),
    ]
}

fn image_item() -> impl Strategy<Value = Vec<u8>> {
    let id = prop_oneof![
        Just("i=1"),
        Just("i=2"),
        Just("I=3"),
        Just("a=t"),
        Just("i=1,I=2")
    ];
    let quiet = prop_oneof![Just(""), Just(",q=1"), Just(",q=2")];
    let put = prop_oneof![
        Just(""),
        Just(",c=2,r=1"),
        Just(",C=1"),
        Just(",U=1,c=1,r=1"),
        Just(",p=4")
    ];
    let transmit = (
        prop_oneof![Just("t"), Just("T"), Just("q")],
        id,
        quiet.clone(),
        put.clone(),
        image_source(false),
        any::<bool>(),
    )
        .prop_map(|(a, id, quiet, put, (format, payload), chunked)| {
            command(
                &format!("{id},a={a},{format}{quiet}{put}"),
                &payload,
                chunked,
            )
        });
    let frame = (
        prop_oneof![Just("i=1"), Just("i=2"), Just("i=9")],
        prop_oneof![Just(""), Just(",r=1"), Just(",c=1,z=50"), Just(",r=2,X=1")],
        quiet.clone(),
        image_source(true),
    )
        .prop_map(|(id, edit, quiet, (format, payload))| {
            command(
                &format!("a=f,{id},x=1,y=1,{format}{edit}{quiet}"),
                &payload,
                false,
            )
        });
    let control = prop_oneof![
        (
            prop_oneof![Just("i=1"), Just("i=2"), Just("I=3")],
            quiet,
            put
        )
            .prop_map(|(id, quiet, put)| format!("a=p,{id}{quiet}{put}")),
        Just("a=d,d=a".to_owned()),
        Just("a=d,d=A".to_owned()),
        Just("a=d,d=I,i=1".to_owned()),
        Just("a=d,d=f,i=1,r=2".to_owned()),
        Just("a=a,i=1,s=3,v=1".to_owned()),
        Just("a=a,i=1,c=2".to_owned()),
        Just("a=c,i=1,r=1,c=2,w=1,h=1".to_owned()),
    ]
    .prop_map(|controls| format!("\x1b_G{controls}\x1b\\").into_bytes());
    prop_oneof![4 => transmit, 2 => frame, 3 => control]
}

fn text_item() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        "[a-z]{1,6}".prop_map(String::into_bytes),
        Just(b"\r\n".to_vec()),
        Just(b"\r\n\r\n\r\n\r\n".to_vec()),
        Just(b"\x1b[2J".to_vec()),
        Just(b"\x1b[3J".to_vec()),
        Just(b"\x1b[?1049h".to_vec()),
        Just(b"\x1b[?1049l".to_vec()),
        Just(b"\x1b[c".to_vec()),
        Just(b"\x1b[6n".to_vec()),
        Just(b"\x1b[3;5H".to_vec()),
        Just(b"\x1b[S".to_vec()),
    ]
}

/// A command sent while a decode is in flight, or after the image in
/// place.
#[derive(Debug, Clone)]
enum Event {
    /// From the first window (`false`) or the second: a resize to the
    /// size, or input with no bytes, which moves size ownership.
    Window(bool, Option<(u16, u16)>),
    InputFence(bool),
    SendFence,
    /// The offloaded run's oldest admitted late window leaves.
    Detach,
}

/// What runs between a decode's prepare and its commit.
#[derive(Debug, Clone)]
struct Interleaved {
    tick_ms: Option<u64>,
    pull: bool,
    attach: bool,
    events: Vec<Event>,
}

fn event() -> impl Strategy<Value = Event> {
    prop_oneof![
        6 => (
            any::<bool>(),
            proptest::option::weighted(0.6, (4u16..10, 10u16..40)),
        )
            .prop_map(|(second, resize)| Event::Window(second, resize)),
        1 => any::<bool>().prop_map(Event::InputFence),
        1 => Just(Event::SendFence),
        1 => Just(Event::Detach),
    ]
}

fn interleaved() -> impl Strategy<Value = Interleaved> {
    (
        proptest::option::of(1u64..120),
        any::<bool>(),
        any::<bool>(),
        proptest::collection::vec(event(), 0..5),
    )
        .prop_map(|(tick_ms, pull, attach, events)| Interleaved {
            tick_ms,
            pull,
            attach,
            events,
        })
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    replies: Vec<u8>,
    images: felis_grid::images::ImageStore,
    placements: felis_grid::images::Placements,
    saved: crate::graphics::SavedScreens,
    text: Vec<String>,
    cursor: (u16, u16),
    mirror: Vec<String>,
}

const REPLIES_END: &[u8] = b"\x1b]replies-end\x07";

async fn outcome(
    task: &mut SessionTask,
    path: &std::path::Path,
    rx: &mut mpsc::UnboundedReceiver<OutEvent>,
) -> Outcome {
    task.drive_cycle().expect("the final drain");
    write_pty(&task.session.writer, REPLIES_END).expect("the sentinel");
    let mut replies = capture_through(path, REPLIES_END).await;
    replies.truncate(find_bytes(&replies, REPLIES_END).unwrap());
    let images = task.session.images.clone();
    let mirror = mirror(&images, rx);
    let core = task.session.lock_core();
    let clusters = core.grid.cluster_table();
    Outcome {
        replies,
        images,
        placements: task.session.placements.clone(),
        saved: task.session.saved_screens.clone(),
        text: core
            .grid
            .rows_with_wrap(felis_grid::AltScreenRows::Include)
            .map(|(row, _)| felis_grid::row_text_trim(row, clusters))
            .collect(),
        cursor: (core.grid.cursor().row, core.grid.cursor().col),
        mirror,
    }
}

/// What a window's image mirror holds of the daemon's store.
fn mirror(
    images: &felis_grid::images::ImageStore,
    rx: &mut mpsc::UnboundedReceiver<OutEvent>,
) -> Vec<String> {
    let mut shadow = felis_client_core::image_shadow::ImageShadow::new();
    while let Ok(ev) = rx.try_recv() {
        if let OutEvent::Image(msg) = ev {
            shadow
                .apply(&msg)
                .expect("the mirror admits every image event");
        }
    }
    let mut mirror: Vec<String> = images
        .iter_ids()
        .map(|id| {
            let pixels = shadow.image(id).map(|image| image.pixels().to_vec());
            format!("{id:?} {pixels:?}")
        })
        .chain(shadow.placements().map(|p| format!("{p:?}")))
        .chain(shadow.virtual_placements().iter().map(|v| format!("{v:?}")))
        .collect();
    mirror.sort();
    mirror
}

type SubscribeReply = oneshot::Receiver<Result<SubscribeOk, SubscribeRefused>>;

/// A window attach as a connection sends it, whose rehydrate burst is
/// left in the outbox.
fn attach_window(task: &mut SessionTask) -> (SubscribeReply, mpsc::UnboundedReceiver<OutEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (reply_tx, reply_rx) = oneshot::channel();
    assert!(
        task.handle_cmd(SessionCmd::Subscribe(SubscribeReq {
            mode: ConnectionMode::Window,
            pull_paced: false,
            minor: felis_protocol::preface::PROTOCOL_MINOR,
            if_exited: IfExited::Land,
            image_regions: false,
            carrier: AttachCarrier::Local { client_pid: None },
            tx,
            buffered: Arc::new(AtomicUsize::new(0)),
            reply: reply_tx,
        }))
        .is_none()
    );
    (reply_rx, rx)
}

/// A late window and its id once its attach was admitted.
struct LateWindow {
    reply: SubscribeReply,
    sub: Option<SubscriberId>,
    rx: mpsc::UnboundedReceiver<OutEvent>,
}

impl LateWindow {
    fn admitted(&mut self) -> Option<SubscriberId> {
        if self.sub.is_none()
            && let Ok(ok) = self.reply.try_recv()
        {
            self.sub = Some(ok.expect("the subscribe was admitted").sub);
        }
        self.sub
    }
}

fn resize_dims((rows, cols): (u16, u16)) -> RequestedDims {
    RequestedDims {
        rows: u32::from(rows),
        cols: u32::from(cols),
        pixel_w: u32::from(cols) * 9,
        pixel_h: u32::from(rows) * 17,
    }
}

fn interleave(
    task: &mut SessionTask,
    subs: [SubscriberId; 2],
    events: &[Event],
    late: Option<&mut Vec<LateWindow>>,
    send_fences: &mut Vec<oneshot::Receiver<bool>>,
) {
    let mut late = late;
    for event in events {
        let cmd = match *event {
            Event::Window(second, resize) => SessionCmd::Input {
                sub: subs[usize::from(second)],
                msg: resize.map_or(InputMsg::KeyBytes(Vec::new()), resize_msg),
                reservation: None,
            },
            Event::InputFence(second) => SessionCmd::InputFence {
                sub: subs[usize::from(second)],
                request: test_request(),
            },
            Event::SendFence => {
                let (reply, answer) = oneshot::channel();
                send_fences.push(answer);
                SessionCmd::SendFence {
                    watch: None,
                    id: 0,
                    reply,
                }
            }
            Event::Detach => {
                let Some(late) = late.as_deref_mut() else {
                    continue;
                };
                let Some(at) = late.iter_mut().position(|w| w.admitted().is_some()) else {
                    continue;
                };
                let leaving = late.remove(at);
                SessionCmd::Unsubscribe {
                    sub: leaving.sub.unwrap(),
                }
            }
        };
        assert!(task.handle_cmd(cmd).is_none(), "the command ends nothing");
    }
}

/// Feeds `bytes` as the parse sink does, a drive cycle at every stop.
/// `at_decode` runs while each offloaded decode is in flight and is
/// handed the offset the parse stopped at.
async fn feed(
    task: &mut SessionTask,
    bytes: &[u8],
    mut at_decode: impl FnMut(&mut SessionTask, usize),
) {
    let mut pos = 0;
    loop {
        let stopped = {
            let mut core = task.session.lock_core();
            let core = &mut *core;
            core.parser
                .advance_until_yield(&mut core.grid, &bytes[pos..])
        };
        task.drive_cycle().expect("the drain");
        if task.decode.is_some() {
            at_decode(task, pos + stopped.expect("only a stopped parse offloads"));
            let joined = {
                let decode = task.decode.as_mut().unwrap();
                (&mut decode.handle).await
            };
            assert!(task.commit_decode(joined).expect("the commit").is_none());
        }
        match stopped {
            Some(consumed) => pos += consumed,
            None => return,
        }
    }
}

/// Feeds `bytes` with every image decoded in place, as the parse
/// stopping on it never happened.
fn feed_in_place(task: &mut SessionTask, bytes: &[u8]) {
    let mut pos = 0;
    loop {
        let core = Arc::clone(&task.session.core);
        let mut guard = core.lock();
        let stopped = {
            let core = &mut *guard;
            core.parser
                .advance_until_yield(&mut core.grid, &bytes[pos..])
        };
        let written = streaming::drain_effects_in_place(&mut task.session, &mut guard);
        task.settle_drain(&guard, streaming::Drained::Done(written))
            .expect("the drain");
        task.ship_cycle(guard);
        match stopped {
            Some(consumed) => pos += consumed,
            None => return,
        }
    }
}

/// Runs the script with each decode offloaded and `interleaved` work
/// done while it is in flight; returns what each offloaded decode's
/// item saw.
async fn run_offloaded(
    items: &[Vec<u8>],
    groups: &[usize],
    plan: &[Interleaved],
    path: &std::path::Path,
) -> (Outcome, Vec<(usize, Interleaved)>) {
    let mut task = capture_task(path).await;
    let (subs, [mut rx, _second_rx]) = two_windows(&mut task, (12, 30));
    let sub = subs[0];
    let mut extra_subs = Vec::new();
    let mut send_fences = Vec::new();
    let mut seen = Vec::new();
    let mut now = 0;
    let mut start = 0;
    for &end in groups {
        let group = &items[start..end];
        let ends: Vec<usize> = group
            .iter()
            .scan(0, |at, item| {
                *at += item.len();
                Some(*at)
            })
            .collect();
        feed(&mut task, &group.concat(), |task, stop| {
            let item = start + ends.partition_point(|&end| end < stop);
            let work = plan[seen.len() % plan.len()].clone();
            if let Some(ms) = work.tick_ms {
                now += ms;
                task.animation_tick_at(now);
            }
            if work.pull {
                task.grid_cycle_for(sub).expect("the pull");
            }
            if work.attach {
                let (reply, rx) = attach_window(task);
                extra_subs.push(LateWindow {
                    reply,
                    sub: None,
                    rx,
                });
            }
            interleave(
                task,
                subs,
                &work.events,
                Some(&mut extra_subs),
                &mut send_fences,
            );
            assert!(
                task.stats().decode_bytes_in_flight.is_some(),
                "the resource report counts the decode in flight"
            );
            seen.push((item, work));
        })
        .await;
        start = end;
    }
    let outcome = outcome(&mut task, path, &mut rx).await;
    for answer in send_fences {
        assert_eq!(answer.await, Ok(true), "every send fence is answered");
    }
    for mut late in extra_subs {
        assert!(late.admitted().is_some(), "every late attach is admitted");
        assert_eq!(
            mirror(&task.session.images, &mut late.rx),
            outcome.mirror,
            "a window attached mid-decode mirrors what the first one does"
        );
    }
    (outcome, seen)
}

/// The same script decoded in place, item by item, with the work
/// [`run_offloaded`] interleaved done where the dispatch would see it:
/// ticks before the image, the commands sent while it decoded after it.
async fn run_in_place(
    items: &[Vec<u8>],
    seen: &[(usize, Interleaved)],
    path: &std::path::Path,
) -> Outcome {
    let mut task = capture_task(path).await;
    let (subs, [mut rx, _second_rx]) = two_windows(&mut task, (12, 30));
    let mut now = 0;
    for (i, item) in items.iter().enumerate() {
        let work = seen.iter().find(|(at, _)| *at == i).map(|(_, work)| work);
        if let Some(ms) = work.and_then(|work| work.tick_ms) {
            now += ms;
            task.animation_tick_at(now);
        }
        feed_in_place(&mut task, item);
        if let Some(work) = work {
            interleave(&mut task, subs, &work.events, None, &mut Vec::new());
        }
    }
    outcome(&mut task, path, &mut rx).await
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// An offloaded decode commits exactly what decoding in place would:
    /// the replies, both screens' images and placements, the cells, the
    /// cursor and a window's mirror, whatever runs while it is in flight
    /// and however the parse batches the commands around it.
    #[test]
    fn an_offloaded_decode_ends_where_decoding_in_place_does(
        items in proptest::collection::vec(prop_oneof![3 => image_item(), 2 => text_item()], 1..24),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..4),
        plan in proptest::collection::vec(interleaved(), 1..4),
    ) {
        let mut groups: Vec<usize> = cuts.iter().map(|cut| cut.index(items.len())).collect();
        groups.push(items.len());
        groups.sort_unstable();
        groups.dedup();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let (offloaded, in_place, seen) = runtime.block_on(async {
            let (offloaded, seen) =
                run_offloaded(&items, &groups, &plan, &dir.path().join("offloaded")).await;
            let in_place = run_in_place(&items, &seen, &dir.path().join("in-place")).await;
            (offloaded, in_place, seen)
        });
        prop_assert_eq!(offloaded, in_place, "with {:?} interleaved", seen);
    }
}

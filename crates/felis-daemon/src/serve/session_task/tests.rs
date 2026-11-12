#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_protocol::messages::{
    KeyEventKind, PromptJump, RequestedDims, RetargetCarrier, RetargetLanding, RetargetTarget,
    SpawnArgs,
};

use felis_pty::Command;

use crate::test_support::sh;

use proptest::prelude::*;

use super::*;

mod command_output;
mod decode_offload;
mod exit_fences;
mod fences;
mod idle_compaction;
mod input;
mod osc52;
mod placements;
mod presentation;
mod program_status;
mod pull_cycles;
mod resize;
mod streams;
mod subscribers;
mod supervision;
mod viewport;

fn test_request() -> RequestId {
    RequestId::new(1).unwrap_or_else(|| unreachable!("1 is non-zero"))
}

fn test_stream(n: u64) -> StreamId {
    StreamId::new(n).unwrap_or_else(|| unreachable!("callers pass non-zero"))
}

/// Builtin-only bodies; one that runs an external program wants
/// [`crate::test_support::fixture_path`], or it silently runs nothing on a host
/// with no FHS `/bin`.
fn session_with(body: &str) -> Session {
    let cmd = sh(body, "/bin:/usr/bin");
    Session::from_spawned(crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"))
}

/// Asserted on the grid rather than the reader half, which is
/// EOF-only in sink mode.
async fn wait_for_grid_text(core: &parking_lot::Mutex<ParseCore>, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let text: String = {
            let core = core.lock();
            let clusters = core.grid.cluster_table();
            core.grid
                .rows_with_wrap(felis_grid::AltScreenRows::Include)
                .map(|(row, _)| felis_grid::row_text_trim(row, clusters) + "\n")
                .collect()
        };
        if text.contains(needle) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "grid never showed {needle:?}; grid text:\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn attach_sub(
    task: &mut SessionTask,
    pull_paced: bool,
) -> (SubscriberId, mpsc::UnboundedReceiver<OutEvent>) {
    attach_sub_as(task, ConnectionMode::Window, pull_paced)
}

fn attach_ops_sub(task: &mut SessionTask) -> (SubscriberId, mpsc::UnboundedReceiver<OutEvent>) {
    attach_sub_as(task, ConnectionMode::Ops, false)
}

fn attach_sub_as(
    task: &mut SessionTask,
    mode: ConnectionMode,
    pull_paced: bool,
) -> (SubscriberId, mpsc::UnboundedReceiver<OutEvent>) {
    attach_sub_if_exited(task, mode, pull_paced, IfExited::Land)
}

fn attach_sub_if_exited(
    task: &mut SessionTask,
    mode: ConnectionMode,
    pull_paced: bool,
    if_exited: IfExited,
) -> (SubscriberId, mpsc::UnboundedReceiver<OutEvent>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (reply_tx, mut reply_rx) = oneshot::channel();
    task.subscribe(SubscribeReq {
        mode,
        pull_paced,
        minor: felis_protocol::preface::PROTOCOL_MINOR,
        if_exited,
        image_regions: false,
        carrier: AttachCarrier::Local { client_pid: None },
        tx,
        buffered: Arc::new(AtomicUsize::new(0)),
        reply: reply_tx,
    });
    let ok = reply_rx
        .try_recv()
        .expect("subscribe reply")
        .expect("the subscribe was admitted");
    while !matches!(
        rx.try_recv().expect("rehydrate event"),
        OutEvent::Grid(GridMsg::RehydrateEnd)
    ) {}
    (ok.sub, rx)
}

fn attach_burst(task: &mut SessionTask, mode: ConnectionMode) -> Vec<OutEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (reply_tx, mut reply_rx) = oneshot::channel();
    task.subscribe(SubscribeReq {
        mode,
        pull_paced: false,
        minor: felis_protocol::preface::PROTOCOL_MINOR,
        if_exited: IfExited::Land,
        image_regions: false,
        carrier: AttachCarrier::Local { client_pid: None },
        tx,
        buffered: Arc::new(AtomicUsize::new(0)),
        reply: reply_tx,
    });
    reply_rx
        .try_recv()
        .expect("subscribe reply")
        .expect("the subscribe was admitted");
    let mut burst = Vec::new();
    loop {
        let ev = rx.try_recv().expect("rehydrate event");
        let end = matches!(ev, OutEvent::Grid(GridMsg::RehydrateEnd));
        burst.push(ev);
        if end {
            return burst;
        }
    }
}

fn drain_events(rx: &mut mpsc::UnboundedReceiver<OutEvent>) -> Vec<OutEvent> {
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    events
}

fn press_key(task: &mut SessionTask, sub: SubscriberId, key: &[u8]) {
    let end = task.handle_input(sub, InputMsg::KeyBytes(key.to_vec()));
    assert!(end.is_none(), "KeyBytes is a continuing input");
}

fn take_input_ownership(task: &mut SessionTask, sub: SubscriberId) {
    press_key(task, sub, b"k");
}

const CAPTURE_READY: &str = "CAPTURE-READY";

/// A child that saves its own stdin to `path`.
///
/// Uses `stty raw -echo` so line discipline does not hold bytes awaiting newline
/// and echo does not feed stdin back into the parser. `emit` runs after `stty`,
/// or with `on_go` after the child prints `READY` and reads a line.
fn capture_session(path: &std::path::Path, emit: &str, on_go: bool) -> Session {
    let go = if on_go {
        "printf READY; read -r _go; "
    } else {
        ""
    };
    let cmd = sh(
        &format!(
            "stty raw -echo; {go}printf '{emit}'; printf {CAPTURE_READY}; exec cat > {}",
            path.display()
        ),
        crate::test_support::fixture_path(),
    );
    Session::from_spawned(crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"))
}

/// Waits for the child's `stty` to land; bytes written before it
/// would sit in the line discipline and miss the capture.
async fn capture_task(path: &std::path::Path) -> SessionTask {
    let task = SessionTask::for_tests(capture_session(path, "", false));
    wait_for_grid_text(&task.session.core, CAPTURE_READY).await;
    task
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

async fn capture_through(path: &std::path::Path, sentinel: &[u8]) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let seen = std::fs::read(path).unwrap_or_default();
        if find_bytes(&seen, sentinel).is_some() {
            return seen;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the PTY never received {sentinel:?}; captured: {seen:?}",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn set_mode_2048(task: &SessionTask, on: bool) {
    let mut core = task.session.lock_core();
    let mut parser = felis_vt::Parser::new();
    parser.advance(
        &mut core.grid,
        if on { b"\x1b[?2048h" } else { b"\x1b[?2048l" },
    );
    assert_eq!(core.grid.in_band_resize_notify(), on);
}

fn pty_pixels(task: &SessionTask) -> (u16, u16) {
    task.session.lock_core().grid.winsize_pixels()
}

fn resize_to(task: &mut SessionTask, sub: SubscriberId, rows: u32, cols: u32) -> Option<EndReason> {
    resize_px(task, sub, (rows, cols), (0, 0))
}

/// `pixels` is `(width, height)`.
fn resize_px(
    task: &mut SessionTask,
    sub: SubscriberId,
    (rows, cols): (u32, u32),
    (pixel_w, pixel_h): (u32, u32),
) -> Option<EndReason> {
    task.handle_input(
        sub,
        InputMsg::Resize {
            dims: RequestedDims {
                rows,
                cols,
                pixel_w,
                pixel_h,
            },
        },
    )
}

fn fence(task: &mut SessionTask, sub: SubscriberId) -> Option<EndReason> {
    task.handle_cmd(SessionCmd::InputFence {
        sub,
        request: test_request(),
    })
}

fn shipped_rows(events: &[OutEvent]) -> usize {
    events
        .iter()
        .filter_map(|ev| match ev {
            OutEvent::Grid(GridMsg::RowDelta { rows }) => Some(rows.len()),
            _ => None,
        })
        .sum()
}

fn mirror_rows(shadow: &felis_client_core::ShadowScreen) -> Vec<String> {
    let screen = shadow.screen();
    let clusters = screen.cluster_table();
    (0..screen.rows())
        .map(|r| felis_grid::row_text_trim(screen.row_cells(r).unwrap_or(&[]), clusters))
        .collect()
}

fn grid_rows(task: &SessionTask) -> Vec<String> {
    let core = task.session.lock_core();
    let clusters = core.grid.cluster_table();
    (0..core.grid.rows())
        .map(|r| felis_grid::row_text_trim(core.grid.row_cells(r).unwrap_or(&[]), clusters))
        .collect()
}

fn mirror_apply(shadow: &mut felis_client_core::ShadowScreen, events: &[OutEvent]) {
    for ev in events {
        if let OutEvent::Grid(msg) = ev {
            shadow.apply(msg).expect("the mirror admits every frame");
        }
    }
}

fn write_to_grid(task: &SessionTask, bytes: &[u8]) {
    let mut core = task.session.lock_core();
    let core = &mut *core;
    core.parser.advance(&mut core.grid, bytes);
}

fn new_mirror(task: &SessionTask) -> felis_client_core::ShadowScreen {
    let core = task.session.lock_core();
    felis_client_core::ShadowScreen::new(core.grid.rows(), core.grid.cols())
}

fn queued(outcome: PushOutcome) -> u32 {
    match outcome {
        PushOutcome::Accepted(queued) => queued,
        PushOutcome::Denied(denied) => panic!("unexpected denial: {denied:?}"),
    }
}

fn requester(task: &mut SessionTask) -> (RequesterId, mpsc::UnboundedReceiver<OutEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let id = RequesterId::next();
    task.add_requester(RequesterHandle {
        id,
        tx,
        buffered: Arc::new(AtomicUsize::new(0)),
        evict: Arc::new(Notify::new()),
    });
    (id, rx)
}

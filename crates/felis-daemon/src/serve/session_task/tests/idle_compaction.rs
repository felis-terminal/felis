//! A quiet session's history moving to the cold tier
//! (docs/explanation/data-model/scrollback.md "Idle compaction"). Most
//! tests run on paused time and stand in for the parse sink, so output
//! lands at instants the test picks; one drives a real shell.

use std::fmt::Write as _;

use felis_grid::{Grid, SearchOptions, SearchQuery};
use felis_protocol::messages::SearchSource;

use super::super::idle::{self, IdleProbe, QUIET, Step};
use super::*;
use crate::parse_sink::ParseSignals;

const HANG: Duration = Duration::from_secs(600);

/// Every 9th line wraps, every 25th starts a marked prompt, and every
/// line carries a color, so the history has wraps, styles and marks.
fn lines(from: usize, count: usize) -> Vec<u8> {
    let mut out = String::new();
    for i in from..from + count {
        if i % 25 == 0 {
            out.push_str("\x1b]133;A\x07$ \x1b]133;B\x07");
        }
        if i % 9 == 0 {
            write!(out, "\x1b[3{}m{}", i % 8, "w".repeat(150)).unwrap();
        }
        write!(out, "\x1b[3{}mrow {i}\x1b[m\r\n", i % 8).unwrap();
    }
    out.into_bytes()
}

/// History, viewport, search and marks as readers see them.
#[derive(Debug, PartialEq, Eq)]
struct Reads {
    history: Vec<(Vec<u8>, bool)>,
    viewport: Vec<(Vec<u8>, bool)>,
    hits: Vec<String>,
    marks: Vec<String>,
}

fn reads(g: &Grid) -> Reads {
    let ansi = |row: &[felis_grid::Cell]| {
        felis_grid::row_ansi(row, g.cluster_table(), g.screen().style_table())
    };
    let sb = g.scrollback();
    let history = (0..sb.len())
        .map(|i| (ansi(sb.row(i).unwrap()), sb.soft_wrap_continued(i).unwrap()))
        .collect();
    let viewport = g
        .rows_with_wrap(felis_grid::AltScreenRows::Include)
        .map(|(row, wrap)| (ansi(row), wrap))
        .collect();
    let query = SearchQuery::new(
        "row 1[0-9]*7",
        SearchOptions {
            regex: true,
            case_insensitive: false,
        },
    )
    .unwrap();
    let hits = g
        .search(&query, SearchSource::Scrollback)
        .map(|hit| format!("{hit:?}"))
        .collect();
    let marks = g
        .prompt_marks()
        .iter()
        .map(|m| format!("{m:?}@{:?}", g.locate_line(m.line)))
        .collect();
    Reads {
        history,
        viewport,
        hits,
        marks,
    }
}

fn assert_reads_as(g: &Grid, twin: &Grid, what: &str) {
    let (got, want) = (reads(g), reads(twin));
    assert_eq!(
        got.history.len(),
        want.history.len(),
        "{what}: history length"
    );
    assert!(got.history == want.history, "{what}: history rows");
    assert_eq!(got.viewport, want.viewport, "{what}: viewport");
    assert_eq!(got.hits, want.hits, "{what}: search hits");
    assert_eq!(got.marks, want.marks, "{what}: prompt marks");
}

/// A run loop over a shell that prints nothing, whose output the test
/// applies itself the way the parse sink does, mirrored into an
/// uncompacted twin.
struct Quiet {
    core: Arc<parking_lot::Mutex<ParseCore>>,
    signals: Arc<ParseSignals>,
    probe: Arc<IdleProbe>,
    twin: ParseCore,
    rx: mpsc::UnboundedReceiver<OutEvent>,
    shadow: felis_client_core::ShadowScreen,
    cmd_tx: mpsc::Sender<SessionCmd>,
    done: tokio::task::JoinHandle<()>,
}

impl Quiet {
    fn start() -> Self {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        let shadow = new_mirror(&task);
        let (_sub, rx) = attach_sub(&mut task, false);
        let (rows, cols) = {
            let core = task.session.lock_core();
            (core.grid.rows(), core.grid.cols())
        };
        let core = Arc::clone(&task.session.core);
        let signals = Arc::clone(&task.session.signals);
        let probe = Arc::new(IdleProbe::default());
        task.idle.probe = Some(Arc::clone(&probe));
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
        let done = tokio::spawn(run_session(task, cmd_rx));
        Self {
            core,
            signals,
            probe,
            twin: ParseCore::new(rows, cols),
            rx,
            shadow,
            cmd_tx,
            done,
        }
    }

    /// What `parse_sink::build_sink` does with one chunk.
    fn feed(&mut self, bytes: &[u8]) {
        {
            let mut guard = self.core.lock();
            let core = &mut *guard;
            core.parser.advance(&mut core.grid, bytes);
            core.maybe_gc_tables();
            self.signals.note_output();
        }
        self.signals.mark_dirty();
        let twin = &mut self.twin;
        twin.parser.advance(&mut twin.grid, bytes);
    }

    async fn until(&self, what: &str, done: impl Fn(&[(tokio::time::Instant, Step)]) -> bool) {
        let waited = tokio::time::timeout(HANG, async {
            loop {
                let changed = self.probe.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if done(&self.probe.log.lock()) {
                    return;
                }
                changed.await;
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "never saw {what}: {:?}",
            self.probe.log.lock()
        );
    }

    async fn until_runs_done(&self, runs: usize) {
        self.until("the run finish", |log| {
            log.iter().filter(|(_, s)| *s == Step::RunDone).count() >= runs
        })
        .await;
    }

    fn resident(&self) -> usize {
        self.core.lock().grid.resident_cell_bytes()
    }

    fn assert_reads_as_twin(&self, what: &str) {
        assert_reads_as(&self.core.lock().grid, &self.twin.grid, what);
    }

    async fn stop(self) {
        drop(self.cmd_tx);
        self.done
            .await
            .expect("the run loop ends on a dropped pool");
    }
}

/// The ring rows a full history occupies at the session's width.
fn full_ring_bytes(q: &Quiet) -> usize {
    let core = q.core.lock();
    let rows = core.grid.scrollback_capacity() + usize::from(core.grid.rows());
    rows * usize::from(core.grid.cols()) * size_of::<felis_grid::Cell>()
}

/// Spec: compaction runs to the end after the last chunk, whatever its
/// size, with no further output to drive it, and readers cannot tell.
#[tokio::test(start_paused = true)]
async fn the_last_chunk_of_any_size_is_followed_by_a_full_compaction() {
    for last in [1usize, 2, 3, 200, 9000] {
        let mut q = Quiet::start();
        q.feed(&lines(0, felis_grid::DEFAULT_SCROLLBACK_ROWS + 30));
        q.feed(&lines(100_000, last));
        let full = q.resident();
        assert!(full * 10 > full_ring_bytes(&q) * 9, "the ring is full");
        q.until_runs_done(1).await;
        let held = q.resident();
        assert!(
            held * 3 < full,
            "after {last} last lines the session holds {held} of {full} bytes"
        );
        q.assert_reads_as_twin(&format!("{last} last lines"));
        q.stop().await;
    }
}

/// Spec: output that lands mid-run ends it at the next slice; the window
/// receives that output, and the next quiet interval finishes the job.
#[tokio::test(start_paused = true)]
async fn output_mid_run_ends_it_and_reaches_the_window() {
    let mut q = Quiet::start();
    q.feed(&lines(0, felis_grid::DEFAULT_SCROLLBACK_ROWS + 30));
    q.until("a first slice", |log| {
        log.iter().any(|(_, s)| *s == Step::Slice)
    })
    .await;
    assert_eq!(q.probe.count(Step::RunDone), 0, "a run several slices long");
    q.feed(b"\x1b[1;35mRESUMED\x1b[m\r\n");
    q.until("the run stop", |log| {
        log.iter().any(|(_, s)| *s == Step::StoppedByOutput)
    })
    .await;
    let slices_before = q.probe.count(Step::Slice);

    let frame = tokio::time::timeout(HANG, async {
        loop {
            let ev = q.rx.recv().await.expect("the outbox stays open");
            mirror_apply(&mut q.shadow, std::slice::from_ref(&ev));
            if mirror_rows(&q.shadow).iter().any(|r| r == "RESUMED") {
                return;
            }
        }
    })
    .await;
    assert!(frame.is_ok(), "the window never showed the new output");
    assert_eq!(
        mirror_rows(&q.shadow),
        {
            let core = q.core.lock();
            let clusters = core.grid.cluster_table();
            (0..core.grid.rows())
                .map(|r| felis_grid::row_text_trim(core.grid.row_content(r).unwrap(), clusters))
                .collect::<Vec<_>>()
        },
        "the window holds the session's rows",
    );

    q.until_runs_done(1).await;
    assert!(q.probe.count(Step::Slice) > slices_before);
    let stopped = q
        .probe
        .log
        .lock()
        .iter()
        .find(|(_, s)| *s == Step::StoppedByOutput)
        .unwrap()
        .0;
    let resumed = q
        .probe
        .log
        .lock()
        .iter()
        .find(|(at, s)| *s == Step::Slice && *at > stopped)
        .unwrap()
        .0;
    assert!(
        resumed - stopped >= QUIET,
        "the run resumes only after a fresh quiet interval"
    );
    q.assert_reads_as_twin("after the resumed run");
    q.stop().await;
}

/// Spec: no slice runs while output keeps arriving faster than the quiet
/// interval, in chunks that land back to back and so reach the task as
/// one dirty wake, and the timer did fire meanwhile; compaction follows
/// once output stops.
#[tokio::test(start_paused = true)]
async fn no_slice_runs_while_output_keeps_flowing() {
    let mut q = Quiet::start();
    q.feed(&lines(0, felis_grid::DEFAULT_SCROLLBACK_ROWS + 30));
    let step = QUIET * 3 / 4;
    let mut last_output = tokio::time::Instant::now();
    for i in 0..40 {
        tokio::time::advance(step).await;
        for j in 0..3 {
            q.feed(&lines(20_000 + i * 3 + j, 1));
        }
        last_output = tokio::time::Instant::now();
    }
    q.until_runs_done(1).await;
    let log = q.probe.log.lock().clone();
    let fired_during_output = log
        .iter()
        .filter(|(at, s)| *s == Step::TimerFired && *at < last_output)
        .count();
    assert!(
        fired_during_output >= 10,
        "the quiet timer fired {fired_during_output} times while output flowed"
    );
    let first_slice = log.iter().find(|(_, s)| *s == Step::Slice).unwrap().0;
    assert!(
        first_slice - last_output >= QUIET,
        "a slice ran {:?} after output",
        first_slice - last_output
    );
    q.assert_reads_as_twin("after output stopped");
    q.stop().await;
}

/// Spec: a gap a little longer than the quiet interval is enough to
/// compact everything, because slices run back to back once it starts.
#[tokio::test(start_paused = true)]
async fn each_gap_between_bursts_compacts_the_whole_history() {
    let mut q = Quiet::start();
    q.feed(&lines(0, felis_grid::DEFAULT_SCROLLBACK_ROWS + 30));
    let full = q.resident();
    for burst in 0..4 {
        let fed = tokio::time::Instant::now();
        q.until_runs_done(burst + 1).await;
        let done = q.probe.log.lock().last().unwrap().0;
        assert!(
            done - fed <= QUIET + QUIET / 2,
            "burst {burst}: compaction finished {:?} after output",
            done - fed
        );
        assert!(
            q.resident() * 3 < full,
            "burst {burst}: {} of {full} bytes",
            q.resident()
        );
        q.assert_reads_as_twin(&format!("after burst {burst}"));
        q.feed(&lines(50_000 + burst * 1000, 1000));
    }
    q.stop().await;
}

/// A shell that prints its history and then waits for input: the real
/// parse sink advances the generation, and compaction follows on its
/// own once the shell goes quiet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_shell_that_goes_quiet_has_its_history_compacted() {
    let mut task = SessionTask::for_tests(session_with(
        "i=0; while [ $i -lt 10100 ]; do echo \"line $i\"; i=$((i+1)); done; echo DONE; read _x",
    ));
    let core = Arc::clone(&task.session.core);
    let probe = Arc::new(IdleProbe::default());
    task.idle.probe = Some(Arc::clone(&probe));
    let (_sub, _rx) = attach_sub(&mut task, false);
    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
    let done = tokio::spawn(run_session(task, cmd_rx));
    wait_for_grid_text(&core, "DONE").await;
    let waited = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let changed = probe.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let compacted = {
                let core = core.lock();
                core.grid.scrollback().len() == felis_grid::DEFAULT_SCROLLBACK_ROWS
                    && probe.count(Step::RunDone) > 0
                    && core.grid.resident_cell_bytes() * 3 < core.grid.reserved_cell_bytes()
            };
            if compacted {
                return;
            }
            changed.await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "the quiet shell's history was never compacted"
    );
    drop(cmd_tx);
    done.await.unwrap();
}

/// Measurement, not a gate (`cargo nextest run --release -p felis-daemon
/// --all-features --run-ignored only measure_ --no-capture`): the longest
/// lock hold a compaction slice takes at the largest admitted geometry
/// with every row full, and what an upgrade's capture clone costs before
/// and after compaction there and at 54x192 with shell-length lines.
#[test]
#[ignore = "measurement"]
fn measure_slice_and_capture_lock_holds() {
    use felis_protocol::messages::{MAX_GRID_COLS, MAX_GRID_ROWS};
    for (rows, cols, width) in [
        (MAX_GRID_ROWS, MAX_GRID_COLS, usize::from(MAX_GRID_COLS)),
        (54, 192, 33),
    ] {
        let mut core = ParseCore::new(rows, cols);
        for i in 0..felis_grid::DEFAULT_SCROLLBACK_ROWS + usize::from(rows) {
            let len = (i * 7919) % (2 * width + 1);
            let line = format!("\x1b[44m{}\r\n", "x".repeat(len.min(usize::from(cols))));
            core.parser.advance(&mut core.grid, line.as_bytes());
        }
        let start = Instant::now();
        let copy = core.grid.clone();
        println!(
            "{rows}x{cols}: capture clone of the full ring {:?}",
            start.elapsed()
        );
        drop(copy);
        let slice = idle::slice_rows(usize::from(cols));
        let (mut longest, mut slices) = (Duration::ZERO, 0);
        let run = Instant::now();
        loop {
            let start = Instant::now();
            let done = core.grid.compact_history(slice);
            longest = longest.max(start.elapsed());
            slices += 1;
            if done {
                break;
            }
        }
        println!(
            "{rows}x{cols}: {slices} slices of {slice} rows, longest {longest:?}, all {:?}",
            run.elapsed()
        );
        let start = Instant::now();
        let copy = core.grid.clone();
        println!(
            "{rows}x{cols}: capture clone once compacted {:?}",
            start.elapsed()
        );
        drop(copy);
    }
}

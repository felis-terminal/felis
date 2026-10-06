//! The PTY byte sink and its signals.
//!
//! Feeds PTY bytes to the parser, coordinating with session tasks via
//! [`crate::pool::ParseCore`] and [`ParseSignals`].

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::pool::ParseCore;

/// Sustained parse rate for a session with zero subscribers, in bytes
/// per second. Not derived from `IdlePolicy::drain_interval`: the sink
/// is built at spawn time, before any policy is attached.
const PARKED_RATE_BYTES_PER_SEC: u64 = 10 * 1024 * 1024;

/// Burst a freshly parked session may parse at full speed before the
/// rate applies: enough for a post-detach flourish (a TUI redrawing on
/// SIGWINCH, a prompt reprint).
const PARKED_BURST_BYTES: u64 = 1024 * 1024;

/// Token-bucket pacer for a parked session's parse rate.
///
/// Throttling the sink pauses the reader thread and blocks child `write(2)`
/// in the kernel PTY buffer, reducing detached CPU usage.
struct ParsePacer {
    rate: u64,
    burst: u64,
    tokens: f64,
    last_refill: Instant,
}

impl ParsePacer {
    #[allow(clippy::cast_precision_loss)]
    const fn new(rate: u64, burst: u64, now: Instant) -> Self {
        Self {
            rate,
            burst,
            tokens: burst as f64,
            last_refill: now,
        }
    }

    /// Account one chunk; returns how long the caller must sleep before
    /// parsing it.
    #[allow(clippy::cast_precision_loss)]
    fn debit(&mut self, n: usize, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.last_refill = now;
        self.tokens = elapsed
            .as_secs_f64()
            .mul_add(self.rate as f64, self.tokens)
            .min(self.burst as f64);
        self.tokens -= n as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate as f64)
        }
    }
}

/// The sink ⇄ session-task signal block, one per session.
pub struct ParseSignals {
    dirty: AtomicBool,
    notify: Notify,
    /// Attached subscriber count, mirrored by the session task; a stale
    /// read costs at most one mis-paced chunk.
    subs: AtomicUsize,
    /// The sink's stop after a cursor-moving image placement
    /// (`Parser::advance_until_yield`) until the task has dispatched it.
    gate: parking_lot::Mutex<YieldGate>,
    gate_cv: parking_lot::Condvar,
    drain_notify: Notify,
}

#[derive(Default)]
struct YieldGate {
    requested: u64,
    drained: u64,
    closed: bool,
}

impl ParseSignals {
    #[must_use]
    pub fn new() -> Self {
        Self {
            dirty: AtomicBool::new(false),
            notify: Notify::new(),
            subs: AtomicUsize::new(0),
            gate: parking_lot::Mutex::new(YieldGate::default()),
            gate_cv: parking_lot::Condvar::new(),
            drain_notify: Notify::new(),
        }
    }

    pub fn mark_dirty(&self) {
        if !self.dirty.swap(true, Ordering::AcqRel) {
            self.notify.notify_one();
        }
    }

    /// Call **before** `take_pty_effects`: a chunk parsed while the task
    /// drains must re-flag rather than strand its effects.
    pub fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Release);
    }

    /// Resolves when the sink has parsed something since the last
    /// [`Self::clear_dirty`]. Cancel-safe in `select!`.
    pub async fn parsed(&self) {
        self.notify.notified().await;
    }

    /// Blocks the sink until the task has drained every effect queued
    /// before this call, or the task is gone. Never called holding the
    /// core lock: the drain takes it.
    pub fn wait_for_drain(&self) {
        let mut gate = self.gate.lock();
        if gate.closed {
            return;
        }
        gate.requested += 1;
        let target = gate.requested;
        self.drain_notify.notify_one();
        while gate.drained < target && !gate.closed {
            self.gate_cv.wait(&mut gate);
        }
    }

    /// Resolves when the sink waits in [`Self::wait_for_drain`].
    /// Cancel-safe in `select!`.
    pub async fn drain_requested(&self) {
        self.drain_notify.notified().await;
    }

    /// Read **before** `take_pty_effects` and handed to
    /// [`Self::publish_drained`] after: every wait counted here had queued
    /// its effects before it was counted.
    pub fn drain_generation(&self) -> u64 {
        self.gate.lock().requested
    }

    pub fn publish_drained(&self, generation: u64) {
        let mut gate = self.gate.lock();
        if generation > gate.drained {
            gate.drained = generation;
            self.gate_cv.notify_all();
        }
    }

    /// Releases a waiting sink for good: no task will drain again.
    pub fn close(&self) {
        self.gate.lock().closed = true;
        self.gate_cv.notify_all();
    }

    pub fn set_attached_subs(&self, n: usize) {
        self.subs.store(n, Ordering::Relaxed);
    }

    fn attached_subs(&self) -> usize {
        self.subs.load(Ordering::Relaxed)
    }
}

impl Default for ParseSignals {
    fn default() -> Self {
        Self::new()
    }
}

/// The per-chunk sink the PTY parse thread runs. Blocking here (the
/// pacer's sleep, the core lock) is the backpressure contract on
/// [`felis_pty::ByteSink`].
pub fn build_sink(
    core: Arc<parking_lot::Mutex<ParseCore>>,
    signals: Arc<ParseSignals>,
) -> felis_pty::ByteSink {
    let mut pacer: Option<ParsePacer> = None;
    Box::new(move |chunk| {
        if signals.attached_subs() == 0 {
            let now = Instant::now();
            let pacer = pacer.get_or_insert_with(|| {
                ParsePacer::new(PARKED_RATE_BYTES_PER_SEC, PARKED_BURST_BYTES, now)
            });
            let pause = pacer.debit(chunk.len(), now);
            if !pause.is_zero() {
                std::thread::sleep(pause);
            }
        } else {
            pacer = None;
        }
        let mut rest = chunk;
        loop {
            let stopped = {
                let mut guard = core.lock();
                let core = &mut *guard;
                let stopped = core.parser.advance_until_yield(&mut core.grid, rest);
                core.maybe_gc_tables();
                stopped
            };
            signals.mark_dirty();
            let Some(consumed) = stopped else {
                break;
            };
            signals.wait_for_drain();
            rest = &rest[consumed..];
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_core() -> Arc<parking_lot::Mutex<ParseCore>> {
        Arc::new(parking_lot::Mutex::new(ParseCore::new(24, 80)))
    }

    fn top_row_text(core: &Arc<parking_lot::Mutex<ParseCore>>) -> String {
        let guard = core.lock();
        felis_grid::row_text_trim(
            guard.grid.row_content(0).unwrap(),
            guard.grid.cluster_table(),
        )
    }

    /// Measurement (ignored): the single-threaded parse-into-grid floor
    /// over the bench file; the sink can never beat this.
    #[test]
    #[ignore = "measurement; needs FELIS_BENCH_FILE, run with --run-ignored all"]
    fn measure_parse_into_grid_floor() {
        let file = std::env::var("FELIS_BENCH_FILE")
            .expect("set FELIS_BENCH_FILE to the payload file to parse");
        let data = std::fs::read(&file).unwrap();
        // Captured payloads only replay in their original geometry.
        let rows = std::env::var("FELIS_BENCH_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24);
        let cols = std::env::var("FELIS_BENCH_COLS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(80);
        let run = |gc: bool| {
            let mut core = ParseCore::new(rows, cols);
            let start = Instant::now();
            for chunk in data.chunks(64 * 1024) {
                core.parser.advance(&mut core.grid, chunk);
                if gc {
                    core.maybe_gc_tables();
                }
                drop(core.grid.take_pty_effects());
                core.grid.damage_mut().clear();
            }
            start.elapsed().as_secs_f64()
        };
        let bare = run(false);
        let swept = run(true);
        let mib = data.len() as f64 / (1024.0 * 1024.0);
        eprintln!(
            "parse-into-grid floor: {mib:.0} MiB in {swept:.3}s = {:.1} MiB/s \
             (parse alone {bare:.3}s = {:.1} MiB/s; registry sweeps {:.3}s)",
            mib / swept,
            mib / bare,
            swept - bare,
        );
    }

    /// A burst passes free, then debt accrues at the sustained rate.
    #[test]
    fn pacer_charges_debt_at_the_sustained_rate_after_the_burst() {
        let t0 = Instant::now();
        let mut pacer = ParsePacer::new(10 * 1024 * 1024, 1024 * 1024, t0);
        assert_eq!(pacer.debit(1024 * 1024, t0), Duration::ZERO);
        let pause = pacer.debit(1024 * 1024, t0);
        let ms = pause.as_secs_f64() * 1000.0;
        assert!((99.0..101.0).contains(&ms), "expected ~100ms, got {ms}ms");
    }

    /// Elapsed wall time refills the bucket: after sleeping off the
    /// debt, an on-rate producer is never asked to sleep again.
    #[test]
    fn pacer_refills_with_elapsed_time() {
        let t0 = Instant::now();
        let mut pacer = ParsePacer::new(10 * 1024 * 1024, 1024 * 1024, t0);
        let _ = pacer.debit(2 * 1024 * 1024, t0);
        let t1 = t0 + Duration::from_millis(200);
        let pause = pacer.debit(1024 * 1024, t1);
        assert!(
            pause < Duration::from_millis(2),
            "on-rate producer must not sleep, got {pause:?}"
        );
    }

    /// Refill is capped at the burst: a long idle gap must not bank an
    /// unbounded free allowance.
    #[test]
    fn pacer_caps_the_bank_at_the_burst() {
        let t0 = Instant::now();
        let mut pacer = ParsePacer::new(10 * 1024 * 1024, 1024 * 1024, t0);
        let t1 = t0 + Duration::from_secs(3600);
        assert_eq!(pacer.debit(1024 * 1024, t1), Duration::ZERO);
        assert!(pacer.debit(1024 * 1024, t1) > Duration::from_millis(90));
    }

    /// The dirty flag is edge-triggered: two marks in a row store one
    /// permit, and a fresh mark after `clear_dirty` notifies again.
    #[tokio::test]
    async fn dirty_edge_notifies_once_per_drain_cycle() {
        let signals = ParseSignals::new();
        signals.mark_dirty();
        signals.mark_dirty();
        tokio::time::timeout(Duration::from_secs(1), signals.parsed())
            .await
            .expect("first parsed() must resolve from the stored permit");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), signals.parsed())
                .await
                .is_err(),
            "coalesced marks must yield a single permit"
        );
        signals.clear_dirty();
        signals.mark_dirty();
        tokio::time::timeout(Duration::from_secs(1), signals.parsed())
            .await
            .expect("a mark after clear_dirty must notify again");
    }

    #[test]
    fn a_waiting_sink_is_released_by_the_drain_that_follows_its_request() {
        let signals = Arc::new(ParseSignals::new());
        let before = signals.drain_generation();
        let waiter = std::thread::spawn({
            let signals = Arc::clone(&signals);
            move || signals.wait_for_drain()
        });
        while signals.drain_generation() == before {
            std::thread::yield_now();
        }
        signals.publish_drained(before);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished(), "a drain begun before the request");
        signals.publish_drained(signals.drain_generation());
        waiter.join().unwrap();
    }

    #[test]
    fn close_releases_a_waiting_sink_and_every_later_wait() {
        let signals = Arc::new(ParseSignals::new());
        let waiter = std::thread::spawn({
            let signals = Arc::clone(&signals);
            move || signals.wait_for_drain()
        });
        while signals.drain_generation() == 0 {
            std::thread::yield_now();
        }
        signals.close();
        waiter.join().unwrap();
        signals.wait_for_drain();
    }

    /// A chunked placement stops the parse once, after its last chunk,
    /// and the bytes after it parse once the task has drained.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sink_pauses_once_per_chunked_placement_until_drained() {
        let core = test_core();
        let signals = Arc::new(ParseSignals::new());
        let mut sink = build_sink(Arc::clone(&core), Arc::clone(&signals));
        let feeder = std::thread::spawn(move || {
            sink(b"\x1b_Ga=T,f=24,s=1,v=1,c=2,r=1,m=1;AA\x1b\\\x1b_Gm=1;AA\x1b\\");
            sink(b"\x1b_Gm=0;\x1b\\done");
        });
        tokio::time::timeout(Duration::from_secs(5), signals.drain_requested())
            .await
            .expect("the last chunk must request a drain");
        assert!(!top_row_text(&core).contains("done"), "parse stopped");
        signals.publish_drained(signals.drain_generation());
        tokio::task::spawn_blocking(move || feeder.join().unwrap())
            .await
            .unwrap();
        assert_eq!(signals.drain_generation(), 1, "one pause for the stream");
        assert!(top_row_text(&core).contains("done"));
    }

    /// End-to-end sink contract: bytes fed to the sink land in the
    /// shared grid and flip the dirty signal.
    #[tokio::test]
    async fn sink_parses_into_the_shared_core_and_marks_dirty() {
        let core = test_core();
        let signals = Arc::new(ParseSignals::new());
        let mut sink = build_sink(Arc::clone(&core), Arc::clone(&signals));
        sink(b"hello-sink");
        let row = top_row_text(&core);
        assert!(
            row.starts_with("hello-sink"),
            "sink must parse into the shared grid, got {row:?}"
        );
        tokio::time::timeout(Duration::from_secs(1), signals.parsed())
            .await
            .expect("the sink must mark the core dirty");
    }
}

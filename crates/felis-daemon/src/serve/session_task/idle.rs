//! Compacting a quiet session's scrollback
//! (docs/explanation/data-model/scrollback.md "Idle compaction").

use std::time::Duration;

use felis_protocol::messages::MAX_GRID_COLS;
use tokio::time::Instant;

/// Output must stop this long before a compaction run starts.
pub(super) const QUIET: Duration = Duration::from_millis(200);

/// Cells one slice may move: 64 rows at the widest admitted grid, which
/// is what bounds a slice's lock hold.
const SLICE_CELLS: usize = 64 * MAX_GRID_COLS as usize;

/// Rows one compaction slice moves at `cols` columns.
pub(super) fn slice_rows(cols: usize) -> usize {
    (SLICE_CELLS / cols.max(1)).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No run owed for [`IdleCompaction::armed`]'s generation.
    Settled,
    /// Waiting out the quiet interval since the generation was armed.
    Waiting(Instant),
    /// The quiet interval passed; slices run until done or output lands.
    Running,
}

/// When a session's history moves to the cold tier. Driven by the output
/// generation the parse sink advances per chunk, never by its content.
#[derive(Debug)]
pub(super) struct IdleCompaction {
    quiet: Duration,
    /// The generation the timer or the run belongs to; `None` asks for a
    /// fresh quiet interval whatever the generation.
    armed: Option<u64>,
    phase: Phase,
    #[cfg(all(test, unix))]
    pub(super) probe: Option<std::sync::Arc<IdleProbe>>,
}

/// Test-only: what the run loop did and when, as sync points.
#[cfg(all(test, unix))]
#[derive(Debug, Default)]
pub(super) struct IdleProbe {
    pub(super) log: parking_lot::Mutex<Vec<(Instant, Step)>>,
    pub(super) changed: tokio::sync::Notify,
}

#[cfg(all(test, unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    TimerFired,
    Slice,
    StoppedByOutput,
    RunDone,
}

#[cfg(all(test, unix))]
impl IdleProbe {
    pub(super) fn count(&self, step: Step) -> usize {
        self.log.lock().iter().filter(|(_, s)| *s == step).count()
    }
}

impl IdleCompaction {
    pub(super) const fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            armed: None,
            phase: Phase::Settled,
            #[cfg(all(test, unix))]
            probe: None,
        }
    }

    #[cfg(all(test, unix))]
    fn record(&self, at: Instant, step: Step) {
        if let Some(probe) = &self.probe {
            probe.log.lock().push((at, step));
            probe.changed.notify_waiters();
        }
    }

    /// Once per run-loop turn: output since the last run arms the timer.
    pub(super) fn observe(&mut self, generation: u64, now: Instant) {
        if self.phase == Phase::Settled && self.armed != Some(generation) {
            self.wait_from(generation, now);
        }
    }

    /// The grid changed shape without output (a resize): history is back
    /// in the ring and owes a run of its own.
    pub(super) const fn invalidate(&mut self) {
        self.armed = None;
    }

    pub(super) const fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Waiting(at) => Some(at),
            Phase::Settled | Phase::Running => None,
        }
    }

    pub(super) fn running(&self) -> bool {
        self.phase == Phase::Running
    }

    /// The quiet timer fired: the run starts only if no output landed
    /// since it was armed.
    pub(super) fn timer_fired(&mut self, generation: u64, now: Instant) {
        #[cfg(all(test, unix))]
        self.record(now, Step::TimerFired);
        if self.armed == Some(generation) {
            self.phase = Phase::Running;
        } else {
            self.wait_from(generation, now);
        }
    }

    /// One slice, with `generation` read under the lock `compact` runs
    /// under; `compact` returns `true` once nothing is left to move.
    pub(super) fn slice(&mut self, generation: u64, now: Instant, compact: impl FnOnce() -> bool) {
        if self.armed != Some(generation) {
            #[cfg(all(test, unix))]
            self.record(now, Step::StoppedByOutput);
            self.wait_from(generation, now);
            return;
        }
        let done = compact();
        #[cfg(all(test, unix))]
        self.record(now, Step::Slice);
        if done {
            self.phase = Phase::Settled;
            #[cfg(all(test, unix))]
            self.record(now, Step::RunDone);
        }
    }

    fn wait_from(&mut self, generation: u64, now: Instant) {
        self.armed = Some(generation);
        self.phase = Phase::Waiting(now + self.quiet);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Clone)]
    enum Event {
        /// Output landed this many milliseconds after the previous event.
        Output(u64),
        /// The loop turned with no output, this many milliseconds later.
        Turn(u64),
        Resize,
    }

    fn event() -> impl Strategy<Value = Event> {
        prop_oneof![
            (0u64..400).prop_map(Event::Output),
            (0u64..400).prop_map(Event::Turn),
            Just(Event::Resize),
        ]
    }

    proptest! {
        /// Driven as the run loop drives it (the timer fires at its
        /// deadline, a run takes slices back to back): a slice runs only
        /// once output has been quiet for the interval, and the last
        /// output or resize always leads to a finished run.
        #[test]
        fn slices_run_only_after_a_quiet_interval(
            events in proptest::collection::vec(event(), 1..60),
            slices_needed in 1usize..6,
        ) {
            let start = Instant::now();
            let mut idle = IdleCompaction::new(Duration::from_millis(200));
            let (mut generation, mut now, mut last_output) = (0u64, start, start);
            let mut left = slices_needed;
            for event in events {
                let step = match event {
                    Event::Output(ms) | Event::Turn(ms) => ms,
                    Event::Resize => 0,
                };
                let target = now + Duration::from_millis(step);
                while let Some(at) = idle.deadline().filter(|at| *at <= target) {
                    now = at;
                    idle.timer_fired(generation, now);
                    while idle.running() {
                        prop_assert!(now - last_output >= Duration::from_millis(200));
                        idle.slice(generation, now, || {
                            left = left.saturating_sub(1);
                            left == 0
                        });
                    }
                }
                now = target;
                match event {
                    Event::Output(_) => {
                        generation += 1;
                        last_output = now;
                        left = slices_needed;
                    }
                    Event::Resize => {
                        idle.invalidate();
                        left = slices_needed;
                    }
                    Event::Turn(_) => {}
                }
                idle.observe(generation, now);
                prop_assert!(!idle.running());
            }
            if let Some(at) = idle.deadline() {
                prop_assert!(at <= now + Duration::from_millis(200));
            } else {
                prop_assert_eq!(left, 0, "settled with history left to move");
            }
        }
    }

    #[test]
    fn output_mid_run_stops_it_and_rearms_the_timer() {
        let t0 = Instant::now();
        let mut idle = IdleCompaction::new(QUIET);
        idle.observe(1, t0);
        idle.timer_fired(1, t0 + QUIET);
        assert!(idle.running());
        idle.slice(1, t0 + QUIET, || false);
        assert!(idle.running(), "an unfinished run keeps going");
        let mut ran = false;
        idle.slice(2, t0 + QUIET, || {
            ran = true;
            true
        });
        assert!(!ran, "a slice that finds new output moves nothing");
        assert_eq!(idle.deadline(), Some(t0 + QUIET * 2));
    }

    #[test]
    fn a_finished_run_waits_for_output_or_a_resize() {
        let t0 = Instant::now();
        let mut idle = IdleCompaction::new(QUIET);
        idle.observe(7, t0);
        idle.timer_fired(7, t0 + QUIET);
        idle.slice(7, t0 + QUIET, || true);
        idle.observe(7, t0 + QUIET * 3);
        assert_eq!(idle.deadline(), None, "nothing new to compact");
        idle.invalidate();
        idle.observe(7, t0 + QUIET * 3);
        assert_eq!(idle.deadline(), Some(t0 + QUIET * 4));
    }

    #[test]
    fn a_slice_is_bounded_by_cells_at_every_width() {
        for cols in [1, 80, 192, 1000, usize::from(MAX_GRID_COLS)] {
            let rows = slice_rows(cols);
            assert!(rows >= 1);
            assert!(rows * cols <= SLICE_CELLS, "{cols} columns");
        }
        assert_eq!(slice_rows(usize::from(MAX_GRID_COLS)), 64);
    }
}

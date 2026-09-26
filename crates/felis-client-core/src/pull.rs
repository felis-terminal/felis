//! Demand-driven frame-pull scheduler (`docs/explanation/rendering/pipeline.md`).
//!
//! The daemon ships a grid diff only in answer to `InputMsg::NextGridFrame`.
//! Keeping at most one pull outstanding ensures idle sessions produce no wire traffic.

use std::time::{Duration, Instant};

use felis_protocol::messages::GridMsg;

/// Must agree with the daemon's pull-gated compose cycle
/// (`serve/streaming.rs` `compose_diffs`). Facet pushes and the resize
/// `Size` ship outside the gate without consuming `pull_pending`;
/// counting one as an answer would issue a replacement pull for a cycle
/// that never arrived.
#[must_use]
pub const fn answers_pull(msg: &GridMsg) -> bool {
    matches!(
        msg,
        GridMsg::RowDelta { .. }
            | GridMsg::Scrolled { .. }
            | GridMsg::CursorState { .. }
            | GridMsg::PromptMark { .. }
            | GridMsg::ViewportState { .. }
            | GridMsg::Hyperlink { .. }
            | GridMsg::Cluster { .. }
            | GridMsg::ModeFlags { .. }
            | GridMsg::RehydrateBegin
            | GridMsg::RehydrateEnd
    )
}

#[derive(Debug, Default)]
pub struct PullScheduler {
    enabled: bool,
    outstanding: bool,
    cycle_open: bool,
    /// Feeds the `felis::pull` `--trace-perf` event.
    sent: u64,
}

impl PullScheduler {
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            outstanding: false,
            cycle_open: false,
            sent: 0,
        }
    }

    /// Whether the peer delimits its cycles with [`GridMsg::CycleEnd`].
    /// The daemon emits one only for a pull-paced subscriber, so an
    /// eager-push connection would open a cycle no marker ever closes
    /// and stop painting for good.
    const fn marks_cycles(&self) -> bool {
        self.enabled
    }

    pub const fn on_frame(&mut self, msg: &GridMsg) {
        let rehydrate = matches!(msg, GridMsg::RehydrateBegin | GridMsg::RehydrateEnd);
        if !self.marks_cycles() {
            // A gate carried from a connection that died mid-cycle
            // ([`Self::holding_gate`]) can land here, and this peer
            // sends no marker: without the burst lifting it the window
            // would never paint again.
            if rehydrate {
                self.cycle_open = false;
            }
            if answers_pull(msg) {
                self.outstanding = false;
            }
            return;
        }
        match msg {
            GridMsg::CycleEnd => {
                self.cycle_open = false;
                self.outstanding = false;
            }
            // The burst ships eagerly, outside the pull gate, so no
            // `CycleEnd` follows it: it is its own boundary, and it is
            // the first thing to redescribe the whole screen after a
            // cycle the previous connection left half-applied.
            _ if rehydrate => self.cycle_open = false,
            _ if answers_pull(msg) => self.cycle_open = true,
            _ => {}
        }
    }

    /// Whether a compose cycle has begun arriving and has not been
    /// terminated: the grid on screen is one message short of the state
    /// the daemon composed, so painting it would show a partial cycle.
    #[must_use]
    pub const fn cycle_open(&self) -> bool {
        self.cycle_open
    }

    /// Carry a gate raised by a cycle the previous connection never
    /// terminated. Its frames already reached the shadow, so dropping
    /// the gate with the scheduler would paint the partial screen the
    /// marker exists to prevent; only the replacement's rehydrate
    /// burst restates it.
    #[must_use]
    pub const fn holding_gate(mut self, open: bool) -> Self {
        self.cycle_open = open;
        self
    }

    /// Call once per frame attempt, after `render`. Only a presented
    /// frame waited on vsync, so only it may pull
    /// (docs/explanation/rendering/pipeline.md "Demand-driven emission").
    pub const fn after_frame(&mut self, presented: bool) -> bool {
        presented && self.take_due()
    }

    const fn take_due(&mut self) -> bool {
        if !self.enabled || self.outstanding {
            return false;
        }
        self.outstanding = true;
        self.sent += 1;
        true
    }

    #[must_use]
    pub const fn sent(&self) -> u64 {
        self.sent
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedrawTurn {
    /// Presenting: flush whatever redraw is pending.
    Free,
    /// A retry fell due: redraw whether or not anything asked.
    Retry,
    /// Between retries: leave requests pending for the next retry.
    Hold,
}

/// Paces redraw retries while the surface cannot present: with no pull
/// out, no cycle arrives to request the next redraw, so this clock is
/// what notices the surface became presentable again.
#[derive(Debug, Default)]
pub struct PresentRetry {
    retry_at: Option<Instant>,
    interval: Duration,
}

impl PresentRetry {
    /// A window whose visibility event never arrives still repaints
    /// within this long of becoming presentable.
    pub const MAX_INTERVAL: Duration = Duration::from_secs(1);

    pub const fn presented(&mut self) {
        self.retry_at = None;
    }

    pub fn unpresented(&mut self, now: Instant, refresh: Duration) {
        self.interval = if self.stalled() {
            self.interval.saturating_mul(2)
        } else {
            refresh
        }
        .min(Self::MAX_INTERVAL);
        self.retry_at = Some(now + self.interval);
    }

    #[must_use]
    pub const fn stalled(&self) -> bool {
        self.retry_at.is_some()
    }

    #[must_use]
    pub const fn next_deadline(&self) -> Option<Instant> {
        self.retry_at
    }

    /// Every redraw source goes through here, not only the retry: a
    /// shader clock or an eager-push frame requesting a redraw between
    /// retries would fail the acquire at its own rate.
    pub fn turn(&mut self, now: Instant) -> RedrawTurn {
        if !self.stalled() {
            RedrawTurn::Free
        } else if self.take_due(now) {
            RedrawTurn::Retry
        } else {
            RedrawTurn::Hold
        }
    }

    /// Re-arms rather than clearing, so a redraw the paint gate drops is
    /// retried and never strands the window without a pull.
    fn take_due(&mut self, now: Instant) -> bool {
        match self.retry_at {
            Some(at) if now >= at => {
                self.retry_at = Some(now + self.interval);
                true
            }
            _ => false,
        }
    }

    /// The window reported it is visible again: retry now rather than
    /// at the next interval.
    pub const fn wake(&mut self, now: Instant) {
        if self.retry_at.is_some() {
            self.retry_at = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use felis_protocol::RowPayload;
    use felis_protocol::messages::{AttentionSource, CursorStyle, PromptKind, ScrollDirection};

    fn cycle_frame() -> GridMsg {
        GridMsg::RowDelta {
            rows: vec![(0, RowPayload(Vec::new()))],
        }
    }

    fn marked() -> PullScheduler {
        PullScheduler::new(true)
    }

    #[test]
    fn a_facet_push_does_not_answer_the_pull() {
        let mut s = marked();
        assert!(s.take_due());
        s.on_frame(&GridMsg::Attention {
            source: AttentionSource::Bell,
        });
        assert!(!s.take_due(), "a facet must leave the pull outstanding");
        assert!(!s.cycle_open(), "a facet push is not a cycle");
        s.on_frame(&cycle_frame());
        s.on_frame(&GridMsg::CycleEnd);
        assert!(s.take_due(), "the compose cycle answers it");
    }

    #[test]
    fn disabled_never_pulls() {
        let mut s = PullScheduler::new(false);
        assert!(!s.take_due());
        s.on_frame(&GridMsg::RehydrateEnd);
        assert!(!s.take_due());
    }

    /// An eager-push peer (an SSH-carried window) is sent no marker
    /// however new the daemon is, so tracking a cycle for it would
    /// freeze the window on its first live row.
    #[test]
    fn an_eager_push_connection_opens_no_cycle() {
        let mut s = PullScheduler::new(false);
        s.on_frame(&cycle_frame());
        assert!(!s.cycle_open(), "no pull gate, no cycle to wait on");
        s.on_frame(&GridMsg::Scrolled {
            region_top: 0,
            region_bottom: 3,
            n_rows: 1,
            direction: ScrollDirection::Up,
        });
        assert!(!s.cycle_open(), "nor on any later frame");
    }

    /// A rebuilt scheduler opens no cycle of its own: it has answered
    /// no pull yet, so only a gate handed to it can be held.
    #[test]
    fn a_rebuilt_scheduler_opens_no_cycle_of_its_own() {
        let landed = PullScheduler::new(false).holding_gate(false);
        assert!(!landed.cycle_open());
        assert!(!marked().cycle_open());
    }

    /// The frames of a cycle the transport cut short already reached
    /// the shadow, so the gate outlives both the connection and the
    /// scheduler; the replacement's burst is what redescribes the
    /// screen and lifts it.
    #[test]
    fn a_gate_from_an_abandoned_cycle_survives_until_the_replacement_rehydrates() {
        let mut lost = marked();
        assert!(lost.take_due());
        lost.on_frame(&GridMsg::Scrolled {
            region_top: 0,
            region_bottom: 3,
            n_rows: 1,
            direction: ScrollDirection::Up,
        });
        assert!(lost.cycle_open(), "the rows of this cycle never arrived");

        let mut landed = PullScheduler::new(true).holding_gate(lost.cycle_open());
        assert!(landed.cycle_open(), "the half-applied screen is still up");
        landed.on_frame(&GridMsg::RehydrateBegin);
        assert!(!landed.cycle_open(), "the burst redescribes the screen");
    }

    /// An eager-push landing is sent no marker, and nothing else would
    /// ever lift a carried gate.
    #[test]
    fn an_unmarked_landing_still_lifts_a_carried_gate_on_its_burst() {
        let mut landed = PullScheduler::new(false).holding_gate(true);
        assert!(landed.cycle_open());
        landed.on_frame(&GridMsg::RehydrateBegin);
        assert!(!landed.cycle_open(), "the burst lifts it on any peer");
    }

    #[test]
    fn only_the_marker_frees_the_next_pull() {
        let mut s = marked();
        assert!(s.take_due());
        assert!(!s.take_due());
        s.on_frame(&cycle_frame());
        assert!(!s.take_due(), "a cycle frame is not the whole cycle");
        s.on_frame(&GridMsg::CycleEnd);
        assert!(s.take_due(), "the marker frees the next one");
        assert!(!s.take_due());
    }

    #[test]
    fn multiple_frames_one_iteration_yield_one_pull() {
        let mut s = marked();
        assert!(s.take_due());
        s.on_frame(&cycle_frame());
        s.on_frame(&cycle_frame());
        s.on_frame(&cycle_frame());
        s.on_frame(&GridMsg::CycleEnd);
        assert!(s.take_due(), "one pull after the batch");
        assert!(!s.take_due(), "not one pull per frame");
    }

    #[test]
    fn a_cycle_stays_open_across_every_frame_it_carries() {
        let mut s = marked();
        assert!(s.take_due());
        assert!(!s.cycle_open(), "nothing has arrived yet");
        s.on_frame(&GridMsg::Scrolled {
            region_top: 0,
            region_bottom: 3,
            n_rows: 1,
            direction: ScrollDirection::Up,
        });
        assert!(s.cycle_open(), "the scroll directive opened the cycle");
        s.on_frame(&cycle_frame());
        s.on_frame(&GridMsg::CursorState {
            row: 0,
            col: 0,
            visible: true,
            style: CursorStyle::Block,
            blink: false,
        });
        assert!(s.cycle_open(), "the rows and cursor are the same cycle");
        s.on_frame(&GridMsg::CycleEnd);
        assert!(!s.cycle_open(), "the marker closed it");
    }

    /// An `Ops` subscriber's cycle can carry no rows at all and is
    /// bounded the same way.
    #[test]
    fn a_cycle_without_rows_is_still_bounded_by_the_marker() {
        let mut s = marked();
        assert!(s.take_due());
        s.on_frame(&GridMsg::PromptMark {
            line: 7,
            kind: PromptKind::PromptStart,
            exit_code: None,
        });
        assert!(s.cycle_open());
        assert!(!s.take_due());
        s.on_frame(&GridMsg::CycleEnd);
        assert!(!s.cycle_open());
        assert!(s.take_due());
    }

    #[test]
    fn a_frame_that_did_not_present_issues_no_pull() {
        let mut s = marked();
        assert!(!s.after_frame(false), "a timed-out acquire must not pull");
        assert!(
            !s.after_frame(false),
            "nor any retry that fails the same way"
        );
        assert_eq!(s.sent(), 0);
        assert!(s.after_frame(true), "the first presented frame pulls");
        assert!(!s.after_frame(true), "and keeps one pull outstanding");
    }

    const REFRESH: Duration = Duration::from_millis(16);

    #[test]
    fn an_unpresented_frame_arms_a_retry_one_refresh_interval_out() {
        let t0 = Instant::now();
        let mut r = PresentRetry::default();
        assert!(!r.stalled());
        r.unpresented(t0, REFRESH);
        assert!(r.stalled());
        assert_eq!(r.next_deadline(), Some(t0 + REFRESH));
        assert!(!r.take_due(t0), "no immediate retry");
        assert!(r.take_due(t0 + REFRESH));
        assert!(
            !r.take_due(t0 + REFRESH),
            "one retry per interval, not one per loop turn",
        );
        assert!(r.take_due(t0 + 2 * REFRESH), "still retrying while stalled");
    }

    #[test]
    fn between_retries_every_redraw_is_held() {
        let t0 = Instant::now();
        let mut r = PresentRetry::default();
        assert_eq!(r.turn(t0), RedrawTurn::Free, "a presenting window");
        r.unpresented(t0, REFRESH);
        assert_eq!(r.turn(t0 + REFRESH / 2), RedrawTurn::Hold);
        assert_eq!(r.turn(t0 + REFRESH), RedrawTurn::Retry);
        assert_eq!(r.turn(t0 + REFRESH), RedrawTurn::Hold);
        r.presented();
        assert_eq!(r.turn(t0 + REFRESH), RedrawTurn::Free);
    }

    #[test]
    fn each_failed_retry_doubles_the_wait_up_to_the_cap() {
        let t0 = Instant::now();
        let mut r = PresentRetry::default();
        let mut now = t0;
        let mut waits = Vec::new();
        for _ in 0..10 {
            r.unpresented(now, REFRESH);
            let at = r.next_deadline().expect("stalled");
            waits.push(at - now);
            now = at;
        }
        assert_eq!(waits[0], REFRESH);
        assert_eq!(waits[1], 2 * REFRESH);
        assert_eq!(waits[2], 4 * REFRESH);
        assert_eq!(waits[9], PresentRetry::MAX_INTERVAL);
    }

    #[test]
    fn a_visibility_change_makes_the_retry_due_at_once() {
        let t0 = Instant::now();
        let mut r = PresentRetry::default();
        for i in 0..5 {
            r.unpresented(t0 + i * REFRESH, REFRESH);
        }
        let visible_at = t0 + 5 * REFRESH;
        r.wake(visible_at);
        assert!(r.take_due(visible_at));
    }

    #[test]
    fn a_visibility_change_on_a_presenting_window_arms_nothing() {
        let t0 = Instant::now();
        let mut r = PresentRetry::default();
        r.wake(t0);
        assert!(!r.stalled());
        assert_eq!(r.next_deadline(), None);
    }

    #[test]
    fn a_presented_frame_ends_the_retries() {
        let t0 = Instant::now();
        let mut r = PresentRetry::default();
        r.unpresented(t0, REFRESH);
        r.presented();
        assert!(!r.stalled());
        assert!(!r.take_due(t0 + 10 * REFRESH));
    }

    /// The display sleeps mid-flood, then wakes: the stalled window
    /// sends nothing, and the first frame it presents sends exactly one
    /// pull, which the daemon answers with everything dirty since.
    #[test]
    fn a_stalled_window_resumes_pulling_with_one_pull_once_it_presents() {
        let t0 = Instant::now();
        let mut pull = marked();
        let mut retry = PresentRetry::default();
        assert!(pull.after_frame(true));
        pull.on_frame(&cycle_frame());
        pull.on_frame(&GridMsg::CycleEnd);

        for i in 0..3 {
            let now = t0 + i * REFRESH;
            assert!(!pull.after_frame(false));
            retry.unpresented(now, REFRESH);
        }
        assert_eq!(pull.sent(), 1, "no pull while the surface cannot present");

        let visible_at = t0 + 3 * REFRESH;
        retry.wake(visible_at);
        assert!(retry.take_due(visible_at), "the window redraws at once");
        retry.presented();
        assert!(pull.after_frame(true), "one pull catches up");
        assert!(!pull.after_frame(true));
        assert_eq!(pull.sent(), 2);
    }

    /// The burst carries rows but no `CycleEnd`; `RehydrateEnd` is what
    /// releases the paint, or the window would never paint again.
    #[test]
    fn the_rehydrate_burst_is_closed_by_its_own_boundary() {
        let mut s = marked();
        assert!(s.take_due());
        s.on_frame(&GridMsg::RehydrateBegin);
        s.on_frame(&cycle_frame());
        s.on_frame(&GridMsg::RehydrateEnd);
        assert!(!s.cycle_open(), "rehydrate ends at its own boundary");
    }
}

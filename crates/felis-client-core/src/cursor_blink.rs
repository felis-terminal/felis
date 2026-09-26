//! Client-side cursor-blink clock.
//!
//! The daemon forwards the blink flag from `DECSCUSR`; animation is client-side.
//! [`BlinkClock::next_deadline`] returns `None` when steady so idle windows
//! burn no CPU.

use std::time::{Duration, Instant};

use crate::config::CursorBlinkMode;

/// A `blink_interval_ms = 0` would otherwise wake the event loop as fast
/// as it can spin.
const MIN_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub struct BlinkClock {
    mode: CursorBlinkMode,
    /// Half-period.
    interval: Duration,
    on: bool,
    since: Instant,
}

impl BlinkClock {
    pub fn new(mode: CursorBlinkMode, interval_ms: u64, now: Instant) -> Self {
        Self {
            mode,
            interval: Duration::from_millis(interval_ms).max(MIN_INTERVAL),
            on: true,
            since: now,
        }
    }

    pub fn reconfigure(&mut self, mode: CursorBlinkMode, interval_ms: u64, now: Instant) {
        *self = Self::new(mode, interval_ms, now);
    }

    const fn active(&self, program_wants_blink: bool) -> bool {
        match self.mode {
            CursorBlinkMode::Never => false,
            CursorBlinkMode::Always => true,
            CursorBlinkMode::Program => program_wants_blink,
        }
    }

    /// Called on cursor movement and input: xterm / kitty restart the
    /// blink rather than leaving the caret mid-"off" where you just typed.
    pub const fn reset(&mut self, now: Instant) {
        self.on = true;
        self.since = now;
    }

    /// `true` when the visible phase changed and a repaint is owed.
    pub fn tick(&mut self, now: Instant, program_wants_blink: bool) -> bool {
        if !self.active(program_wants_blink) {
            if self.on {
                return false;
            }
            self.reset(now);
            return true;
        }
        if now.duration_since(self.since) < self.interval {
            return false;
        }
        self.on = !self.on;
        self.since = now;
        true
    }

    pub const fn visible(&self) -> bool {
        self.on
    }

    /// `None` when steady, so the loop parks on `ControlFlow::Wait`.
    pub fn next_deadline(&self, program_wants_blink: bool) -> Option<Instant> {
        self.active(program_wants_blink)
            .then(|| self.since + self.interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(mode: CursorBlinkMode) -> (BlinkClock, Instant) {
        let now = Instant::now();
        (BlinkClock::new(mode, 500, now), now)
    }

    #[test]
    fn never_mode_stays_solid_on_with_no_wakeup() {
        let (mut c, t0) = clock(CursorBlinkMode::Never);
        assert!(!c.tick(t0 + Duration::from_secs(10), true));
        assert!(c.visible());
        assert!(c.next_deadline(true).is_none(), "steady ⇒ no timer");
    }

    #[test]
    fn always_mode_toggles_each_interval_even_for_steady_request() {
        let (mut c, t0) = clock(CursorBlinkMode::Always);
        assert!(
            !c.tick(t0 + Duration::from_millis(499), false),
            "before period"
        );
        assert!(
            c.tick(t0 + Duration::from_millis(500), false),
            "first toggle"
        );
        assert!(!c.visible(), "now in the off phase");
        assert!(c.next_deadline(false).is_some(), "blink ⇒ timer armed");
    }

    #[test]
    fn program_mode_follows_the_decscusr_request() {
        let (mut c, t0) = clock(CursorBlinkMode::Program);
        assert!(!c.tick(t0 + Duration::from_secs(5), false));
        assert!(c.visible());
        assert!(c.next_deadline(false).is_none());
        assert!(c.tick(t0 + Duration::from_secs(5), true));
        assert!(!c.visible());
        assert!(c.next_deadline(true).is_some());
    }

    #[test]
    fn reset_restores_solid_on_after_input() {
        let (mut c, t0) = clock(CursorBlinkMode::Always);
        assert!(c.tick(t0 + Duration::from_millis(500), true));
        assert!(!c.visible(), "off phase");
        c.reset(t0 + Duration::from_millis(600));
        assert!(c.visible());
        assert!(
            !c.tick(t0 + Duration::from_secs(1), true),
            "reset re-armed the period"
        );
        assert!(c.tick(t0 + Duration::from_millis(1100), true));
    }

    #[test]
    fn switching_off_mid_off_phase_snaps_back_on() {
        let (mut c, t0) = clock(CursorBlinkMode::Program);
        assert!(
            c.tick(t0 + Duration::from_millis(500), true),
            "toggle to off"
        );
        assert!(!c.visible());
        assert!(c.tick(t0 + Duration::from_millis(600), false));
        assert!(c.visible());
    }

    #[test]
    fn reconfigure_adopts_the_new_mode_and_restarts_the_phase() {
        let (mut c, t0) = clock(CursorBlinkMode::Always);
        assert!(c.tick(t0 + Duration::from_millis(500), true), "off phase");
        assert!(!c.visible());

        let reload = t0 + Duration::from_millis(700);
        c.reconfigure(CursorBlinkMode::Never, 500, reload);
        assert!(c.visible(), "reconfigure restarts solid-on");
        assert!(
            c.next_deadline(true).is_none(),
            "the new `never` mode governs, even with the program asking to blink",
        );
        assert!(!c.tick(reload + Duration::from_secs(10), true));
    }

    #[test]
    fn zero_interval_is_floored_not_busy_looping() {
        let now = Instant::now();
        let mut c = BlinkClock::new(CursorBlinkMode::Always, 0, now);
        assert!(!c.tick(now + Duration::from_millis(10), true));
        assert!(c.tick(now + MIN_INTERVAL, true));
    }
}

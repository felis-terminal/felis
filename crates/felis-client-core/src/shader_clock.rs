//! Frame clock for a self-animating post-process shader, opt-in because
//! a shader animating off `time_s` has no observable end for the host to
//! stop at (docs/explanation/rendering/pipeline.md "Idle-zero under
//! animation"). A deadline is reported only while a shader is loaded and
//! the window holds focus, so a background window never animates.

use std::time::{Duration, Instant};

use crate::config::ShaderAnimation;

/// 60 Hz rather than the panel's refresh: winit exposes no portable
/// refresh rate.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug)]
pub struct ShaderClock {
    mode: ShaderAnimation,
    last: Instant,
}

impl ShaderClock {
    pub const fn new(mode: ShaderAnimation, now: Instant) -> Self {
        Self { mode, last: now }
    }

    pub const fn set_mode(&mut self, mode: ShaderAnimation) {
        self.mode = mode;
    }

    /// `active`: a shader is loaded and the window is focused.
    const fn running(&self, active: bool) -> bool {
        match self.mode {
            ShaderAnimation::Never => false,
            ShaderAnimation::Focused => active,
        }
    }

    pub fn tick(&mut self, now: Instant, active: bool) -> bool {
        if !self.running(active) {
            // Held at the present so a re-focus owes one frame, not a
            // backlog.
            self.last = now;
            return false;
        }
        if now.saturating_duration_since(self.last) < FRAME_INTERVAL {
            return false;
        }
        self.last = now;
        true
    }

    pub fn next_deadline(&self, active: bool) -> Option<Instant> {
        self.running(active).then(|| self.last + FRAME_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_mode_never_wakes_the_loop() {
        let t0 = Instant::now();
        let mut c = ShaderClock::new(ShaderAnimation::default(), t0);
        assert!(!c.tick(t0 + Duration::from_secs(10), true));
        assert!(
            c.next_deadline(true).is_none(),
            "off ⇒ no timer, even with a shader loaded and focus held",
        );
    }

    #[test]
    fn focused_mode_draws_at_the_frame_interval() {
        let t0 = Instant::now();
        let mut c = ShaderClock::new(ShaderAnimation::Focused, t0);
        assert!(!c.tick(t0 + Duration::from_millis(15), true), "before due");
        assert!(c.tick(t0 + FRAME_INTERVAL, true), "first frame");
        assert!(
            !c.tick(t0 + FRAME_INTERVAL, true),
            "same instant, once only"
        );
        assert!(c.next_deadline(true).is_some());
    }

    #[test]
    fn an_inactive_window_arms_nothing() {
        let t0 = Instant::now();
        let mut c = ShaderClock::new(ShaderAnimation::Focused, t0);
        assert!(!c.tick(t0 + Duration::from_secs(1), false));
        assert!(c.next_deadline(false).is_none());
    }

    #[test]
    fn set_mode_starts_the_clock_on_a_live_reload() {
        let t0 = Instant::now();
        let mut c = ShaderClock::new(ShaderAnimation::Never, t0);
        assert!(!c.tick(t0 + Duration::from_secs(1), true));
        assert!(c.next_deadline(true).is_none());

        let reload = t0 + Duration::from_secs(1);
        c.set_mode(ShaderAnimation::Focused);
        assert_eq!(c.next_deadline(true), Some(reload + FRAME_INTERVAL));
        assert!(!c.tick(reload, true), "not owed at the reload instant");
        assert!(c.tick(reload + FRAME_INTERVAL, true));
    }

    #[test]
    fn set_mode_parks_the_loop_when_animation_is_turned_off() {
        let t0 = Instant::now();
        let mut c = ShaderClock::new(ShaderAnimation::Focused, t0);
        assert!(c.tick(t0 + FRAME_INTERVAL, true));

        c.set_mode(ShaderAnimation::Never);
        assert!(c.next_deadline(true).is_none());
        assert!(!c.tick(t0 + Duration::from_secs(10), true));
    }

    #[test]
    fn refocusing_owes_a_frame_at_once() {
        let t0 = Instant::now();
        let mut c = ShaderClock::new(ShaderAnimation::Focused, t0);
        assert!(!c.tick(t0 + Duration::from_secs(60), false));
        let back = t0 + Duration::from_secs(60);
        assert!(!c.tick(back, true), "not instantly, but re-armed from now");
        assert!(c.tick(back + FRAME_INTERVAL, true));
    }
}

//! Redraw-request coalescer: handlers call [`RedrawScheduler::request`];
//! `about_to_wait` calls [`RedrawScheduler::flush`] once and issues the
//! real `window.request_redraw()` there.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RedrawStats {
    pub requested: u64,
    /// Flushes that returned `true`: the `window.request_redraw()` calls
    /// actually issued.
    pub flushed: u64,
}

#[derive(Debug, Default)]
pub struct RedrawScheduler {
    pending: bool,
    stats: RedrawStats,
}

impl RedrawScheduler {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: false,
            stats: RedrawStats {
                requested: 0,
                flushed: 0,
            },
        }
    }

    pub const fn request(&mut self) {
        self.pending = true;
        self.stats.requested = self.stats.requested.saturating_add(1);
    }

    /// `true` exactly when a redraw should be issued.
    #[must_use]
    pub const fn flush(&mut self) -> bool {
        if !self.pending {
            return false;
        }
        self.pending = false;
        self.stats.flushed = self.stats.flushed.saturating_add(1);
        true
    }

    /// A paint put every state requested so far on screen. Without
    /// dropping the pending request, a frame that arrived between the
    /// flush and the paint schedules a second paint of the same grid,
    /// which blocks on the next vsync ahead of the frame that follows.
    pub const fn painted(&mut self) {
        self.pending = false;
    }

    /// Cumulative since `new()`, for the `--trace-perf` event.
    #[must_use]
    pub const fn stats(&self) -> RedrawStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_requests_then_one_flush_returns_true_once() {
        let mut s = RedrawScheduler::new();
        for _ in 0..10 {
            s.request();
        }
        assert!(s.flush(), "first flush after requests must redraw");
        assert!(
            !s.flush(),
            "second flush with nothing pending must not redraw"
        );
        assert_eq!(s.stats().requested, 10);
        assert_eq!(s.stats().flushed, 1);
    }

    #[test]
    fn a_paint_satisfies_the_requests_made_before_it() {
        let mut s = RedrawScheduler::new();
        s.request();
        assert!(s.flush());
        s.request();
        s.painted();
        assert!(!s.flush(), "the paint already showed the second request");
        s.request();
        assert!(s.flush(), "a request after the paint still redraws");
    }

    #[test]
    fn flush_without_request_returns_false() {
        let mut s = RedrawScheduler::new();
        assert!(!s.flush());
        assert_eq!(s.stats().flushed, 0);
    }

    #[test]
    fn alternating_request_flush_pairs_each_produce_one_redraw() {
        let mut s = RedrawScheduler::new();
        for _ in 0..5 {
            s.request();
            assert!(s.flush());
        }
        assert_eq!(s.stats().requested, 5);
        assert_eq!(s.stats().flushed, 5);
    }

    #[test]
    fn empty_flush_does_not_bump_flushed_counter() {
        let mut s = RedrawScheduler::new();
        let _ = s.flush();
        let _ = s.flush();
        assert_eq!(s.stats().flushed, 0);
        s.request();
        let _ = s.flush();
        assert_eq!(s.stats().flushed, 1);
    }
}

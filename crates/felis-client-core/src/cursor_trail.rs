//! Client-side cursor-trail easing clock.
//!
//! Eases corners on the CPU so the timer stops upon arrival and idle windows
//! return to zero redraws (docs/explanation/rendering/pipeline.md). Follows
//! per-corner exponential decay like kitty's `kitty/cursor_trail.c`.

use std::time::{Duration, Instant};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Seconds for a leading / trailing corner to close 1023/1024 of its
/// distance.
const DECAY_FAST_S: f32 = 0.1;
const DECAY_SLOW_S: f32 = 0.4;

/// In cells; below this no trail starts, so typing one cell at a time
/// never arms the frame timer. Matches kitty's
/// `cursor_trail_start_threshold`.
const START_THRESHOLD_CELLS: f32 = 2.0;

/// Corner order 0 top-right, 1 bottom-right, 2 bottom-left, 3 top-left:
/// the shader contract's order.
const CORNER_X_IS_RIGHT: [bool; 4] = [true, true, false, false];
const CORNER_Y_IS_BOTTOM: [bool; 4] = [false, true, true, false];

/// Physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrailMetrics {
    pub viewport_px: [f32; 2],
    pub cell_px: [f32; 2],
}

/// In UV with the origin top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrailGeometry {
    /// `(x, y, width, height)`.
    pub cursor_rect: [f32; 4],
    pub prev_cursor_rect: [f32; 4],
    pub corners_x: [f32; 4],
    pub corners_y: [f32; 4],
    pub seconds_since_change: f32,
}

#[derive(Debug)]
pub struct TrailClock {
    /// UV, `[x, y]` each.
    corners: [[f32; 2]; 4],
    cursor_rect: [f32; 4],
    prev_cursor_rect: [f32; 4],
    changed_at: Instant,
    updated_at: Instant,
    animating: bool,
    seeded: bool,
}

impl TrailClock {
    pub const fn new(now: Instant) -> Self {
        Self {
            corners: [[0.0; 2]; 4],
            cursor_rect: [0.0; 4],
            prev_cursor_rect: [0.0; 4],
            changed_at: now,
            updated_at: now,
            animating: false,
            seeded: false,
        }
    }

    /// `target` is the cursor rectangle in UV, `None` when no cursor is
    /// painted; the corners then snap so the trail cannot reappear from a
    /// stale position. Returns whether a repaint is owed; the frame after
    /// the corners settle still repaints, painting the collapsed quad away.
    pub fn update(
        &mut self,
        now: Instant,
        target: Option<[f32; 4]>,
        metrics: TrailMetrics,
    ) -> bool {
        let was_animating = self.animating;
        let Some(target) = target else {
            self.snap_to(self.cursor_rect);
            self.animating = false;
            self.updated_at = now;
            return was_animating;
        };

        if !self.seeded {
            self.seeded = true;
            self.cursor_rect = target;
            self.prev_cursor_rect = target;
            self.snap_to(target);
            self.updated_at = now;
            self.changed_at = now;
            return false;
        }

        if rect_moved(target, self.cursor_rect) {
            self.prev_cursor_rect = self.cursor_rect;
            self.cursor_rect = target;
            self.changed_at = now;
        }

        if self.animating || self.moved_far_enough(metrics) {
            let dt = now.saturating_duration_since(self.updated_at).as_secs_f32();
            self.ease(dt);
        } else {
            self.snap_to(self.cursor_rect);
        }
        self.updated_at = now;
        self.animating = self.any_corner_off(metrics);
        self.animating || was_animating
    }

    pub fn geometry(&self, now: Instant) -> TrailGeometry {
        TrailGeometry {
            cursor_rect: self.cursor_rect,
            prev_cursor_rect: self.prev_cursor_rect,
            corners_x: [
                self.corners[0][0],
                self.corners[1][0],
                self.corners[2][0],
                self.corners[3][0],
            ],
            corners_y: [
                self.corners[0][1],
                self.corners[1][1],
                self.corners[2][1],
                self.corners[3][1],
            ],
            seconds_since_change: now.saturating_duration_since(self.changed_at).as_secs_f32(),
        }
    }

    pub const fn animating(&self) -> bool {
        self.animating
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.animating.then(|| self.updated_at + FRAME_INTERVAL)
    }

    fn corner_target(&self, i: usize) -> [f32; 2] {
        let [left, top, width, height] = self.cursor_rect;
        [
            if CORNER_X_IS_RIGHT[i] {
                left + width
            } else {
                left
            },
            if CORNER_Y_IS_BOTTOM[i] {
                top + height
            } else {
                top
            },
        ]
    }

    fn snap_to(&mut self, rect: [f32; 4]) {
        self.cursor_rect = rect;
        for i in 0..4 {
            self.corners[i] = self.corner_target(i);
        }
    }

    fn moved_far_enough(&self, metrics: TrailMetrics) -> bool {
        let cell_uv = [
            metrics.cell_px[0] / metrics.viewport_px[0].max(1.0),
            metrics.cell_px[1] / metrics.viewport_px[1].max(1.0),
        ];
        (0..4).any(|i| {
            let target = self.corner_target(i);
            let dx = (target[0] - self.corners[i][0]).abs();
            let dy = (target[1] - self.corners[i][1]).abs();
            dx >= cell_uv[0] * START_THRESHOLD_CELLS || dy >= cell_uv[1] * START_THRESHOLD_CELLS
        })
    }

    /// Half a physical pixel is the floor below which further frames
    /// change nothing on screen.
    fn any_corner_off(&self, metrics: TrailMetrics) -> bool {
        let threshold = [
            0.5 / metrics.viewport_px[0].max(1.0),
            0.5 / metrics.viewport_px[1].max(1.0),
        ];
        (0..4).any(|i| {
            let target = self.corner_target(i);
            (target[0] - self.corners[i][0]).abs() >= threshold[0]
                || (target[1] - self.corners[i][1]).abs() >= threshold[1]
        })
    }

    fn ease(&mut self, dt: f32) {
        if dt <= 0.0 {
            return;
        }
        let [left, top, width, height] = self.cursor_rect;
        let center = [width.mul_add(0.5, left), height.mul_add(0.5, top)];
        let half_diagonal = width.hypot(height) * 0.5;
        if half_diagonal <= 0.0 {
            return;
        }

        // A corner pushed outward from the cursor center is on the
        // leading edge and closes fast; one pulled in trails.
        let mut delta = [[0.0_f32; 2]; 4];
        let mut facing = [0.0_f32; 4];
        let mut moving = [false; 4];
        for i in 0..4 {
            let target = self.corner_target(i);
            let d = [
                target[0] - self.corners[i][0],
                target[1] - self.corners[i][1],
            ];
            let len = d[0].hypot(d[1]);
            if len < 1e-6 {
                continue;
            }
            delta[i] = d;
            moving[i] = true;
            facing[i] = d[0].mul_add(target[0] - center[0], d[1] * (target[1] - center[1]))
                / half_diagonal
                / len;
        }
        let live: Vec<usize> = (0..4).filter(|&i| moving[i]).collect();
        if live.is_empty() {
            return;
        }
        let min_facing = live.iter().map(|&i| facing[i]).fold(f32::MAX, f32::min);
        let max_facing = live.iter().map(|&i| facing[i]).fold(f32::MIN, f32::max);

        for &i in &live {
            let decay = if (max_facing - min_facing).abs() < f32::EPSILON {
                DECAY_SLOW_S
            } else {
                DECAY_SLOW_S
                    + (DECAY_FAST_S - DECAY_SLOW_S) * (facing[i] - min_facing)
                        / (max_facing - min_facing)
            };
            let step = 1.0 - (-10.0 * dt / decay).exp2();
            self.corners[i][0] = delta[i][0].mul_add(step, self.corners[i][0]);
            self.corners[i][1] = delta[i][1].mul_add(step, self.corners[i][1]);
        }
    }
}

#[expect(
    clippy::float_cmp,
    reason = "both rectangles are recomputed from the same integers; a tolerance would swallow a one-pixel move"
)]
fn rect_moved(a: [f32; 4], b: [f32; 4]) -> bool {
    a != b
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;

    fn metrics() -> TrailMetrics {
        TrailMetrics {
            viewport_px: [800.0, 600.0],
            cell_px: [10.0, 20.0],
        }
    }

    fn cell_rect(row: f32, col: f32) -> [f32; 4] {
        [
            col * 10.0 / 800.0,
            row * 20.0 / 600.0,
            10.0 / 800.0,
            20.0 / 600.0,
        ]
    }

    #[test]
    fn first_cursor_sighting_starts_settled() {
        let t0 = Instant::now();
        let mut c = TrailClock::new(t0);
        assert!(!c.update(t0, Some(cell_rect(0.0, 0.0)), metrics()));
        assert!(!c.animating(), "the first frame must not animate");
        assert!(c.next_deadline().is_none(), "settled ⇒ no timer");
    }

    #[test]
    fn typing_one_cell_at_a_time_arms_no_timer() {
        let t0 = Instant::now();
        let mut c = TrailClock::new(t0);
        c.update(t0, Some(cell_rect(0.0, 0.0)), metrics());
        for step in 1..=6 {
            let now = t0 + Duration::from_millis(step * 40);
            let repaint = c.update(now, Some(cell_rect(0.0, step as f32)), metrics());
            assert!(!repaint, "a one-cell move must not start a trail");
            assert!(c.next_deadline().is_none());
        }
    }

    #[test]
    fn the_frame_after_settling_still_repaints() {
        let t0 = Instant::now();
        let mut c = TrailClock::new(t0);
        c.update(t0, Some(cell_rect(0.0, 0.0)), metrics());
        let target = cell_rect(20.0, 40.0);
        let mut now = t0;
        loop {
            now += FRAME_INTERVAL;
            let repaint = c.update(now, Some(target), metrics());
            if !c.animating() {
                assert!(repaint);
                break;
            }
            assert!(repaint);
        }
    }

    fn corner_targets(rect: [f32; 4]) -> ([f32; 4], [f32; 4]) {
        let [left, top, width, height] = rect;
        (
            [left + width, left + width, left, left],
            [top, top + height, top + height, top],
        )
    }

    proptest::proptest! {
        /// Exponential decay only settles if every corner closes on its
        /// target and never passes it; overshoot would leave the quad
        /// oscillating, and the frame timer armed, over an idle window.
        #[test]
        fn every_corner_closes_on_its_target_without_overshooting(
            row in 0.0_f32..=24.0,
            col in 0.0_f32..=80.0,
        ) {
            let t0 = Instant::now();
            let mut clock = TrailClock::new(t0);
            clock.update(t0, Some(cell_rect(0.0, 0.0)), metrics());
            let target = cell_rect(row, col);
            let (target_x, target_y) = corner_targets(target);

            let mut now = t0;
            let mut previous = [[f32::MAX; 2]; 4];
            let mut frames = 0_u32;
            let gap = loop {
                now += FRAME_INTERVAL;
                clock.update(now, Some(target), metrics());
                let geometry = clock.geometry(now);
                let mut gap = 0.0_f32;
                for i in 0..4 {
                    for (axis, (corner, goal)) in [
                        (geometry.corners_x[i], target_x[i]),
                        (geometry.corners_y[i], target_y[i]),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let remaining = goal - corner;
                        gap = gap.max(remaining.abs());
                        let before = previous[i][axis];
                        proptest::prop_assert!(
                            remaining.abs() <= before.abs() + 1e-6,
                            "corner {} axis {} moved away: {} after {}",
                            i,
                            axis,
                            remaining,
                            before,
                        );
                        proptest::prop_assert!(
                            remaining.abs() < 1e-7
                                || before == f32::MAX
                                || remaining.signum() == before.signum(),
                            "corner {} axis {} passed its target: {} after {}",
                            i,
                            axis,
                            remaining,
                            before,
                        );
                        previous[i][axis] = remaining;
                    }
                }
                frames += 1;
                if !clock.animating() {
                    break gap;
                }
                proptest::prop_assert!(frames < 400, "the trail never settled");
            };
            // Half a physical pixel on the shorter viewport axis.
            proptest::prop_assert!(gap < 0.5 / 600.0, "settled {} short of the cursor", gap);
        }
    }

    #[test]
    fn a_hidden_cursor_snaps_the_trail_away() {
        let t0 = Instant::now();
        let mut c = TrailClock::new(t0);
        c.update(t0, Some(cell_rect(0.0, 0.0)), metrics());
        c.update(t0 + FRAME_INTERVAL, Some(cell_rect(20.0, 40.0)), metrics());
        assert!(c.animating());
        let repaint = c.update(t0 + FRAME_INTERVAL * 2, None, metrics());
        assert!(repaint, "the collapse itself needs one paint");
        assert!(!c.animating());
        assert!(c.next_deadline().is_none());
        let g = c.geometry(t0 + FRAME_INTERVAL * 2);
        assert!((g.corners_x[2] - g.cursor_rect[0]).abs() < 1e-6);
    }

    #[test]
    fn the_previous_rectangle_tracks_the_last_move() {
        let t0 = Instant::now();
        let mut c = TrailClock::new(t0);
        let first = cell_rect(0.0, 0.0);
        let second = cell_rect(5.0, 5.0);
        c.update(t0, Some(first), metrics());
        c.update(t0 + FRAME_INTERVAL, Some(second), metrics());
        let g = c.geometry(t0 + FRAME_INTERVAL);
        assert_eq!(g.prev_cursor_rect, first);
        assert_eq!(g.cursor_rect, second);
        assert!(g.seconds_since_change < 0.001, "the move just happened");
    }
}

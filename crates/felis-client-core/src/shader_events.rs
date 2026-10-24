//! Frame clock for event-bounded post-stage animations. Each run has an end
//! the host knows, so the clock wakes the loop only until then plus one final
//! frame, focused or not (docs/explanation/rendering/pipeline.md "Idle-zero
//! under animation").

use std::time::{Duration, Instant};

use crate::config::{ShaderEvent, StageAnimation};
use crate::shader_clock::FRAME_INTERVAL;

#[derive(Debug)]
pub struct StageAnimations {
    /// Indexed like the chain the renderer was handed.
    stages: Vec<Option<StageAnimation>>,
    /// When each stage's current run ends.
    ends: Vec<Option<Instant>>,
    last_frame: Instant,
}

/// One stage to (re)start, for the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationStart {
    pub stage: usize,
    pub duration: Duration,
}

impl StageAnimations {
    #[must_use]
    pub fn new(stages: Vec<Option<StageAnimation>>, now: Instant) -> Self {
        let ends = vec![None; stages.len()];
        Self {
            stages,
            ends,
            last_frame: now,
        }
    }

    /// Restarts every stage listening for `event`, a run already in flight
    /// included.
    pub fn fire(&mut self, event: &ShaderEvent, now: Instant) -> Vec<AnimationStart> {
        let mut started = Vec::new();
        for (stage, (animation, end)) in self.stages.iter().zip(&mut self.ends).enumerate() {
            let Some(animation) = animation else {
                continue;
            };
            if !animation.on.contains(event) {
                continue;
            }
            let duration = Duration::from_millis(u64::from(animation.duration_ms));
            *end = Some(now + duration);
            started.push(AnimationStart { stage, duration });
        }
        if !started.is_empty() {
            self.last_frame = now;
        }
        started
    }

    /// Swaps in the animation tables of a reload that kept the chain's
    /// shaders, so the renderer kept every stage's run. A stage whose table
    /// changed loses its run; the returned stages are the ones the
    /// renderer must stop too, or it would keep drawing a run nothing
    /// wakes the loop for.
    pub fn reconfigure(&mut self, stages: Vec<Option<StageAnimation>>) -> Vec<usize> {
        let mut stopped = Vec::new();
        let mut ends = vec![None; stages.len()];
        for (stage, end) in self.ends.iter().enumerate() {
            let Some(end) = *end else {
                continue;
            };
            match (stages.get(stage), self.stages.get(stage)) {
                (Some(new), Some(old)) if new == old => ends[stage] = Some(end),
                _ => stopped.push(stage),
            }
        }
        self.stages = stages;
        self.ends = ends;
        stopped
    }

    /// For a stage the renderer did not load: nothing would draw its run.
    pub fn cancel(&mut self, stage: usize) {
        if let Some(end) = self.ends.get_mut(stage) {
            *end = None;
        }
    }

    /// Whether a frame is owed now. A run past its end owes one last
    /// frame, the one that shows progress 1, and then stops.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut owed = false;
        let mut running = false;
        for end in &mut self.ends {
            match *end {
                Some(at) if now >= at => {
                    *end = None;
                    owed = true;
                }
                Some(_) => running = true,
                None => {}
            }
        }
        if running && now.saturating_duration_since(self.last_frame) >= FRAME_INTERVAL {
            owed = true;
        }
        if owed {
            self.last_frame = now;
        }
        owed
    }

    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let next_end = self.ends.iter().flatten().min().copied()?;
        Some(next_end.min(self.last_frame + FRAME_INTERVAL))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn flash(on: Vec<ShaderEvent>, duration_ms: u32) -> StageAnimation {
        StageAnimation { on, duration_ms }
    }

    #[test]
    fn no_event_no_wakeup() {
        let t0 = Instant::now();
        let mut clock = StageAnimations::new(vec![Some(flash(vec![ShaderEvent::Bell], 300))], t0);
        assert_eq!(clock.next_deadline(), None);
        assert!(!clock.tick(t0 + Duration::from_secs(5)));
    }

    #[test]
    fn an_event_starts_only_the_stages_that_listen_for_it() {
        let t0 = Instant::now();
        let mut clock = StageAnimations::new(
            vec![
                Some(flash(vec![ShaderEvent::Bell], 300)),
                None,
                Some(flash(vec![ShaderEvent::FocusIn, ShaderEvent::Bell], 100)),
                Some(flash(vec![ShaderEvent::User("glow".to_owned())], 200)),
            ],
            t0,
        );
        assert_eq!(
            clock.fire(&ShaderEvent::Bell, t0),
            [
                AnimationStart {
                    stage: 0,
                    duration: Duration::from_millis(300)
                },
                AnimationStart {
                    stage: 2,
                    duration: Duration::from_millis(100)
                },
            ]
        );
        assert_eq!(clock.fire(&ShaderEvent::User("other".to_owned()), t0), []);
    }

    #[test]
    fn a_run_draws_at_the_frame_interval_then_one_final_frame_then_stops() {
        let t0 = Instant::now();
        let mut clock = StageAnimations::new(vec![Some(flash(vec![ShaderEvent::Bell], 40))], t0);
        clock.fire(&ShaderEvent::Bell, t0);
        assert_eq!(clock.next_deadline(), Some(t0 + FRAME_INTERVAL));
        assert!(!clock.tick(t0 + Duration::from_millis(5)), "before due");
        assert!(clock.tick(t0 + FRAME_INTERVAL));
        assert!(clock.tick(t0 + FRAME_INTERVAL * 2));
        let end = t0 + Duration::from_millis(40);
        assert_eq!(
            clock.next_deadline(),
            Some(end),
            "the end is the next wake-up"
        );
        assert!(clock.tick(end), "the final frame shows progress 1");
        assert_eq!(clock.next_deadline(), None);
        assert!(!clock.tick(end + Duration::from_secs(1)), "then nothing");
    }

    #[test]
    fn a_retrigger_while_running_restarts_the_run() {
        let t0 = Instant::now();
        let mut clock = StageAnimations::new(vec![Some(flash(vec![ShaderEvent::Bell], 100))], t0);
        clock.fire(&ShaderEvent::Bell, t0);
        let again = t0 + Duration::from_millis(80);
        clock.fire(&ShaderEvent::Bell, again);
        assert!(
            clock.tick(t0 + Duration::from_millis(100)),
            "a frame, not the end"
        );
        assert!(
            clock.next_deadline().is_some(),
            "the run now ends 100 ms after the retrigger"
        );
        assert!(clock.tick(again + Duration::from_millis(100)));
        assert_eq!(clock.next_deadline(), None);
    }

    #[test]
    fn reconfiguring_with_unchanged_tables_keeps_a_running_stage_and_its_deadline() {
        let t0 = Instant::now();
        let tables = vec![Some(flash(vec![ShaderEvent::Bell], 300)), None];
        let mut clock = StageAnimations::new(tables.clone(), t0);
        clock.fire(&ShaderEvent::Bell, t0);
        let deadline = clock.next_deadline();
        assert_eq!(clock.reconfigure(tables), Vec::<usize>::new());
        assert_eq!(clock.next_deadline(), deadline);
        assert!(
            clock.tick(t0 + Duration::from_millis(300)),
            "the final frame is still owed"
        );
    }

    #[test]
    fn reconfiguring_stops_only_the_running_stages_whose_table_changed_or_went_away() {
        let t0 = Instant::now();
        let mut clock = StageAnimations::new(
            vec![
                Some(flash(vec![ShaderEvent::Bell], 300)),
                Some(flash(vec![ShaderEvent::Bell], 300)),
                Some(flash(vec![ShaderEvent::Bell], 300)),
                Some(flash(vec![ShaderEvent::FocusIn], 300)),
            ],
            t0,
        );
        clock.fire(&ShaderEvent::Bell, t0);
        let stopped = clock.reconfigure(vec![
            Some(flash(vec![ShaderEvent::Bell], 300)),
            Some(flash(vec![ShaderEvent::Bell], 500)),
            None,
            Some(flash(vec![ShaderEvent::Bell], 300)),
        ]);
        assert_eq!(stopped, [1, 2], "stage 3 changed but was not running");
        assert_eq!(clock.next_deadline(), Some(t0 + FRAME_INTERVAL));
        assert!(clock.tick(t0 + Duration::from_millis(300)));
        assert_eq!(clock.next_deadline(), None, "only stage 0 was left running");
    }

    #[test]
    fn reconfiguring_to_fewer_stages_stops_the_running_ones_cut_off() {
        let t0 = Instant::now();
        let table = Some(flash(vec![ShaderEvent::Bell], 300));
        let mut clock = StageAnimations::new(vec![table.clone(), table.clone()], t0);
        clock.fire(&ShaderEvent::Bell, t0);
        assert_eq!(clock.reconfigure(vec![table]), [1]);
    }

    #[test]
    fn a_cancelled_stage_owes_no_frames() {
        let t0 = Instant::now();
        let mut clock = StageAnimations::new(vec![Some(flash(vec![ShaderEvent::Bell], 300))], t0);
        clock.fire(&ShaderEvent::Bell, t0);
        clock.cancel(0);
        assert_eq!(clock.next_deadline(), None);
        assert!(!clock.tick(t0 + Duration::from_secs(1)));
    }
}

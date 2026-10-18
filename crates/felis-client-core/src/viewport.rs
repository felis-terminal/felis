//! Composed-view viewport math (REQ-608 line indexing: `-1` is the
//! youngest scrollback row, `-N` older, `>= 0` the live grid). Must
//! mirror the daemon's `Grid::cell_at_viewport` composition, or search
//! highlights paint on the wrong rows.

use felis_protocol::messages::{InputMsg, ViewportAnchor};
use felis_protocol::minor::MinorGated as _;

/// The `lines_from_bottom` offset that centers a line index where
/// possible. `max_scroll` is `viewport_max - rows`, the deepest offset the
/// daemon accepts. A live-grid hit collapses to `0`: jumping to it means
/// snapping out of browse.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn viewport_for_hit(line_index: i64, rows: u16, max_scroll: u32) -> u32 {
    if line_index >= 0 {
        return 0;
    }
    let target = i64::from(rows) / 2 - line_index;
    target.clamp(0, i64::from(max_scroll)) as u32
}

/// `composed_row = line_index + viewport`, mirroring the daemon's
/// `Grid::cell_at_viewport`. A `rows - 1 + …` form (scrollback filling
/// the window, youngest at the bottom) disagrees with the daemon by
/// `rows - 1` rows.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn composed_row_for_line(line_index: i64, viewport: u32, rows: u16) -> Option<u16> {
    let r = line_index + i64::from(viewport);
    if r < 0 || r >= i64::from(rows) {
        return None;
    }
    Some(r as u16)
}

/// Whether the window's latest viewport request targets the live
/// bottom, which the shadow only learns once the reply lands. Stale only
/// toward "browsing": a return to live the daemon made on its own costs
/// one redundant `Viewport(0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportIntent {
    live: bool,
}

impl Default for ViewportIntent {
    /// A fresh attachment presents the live bottom.
    fn default() -> Self {
        Self { live: true }
    }
}

impl ViewportIntent {
    /// Records an outgoing input. A step toward live leaves the intent
    /// as it was: only the reply says whether it reached the bottom.
    pub const fn note(&mut self, msg: &InputMsg) {
        match msg {
            InputMsg::Viewport {
                lines_from_bottom, ..
            } => self.live = *lines_from_bottom == 0,
            InputMsg::ViewportStep { rows } if *rows > 0 => self.live = false,
            InputMsg::JumpPrompt { .. } => self.live = false,
            _ => {}
        }
    }

    /// Live both on screen and in every request still in flight.
    #[must_use]
    pub const fn is_live(self, shown_offset: u32) -> bool {
        shown_offset == 0 && self.live
    }
}

/// The request that scrolls `rows` toward history. Against a daemon that
/// takes `ViewportStep` the daemon applies it to wherever its view is,
/// so steps sent before a reply lands accumulate; an older daemon gets
/// an absolute offset computed from the shadow, `None` when that would
/// not move.
#[must_use]
pub fn step_request(
    rows: i32,
    effective_minor: u16,
    shown: u32,
    shown_anchor: Option<ViewportAnchor>,
    max_scroll: u32,
) -> Option<InputMsg> {
    if rows == 0 {
        return None;
    }
    let step = InputMsg::ViewportStep { rows };
    if step.requires().minor <= effective_minor {
        return Some(step);
    }
    let target =
        u32::try_from((i64::from(shown) + i64::from(rows)).clamp(0, i64::from(max_scroll)))
            .unwrap_or(max_scroll);
    (target != shown).then_some(InputMsg::Viewport {
        lines_from_bottom: target,
        anchor: shown_anchor.filter(|_| target > 0),
    })
}

#[cfg(test)]
mod tests {
    use felis_protocol::messages::PromptJump;

    use super::*;

    const STEPS: u16 = 2;
    const NO_STEPS: u16 = 1;

    /// End then `LineUp` before End's reply: the shadow still shows 5, and
    /// the step must not become the absolute `6` that would undo End.
    #[test]
    fn a_step_is_sent_as_a_step_whatever_the_shadow_shows() {
        assert_eq!(
            step_request(1, STEPS, 5, None, 100),
            Some(InputMsg::ViewportStep { rows: 1 })
        );
    }

    #[test]
    fn two_quick_steps_are_two_requests() {
        let shown = 7;
        let first = step_request(1, STEPS, shown, None, 100);
        let second = step_request(1, STEPS, shown, None, 100);
        assert_eq!(first, Some(InputMsg::ViewportStep { rows: 1 }));
        assert_eq!(second, first, "the daemon applies each to the last");
    }

    #[test]
    fn an_older_daemon_gets_an_absolute_offset_from_the_shadow() {
        let anchor = Some(ViewportAnchor {
            pushed: 9,
            generation: 0,
        });
        assert_eq!(
            step_request(1, NO_STEPS, 7, anchor, 100),
            Some(InputMsg::Viewport {
                lines_from_bottom: 8,
                anchor,
            })
        );
        assert_eq!(
            step_request(i32::MAX, NO_STEPS, 7, anchor, 100),
            Some(InputMsg::Viewport {
                lines_from_bottom: 100,
                anchor,
            })
        );
        assert_eq!(
            step_request(-9, NO_STEPS, 7, anchor, 100),
            Some(InputMsg::Viewport {
                lines_from_bottom: 0,
                anchor: None,
            })
        );
        assert_eq!(step_request(1, NO_STEPS, 100, anchor, 100), None);
    }

    /// `LineUp` then a keystroke before the reply: the shadow still shows
    /// live, but the step in flight is browsing.
    #[test]
    fn a_step_toward_history_in_flight_is_not_live() {
        let mut intent = ViewportIntent::default();
        assert!(intent.is_live(0));
        intent.note(&InputMsg::ViewportStep { rows: 1 });
        assert!(!intent.is_live(0));
        intent.note(&InputMsg::Viewport {
            lines_from_bottom: 0,
            anchor: None,
        });
        assert!(intent.is_live(0));
    }

    #[test]
    fn a_jump_or_a_browsing_offset_is_not_live() {
        let mut intent = ViewportIntent::default();
        intent.note(&InputMsg::JumpPrompt {
            direction: PromptJump::Previous,
        });
        assert!(!intent.is_live(0));

        let mut intent = ViewportIntent::default();
        intent.note(&InputMsg::Viewport {
            lines_from_bottom: 3,
            anchor: None,
        });
        assert!(!intent.is_live(0));
    }

    #[test]
    fn a_step_toward_live_does_not_claim_the_bottom() {
        let mut intent = ViewportIntent::default();
        intent.note(&InputMsg::ViewportStep { rows: 4 });
        intent.note(&InputMsg::ViewportStep { rows: -4 });
        assert!(!intent.is_live(0));
    }

    #[test]
    fn a_browsing_shadow_is_not_live_whatever_was_sent() {
        assert!(!ViewportIntent::default().is_live(3));
    }

    #[test]
    fn composed_row_for_line_maps_youngest_scrollback_just_above_live_grid() {
        assert_eq!(composed_row_for_line(-1, 1, 24), Some(0));
    }

    proptest::proptest! {
        /// The window is a contiguous run of `rows` line indices mapped
        /// onto `0..rows` in order: a formula off by a constant would
        /// still pass, which is what the anchor example above pins.
        #[test]
        fn composed_row_for_line_shifts_neighbours_by_one_row(
            line_index in -100_000_i64..=100_000,
            viewport in 0_u32..=100_000,
            rows in 1_u16..=400,
        ) {
            let Some(row) = composed_row_for_line(line_index, viewport, rows) else {
                return Ok(());
            };
            proptest::prop_assert!(row < rows);
            let above = composed_row_for_line(line_index - 1, viewport, rows);
            let below = composed_row_for_line(line_index + 1, viewport, rows);
            proptest::prop_assert_eq!(above, (row > 0).then(|| row - 1));
            proptest::prop_assert_eq!(below, (row + 1 < rows).then(|| row + 1));
        }

        /// A jump either centers the hit or hits a clamp end: landing
        /// anywhere else means the highlight paints off-screen.
        #[test]
        fn viewport_for_hit_centers_a_scrollback_hit_it_can_reach(
            line_index in -100_000_i64..=-1,
            max_scroll in 0_u32..=100_000,
            rows in 1_u16..=400,
        ) {
            let viewport = viewport_for_hit(line_index, rows, max_scroll);
            proptest::prop_assert!(viewport <= max_scroll);
            let center = i64::from(rows) / 2;
            let ideal = center - line_index;
            if ideal <= i64::from(max_scroll) {
                proptest::prop_assert_eq!(
                    composed_row_for_line(line_index, viewport, rows),
                    u16::try_from(center).ok(),
                );
            } else {
                proptest::prop_assert_eq!(viewport, max_scroll, "an unreachable hit parks at the floor");
            }
        }

        /// A live-grid hit means "leave browse", not "scroll somewhere".
        #[test]
        fn viewport_for_hit_snaps_out_of_browse_for_a_live_grid_hit(
            line_index in proptest::prop_oneof![0_i64..=500, 0_i64..=100_000],
            max_scroll in 0_u32..=100_000,
            rows in 1_u16..=400,
        ) {
            proptest::prop_assert_eq!(viewport_for_hit(line_index, rows, max_scroll), 0);
        }
    }
}

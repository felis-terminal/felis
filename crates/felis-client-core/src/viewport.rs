//! Composed-view viewport math (REQ-608 line indexing: `-1` is the
//! youngest scrollback row, `-N` older, `>= 0` the live grid). Must
//! mirror the daemon's `Grid::cell_at_viewport` composition, or search
//! highlights paint on the wrong rows.

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

#[cfg(test)]
mod tests {
    use super::*;

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

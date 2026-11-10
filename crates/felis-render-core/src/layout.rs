//! Where the cell block sits on the surface, and the inverse mapping from a
//! surface pixel back to a cell. All values are physical pixels; `origin`
//! is the block's top-left corner as [`letterbox_origin`] places it.

/// Same-user mirroring (`docs/explanation/architecture/session-lifecycle.md`):
/// a surplus of at least one cell centers the block on whole pixels, a
/// sub-cell remainder anchors top-left, and a deficit anchors bottom-left
/// so the GPU clips while the prompt stays visible.
#[must_use]
pub fn letterbox_origin(rows: u16, cols: u16, cell_px: [u32; 2], surface_px: [u32; 2]) -> [f32; 2] {
    let [cw, ch] = cell_px.map(|v| v as f32);
    let [win_w, win_h] = surface_px.map(|v| v as f32);
    let surplus_x = f32::from(cols).mul_add(-cw, win_w);
    let surplus_y = f32::from(rows).mul_add(-ch, win_h);
    let x = if surplus_x >= cw {
        (surplus_x / 2.0).floor()
    } else {
        0.0
    };
    let y = if surplus_y >= ch {
        (surplus_y / 2.0).floor()
    } else if surplus_y < 0.0 {
        surplus_y
    } else {
        0.0
    };
    [x, y]
}

/// The `(row, col)` holding `pos_px`. A position outside the block clamps
/// to its edge cell, so a stray event mid-resize still names a cell the
/// grid has; a zero cell size counts as one pixel.
#[must_use]
pub fn cell_at(
    pos_px: [f64; 2],
    origin: [f32; 2],
    cell_px: [u32; 2],
    rows: u16,
    cols: u16,
) -> (u16, u16) {
    let [cw, ch] = cell_px.map(|v| v.max(1));
    let px = (pos_px[0] - f64::from(origin[0])).max(0.0) as u32;
    let py = (pos_px[1] - f64::from(origin[1])).max(0.0) as u32;
    let col = (px / cw).min(u32::from(cols).saturating_sub(1));
    let row = (py / ch).min(u32::from(rows).saturating_sub(1));
    (row as u16, col as u16)
}

/// The 1-based, block-relative pixel the `?1016` SGR-pixel mouse encoding
/// reports, clamped into `u16` for the wire.
#[must_use]
pub fn report_pixel(pos_px: [f64; 2], origin: [f32; 2]) -> (u16, u16) {
    let clamp = |v: f64| -> u16 {
        (v.max(0.0) as u32)
            .saturating_add(1)
            .min(u32::from(u16::MAX)) as u16
    };
    (
        clamp(pos_px[0] - f64::from(origin[0])),
        clamp(pos_px[1] - f64::from(origin[1])),
    )
}

/// `[x, y, w, h]` of the cell at `(row, col)`.
#[must_use]
pub fn cell_rect_px(origin: [f32; 2], cell_px: [u32; 2], row: u16, col: u16) -> [f32; 4] {
    let [cw, ch] = cell_px;
    [
        (u32::from(col) * cw) as f32 + origin[0],
        (u32::from(row) * ch) as f32 + origin[1],
        cw as f32,
        ch as f32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_origin_keeps_top_left_for_the_everyday_remainder() {
        assert_eq!(letterbox_origin(24, 80, [10, 20], [805, 485]), [0.0, 0.0]);
    }

    #[test]
    fn letterbox_origin_centers_a_smaller_authoritative_grid() {
        assert_eq!(
            letterbox_origin(10, 40, [10, 20], [800, 600]),
            [200.0, 200.0]
        );
    }

    #[test]
    fn letterbox_origin_bottom_anchors_a_larger_authoritative_grid() {
        assert_eq!(
            letterbox_origin(30, 100, [10, 20], [800, 500]),
            [0.0, -100.0]
        );
    }

    /// Why the client re-derives the hovered cell on a reflow: font zooms
    /// or DPI changes move the grid under a motionless pointer without a
    /// motion event, so a cell cached from the last motion goes stale.
    #[test]
    fn a_cell_size_change_moves_the_cell_under_a_motionless_pointer() {
        let pointer = [35.0, 61.0];
        assert_eq!(cell_at(pointer, [0.0, 0.0], [10, 20], 24, 80), (3, 3));
        assert_eq!(
            cell_at(pointer, [0.0, 0.0], [20, 40], 24, 80),
            (1, 1),
            "a zoom to double-size cells puts a different cell under the same pixel"
        );
    }

    /// `?1016` reports 1-based pixels relative to the content block.
    #[test]
    fn report_pixel_is_one_based_and_block_relative() {
        assert_eq!(report_pixel([0.0, 0.0], [0.0, 0.0]), (1, 1));
        assert_eq!(report_pixel([11.7, 25.2], [0.0, 0.0]), (12, 26));
        assert_eq!(report_pixel([111.7, 65.2], [100.0, 40.0]), (12, 26));
        assert_eq!(report_pixel([10.0, 10.0], [100.0, 40.0]), (1, 1));
    }

    #[test]
    fn report_pixel_saturates_at_the_wire_limit() {
        assert_eq!(
            report_pixel([f64::from(u32::MAX), 100_000.0], [0.0, 0.0]),
            (u16::MAX, u16::MAX),
        );
    }

    /// Platform IMEs anchor their pre-edit panel here; a one-pixel drift
    /// puts the popup over the wrong character.
    #[test]
    fn cell_rect_px_is_the_cell_shifted_by_the_origin() {
        for (origin, row, col, want) in [
            ([0.0, 0.0], 0, 0, [0.0, 0.0, 8.0, 16.0]),
            ([0.0, 0.0], 3, 10, [80.0, 48.0, 8.0, 16.0]),
            ([0.0, 0.0], 0, 1000, [8000.0, 0.0, 8.0, 16.0]),
            ([200.0, -100.0], 3, 10, [280.0, -52.0, 8.0, 16.0]),
        ] {
            assert_eq!(
                cell_rect_px(origin, [8, 16], row, col),
                want,
                "cell ({row}, {col}) under origin {origin:?}",
            );
        }
    }

    proptest::proptest! {
        /// Every pixel of a cell names that cell once the origin comes
        /// off, and a zero cell size floors to one pixel so a pre-resize
        /// renderer still names a cell the grid has.
        #[test]
        fn cell_at_names_the_cell_holding_the_pixel(
            rows in 1_u16..=200,
            cols in 1_u16..=200,
            cell_w in 0_u32..=64,
            cell_h in 0_u32..=64,
            origin_x in -500.0_f32..=500.0,
            origin_y in -500.0_f32..=500.0,
            row_offset in 0_u32..=200,
            col_offset in 0_u32..=200,
            within_x in 0_u32..=64,
            within_y in 0_u32..=64,
            frac_x in 0.0_f64..0.999,
            frac_y in 0.0_f64..0.999,
        ) {
            let (cw, ch) = (cell_w.max(1), cell_h.max(1));
            let (row, col) = (row_offset % u32::from(rows), col_offset % u32::from(cols));
            let x = f64::from(origin_x) + f64::from(col * cw + within_x % cw) + frac_x;
            let y = f64::from(origin_y) + f64::from(row * ch + within_y % ch) + frac_y;
            let cell = cell_at([x, y], [origin_x, origin_y], [cell_w, cell_h], rows, cols);
            proptest::prop_assert_eq!(cell, (row as u16, col as u16));
        }

        /// Off the block in either direction, the nearest edge cell.
        #[test]
        fn cell_at_clamps_to_the_nearest_edge_cell(
            rows in 1_u16..=200,
            cols in 1_u16..=200,
            cell_w in 1_u32..=64,
            cell_h in 1_u32..=64,
            origin_x in -500.0_f32..=500.0,
            origin_y in -500.0_f32..=500.0,
            before in 0.0_f64..=10_000.0,
            past in 0.0_f64..=10_000.0,
        ) {
            let origin = [origin_x, origin_y];
            let cell_px = [cell_w, cell_h];
            let above = [f64::from(origin_x) - before, f64::from(origin_y) - before];
            proptest::prop_assert_eq!(cell_at(above, origin, cell_px, rows, cols), (0, 0));
            let beyond = [
                f64::from(origin_x) + f64::from(u32::from(cols) * cell_w) + past,
                f64::from(origin_y) + f64::from(u32::from(rows) * cell_h) + past,
            ];
            proptest::prop_assert_eq!(
                cell_at(beyond, origin, cell_px, rows, cols),
                (rows - 1, cols - 1),
            );
        }

        /// The forward placement and the inverse mapping agree: on a
        /// surface the block fits, every pixel of a cell's rectangle
        /// under the letterbox origin maps back to that cell.
        #[test]
        fn cell_at_inverts_cell_rect_under_the_letterbox_origin(
            rows in 1_u16..=100,
            cols in 1_u16..=200,
            cell_w in 1_u32..=32,
            cell_h in 1_u32..=48,
            extra_w in 0_u32..=2000,
            extra_h in 0_u32..=2000,
            row_offset in 0_u16..=100,
            col_offset in 0_u16..=200,
            within_x in 0.0_f64..1.0,
            within_y in 0.0_f64..1.0,
        ) {
            let cell_px = [cell_w, cell_h];
            let surface = [u32::from(cols) * cell_w + extra_w, u32::from(rows) * cell_h + extra_h];
            let origin = letterbox_origin(rows, cols, cell_px, surface);
            let (row, col) = (row_offset % rows, col_offset % cols);
            let [x, y, w, h] = cell_rect_px(origin, cell_px, row, col);
            let pos = [
                within_x.mul_add(f64::from(w), f64::from(x)),
                within_y.mul_add(f64::from(h), f64::from(y)),
            ];
            proptest::prop_assert_eq!(cell_at(pos, origin, cell_px, rows, cols), (row, col));
        }
    }
}

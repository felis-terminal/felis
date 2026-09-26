//! Property-based tests for grid-geometry admission invariants (REQ-605a).
//!
//! Evaluates creation rejection and resize clamping across bounded ranges.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_protocol::messages::{
    GRID_RESERVATION_BUDGET_BYTES, GeometryAxis, GridDims, MAX_GRID_COLS, MAX_GRID_PIXELS,
    MAX_GRID_ROWS, MIN_GRID_COLS, MIN_GRID_ROWS, RequestedDims,
};
use proptest::prelude::*;

/// `felis-grid`'s scrollback default and cell width, restated because
/// `felis-protocol` sits below `felis-grid`; `felis-grid`'s
/// `the_geometry_bounds_fit_the_held_cell_budget` checks them against
/// the real `size_of::<Cell>()`.
const SCROLLBACK_ROWS: usize = 10_000;
const CELL_BYTES: usize = 16;

/// The steady-state cell footprint of one session at `dims`: the
/// primary ring plus one parked viewport. A reflow's transient peak is
/// outside the budget by decision (session-lifecycle.md "Geometry
/// bounds").
fn held_cell_bytes(dims: GridDims) -> usize {
    let rows = usize::from(dims.rows.max(1));
    let cols = usize::from(dims.cols.max(1));
    (rows + SCROLLBACK_ROWS + rows) * cols * CELL_BYTES
}

fn dims(
    rows: impl Strategy<Value = u32>,
    cols: impl Strategy<Value = u32>,
    pixels: impl Strategy<Value = u32> + Clone,
) -> impl Strategy<Value = RequestedDims> {
    (rows, cols, pixels.clone(), pixels).prop_map(|(rows, cols, pixel_w, pixel_h)| RequestedDims {
        rows,
        cols,
        pixel_w,
        pixel_h,
    })
}

/// Weighted toward boundaries: a uniform `u32` draw would never
/// straddle one.
fn out_of_bounds() -> impl Strategy<Value = RequestedDims> {
    let axis = || {
        prop_oneof![
            3 => 0u32..=4096,
            1 => 65_000u32..=70_000,
            1 => any::<u32>(),
        ]
    };
    dims(axis(), axis(), axis())
}

/// Draws only geometry `admit` accepts, ends included: the cell axes
/// have no zero sentinel left, the pixel axes still do.
fn in_bounds() -> impl Strategy<Value = RequestedDims> {
    let cell = |min: u16, max: u16| {
        prop_oneof![
            4 => u32::from(min)..=u32::from(max),
            1 => prop::sample::select(vec![u32::from(min), u32::from(max)]),
        ]
    };
    let pixel = prop_oneof![
        4 => 0u32..=u32::from(MAX_GRID_PIXELS),
        1 => prop::sample::select(vec![0u32, 1, u32::from(MAX_GRID_PIXELS)]),
    ];
    dims(
        cell(MIN_GRID_ROWS, MAX_GRID_ROWS),
        cell(MIN_GRID_COLS, MAX_GRID_COLS),
        pixel,
    )
}

proptest! {
    /// Whatever a create admits fits the budget the maxima were derived
    /// from.
    #[test]
    fn every_admitted_geometry_fits_the_held_cell_budget(req in in_bounds()) {
        let admitted = req.admit().expect("in_bounds draws only admissible geometry");
        prop_assert!(held_cell_bytes(admitted) <= GRID_RESERVATION_BUDGET_BYTES);
    }

    /// Every geometry inside the bounds is accepted unchanged.
    #[test]
    fn admission_accepts_the_bounded_range_unchanged(req in in_bounds()) {
        let admitted = req.admit().expect("a geometry inside the bounds must be admitted");
        prop_assert_eq!(RequestedDims::from(admitted), req);
    }

    /// A create is refused exactly when some nonzero axis is out of
    /// range, and the rejection names an offending axis.
    #[test]
    fn admission_refuses_exactly_the_out_of_range_axes(req in out_of_bounds()) {
        let offenders = [
            (GeometryAxis::Rows, req.rows),
            (GeometryAxis::Cols, req.cols),
            (GeometryAxis::PixelWidth, req.pixel_w),
            (GeometryAxis::PixelHeight, req.pixel_h),
        ]
        .into_iter()
        .filter(|&(axis, value)| {
            let (min, max) = axis.bounds();
            if value == 0 && matches!(axis, GeometryAxis::PixelWidth | GeometryAxis::PixelHeight) {
                return false;
            }
            !(u32::from(min)..=u32::from(max)).contains(&value)
        })
        .collect::<Vec<_>>();
        match req.admit() {
            Ok(_) => prop_assert!(offenders.is_empty()),
            Err(rejection) => {
                prop_assert!(offenders.contains(&(rejection.axis, rejection.value)));
                prop_assert_eq!((rejection.min, rejection.max), rejection.axis.bounds());
            }
        }
    }

    /// The one surviving sentinel is the pixel pair's `0` = unknown;
    /// admitted cell axes are always a real size.
    #[test]
    fn admission_preserves_only_the_pixel_sentinel(req in in_bounds()) {
        let admitted = req.admit().expect("in_bounds draws only admissible geometry");
        prop_assert!(admitted.rows >= MIN_GRID_ROWS);
        prop_assert!(admitted.cols >= MIN_GRID_COLS);
        prop_assert_eq!(admitted.pixel_w == 0, req.pixel_w == 0);
        prop_assert_eq!(admitted.pixel_h == 0, req.pixel_h == 0);
    }

    /// A zero row or column count is refused, not defaulted: a create
    /// asks for the daemon default by omitting `SpawnArgs.dims`.
    #[test]
    fn admission_refuses_a_zero_cell_axis(
        rows in prop::sample::select(vec![0u32, 24]),
        cols in prop::sample::select(vec![0u32, 80]),
    ) {
        let req = RequestedDims { rows, cols, pixel_w: 0, pixel_h: 0 };
        match req.admit() {
            Ok(_) => prop_assert!(rows != 0 && cols != 0),
            Err(rejection) => {
                prop_assert!(rows == 0 || cols == 0);
                prop_assert_eq!(
                    rejection.axis,
                    if rows == 0 { GeometryAxis::Rows } else { GeometryAxis::Cols },
                );
            }
        }
    }

    /// A resize is never refused, and every axis lands on the value the
    /// bounds dictate rather than merely somewhere inside them. Stating
    /// the target also states idempotence: a clamped axis is its own
    /// clamp.
    #[test]
    fn clamping_lands_on_the_bounded_value(req in out_of_bounds()) {
        let d = req.clamp();
        for (axis, raw, clamped) in [
            (GeometryAxis::Rows, req.rows, d.rows),
            (GeometryAxis::Cols, req.cols, d.cols),
            (GeometryAxis::PixelWidth, req.pixel_w, d.pixel_w),
            (GeometryAxis::PixelHeight, req.pixel_h, d.pixel_h),
        ] {
            let (min, max) = axis.bounds();
            let sentinel = matches!(axis, GeometryAxis::PixelWidth | GeometryAxis::PixelHeight);
            let want = if raw == 0 && sentinel {
                0
            } else {
                raw.clamp(u32::from(min), u32::from(max)) as u16
            };
            prop_assert_eq!(clamped, want, "{:?} clamped {}", axis, raw);
        }
    }
}

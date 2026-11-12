#![allow(clippy::float_cmp)]
#![cfg_attr(
    windows,
    allow(
        dead_code,
        reason = "the pinned-font tests are compiled out on Windows"
    )
)]

#[cfg(not(windows))]
use felis_grid::Grid;

use std::sync::Arc;

use felis_shaping::Font;
#[cfg(not(windows))]
use felis_vt::Parser;

use super::*;

#[cfg(not(windows))]
mod ensure_and_atlas;
#[cfg(not(windows))]
mod fitting;
#[cfg(not(windows))]
mod ligatures_and_shape_runs;
#[cfg(not(windows))]
mod walk;

fn nz(v: u32) -> NonZeroU32 {
    NonZeroU32::new(v).unwrap()
}

fn stack() -> FontStack {
    FontStack::new(Arc::new(Font::load_test_font()))
}

#[test]
fn empty_shape_frame_reports_none_for_every_cell() {
    let frame = ShapeFrame::empty();
    assert_eq!(frame.cell_at(0, 0), ShapedCell::None);
    assert_eq!(frame.cell_at(100, 100), ShapedCell::None);
    assert_eq!(frame.cols(), 0);
    assert_eq!(frame.cells(), []);
}

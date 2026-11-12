//! Fixtures the unit tests of several modules share.

use std::{
    num::NonZeroU32,
    sync::{Arc, OnceLock},
};

use felis_grid::Grid;
use felis_shaping::{Font, FontStack, Shaper};
use felis_vt::Parser;

use crate::glyphs::{GlyphIndex, GridWalker, ShapeFrame, ShapedCell};

/// The test font alone, loaded once per process.
pub(crate) fn stack() -> FontStack {
    static FONT: OnceLock<Arc<Font>> = OnceLock::new();
    let font = FONT.get_or_init(|| Arc::new(Font::load_test_font()));
    FontStack::new(Arc::clone(font))
}

/// A one-row grid fed `bytes`, walked against the pinned set at 18 px.
pub(crate) struct Walked {
    pub(crate) index: GlyphIndex,
    pub(crate) frame: ShapeFrame,
    pub(crate) grid: Grid,
}

pub(crate) fn walk_pinned(bytes: &[u8], cols: u16) -> Walked {
    let stack = FontStack::pinned_test_stack();
    let mut grid = Grid::new(1, cols);
    Parser::new().advance(&mut grid, bytes);
    let mut index = GlyphIndex::new(stack, 18.0, NonZeroU32::new(1024).unwrap());
    let mut frame = ShapeFrame::empty();
    GridWalker::default().walk(
        &mut index,
        grid.screen(),
        &mut Shaper::new(),
        &[],
        &mut frame,
    );
    Walked { index, frame, grid }
}

pub(crate) fn cluster_fit_px_at(p: &Walked, col: u16) -> u16 {
    match p.frame.cell_at(0, col) {
        ShapedCell::Cluster { fit_px, .. } => fit_px,
        other => panic!("col {col} is not a cluster: {other:?}"),
    }
}

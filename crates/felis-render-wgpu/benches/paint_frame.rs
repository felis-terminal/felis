//! Criterion benchmark: `RowCache::paint_frame` on a 200x60 screen of
//! mixed SGR text, for a full repaint and for the two steady-state
//! shapes that repaint only some rows: one dirty row, and a cursor move.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{fmt::Write as _, hint::black_box, num::NonZeroU32, sync::Arc};

use criterion::{Criterion, criterion_group, criterion_main};
use felis_grid::Grid;
use felis_render_wgpu::{
    GLYPH_ATLAS_SIDE, SelectionRange,
    glyphs::{GlyphIndex, ShapeFrame},
    palette::{ResolvedTheme, Theme},
    row_cache::{CellFrame, RowCache},
};
use felis_shaping::{Font, FontStack};
use felis_vt::Parser;

const ROWS: u16 = 60;
const COLS: u16 = 200;
const DIRTY_ROW: usize = 29;
const BUFFER_LIMIT: u64 = 1 << 28;

/// Every row cycles through palette, truecolor, bold, italic, underline
/// and reverse runs, so no SGR path dominates.
fn mixed_sgr_screen() -> String {
    let mut out = String::new();
    for r in 0..ROWS {
        write!(out, "\x1b[{};1H", r + 1).unwrap();
        let mut col = 0;
        let mut run = usize::from(r);
        while col < usize::from(COLS) {
            let sgr = match run % 6 {
                0 => format!("38;5;{}", run % 256),
                1 => format!(
                    "38;2;{};{};200;48;5;{}",
                    run % 256,
                    (run * 7) % 256,
                    run % 16
                ),
                2 => "1".to_owned(),
                3 => "3".to_owned(),
                4 => "4".to_owned(),
                _ => "7".to_owned(),
            };
            let word = &"the quick brown fox jumps over lazy dog"[run % 20..][..8];
            write!(out, "\x1b[{sgr}m{word}\x1b[0m ").unwrap();
            col += word.len() + 1;
            run += 1;
        }
    }
    out
}

fn grid(input: &str) -> Grid {
    let mut g = Grid::new(ROWS, COLS);
    Parser::new().advance(&mut g, input.as_bytes());
    g.damage_mut().clear();
    g
}

const fn frame(selection_end: u16) -> CellFrame<'static> {
    CellFrame {
        viewport: 0,
        viewport_max: 0,
        cursor_visible: true,
        selection: Some(SelectionRange {
            start: (10, 20),
            end: (40, selection_end),
            rectangle: false,
        }),
        reverse_video: false,
        font_features: &[],
        atlas_generation: 0,
        preedit: None,
        search: None,
        confirm: None,
        link_preview: None,
    }
}

fn bench_paint_frame(c: &mut Criterion) {
    let font = Arc::new(Font::load_test_font_or_default().expect("load bench font"));
    let side = NonZeroU32::new(GLYPH_ATLAS_SIDE).expect("non-zero atlas side");
    let mut index = GlyphIndex::new(FontStack::new(font), 16.0, side);
    let text = mixed_sgr_screen();
    let mut dirty_row = grid(&text);
    dirty_row.damage_mut().mark(DIRTY_ROW);
    let cursor_a = grid(&format!("{text}\x1b[12;40H"));
    let cursor_b = grid(&format!("{text}\x1b[48;160H"));
    index.populate(dirty_row.screen());
    let _ = index.drain_pending().count();
    let metrics = index.cell_metrics();
    let theme = ResolvedTheme::new(&Theme::default());
    let shape = ShapeFrame::empty();
    let mut cache = RowCache::new(BUFFER_LIMIT);
    let paint = |cache: &mut RowCache, g: &Grid, f: &CellFrame<'_>| {
        black_box(cache.paint_frame(g.screen(), metrics, &theme, &index, &shape, f));
    };

    let mut group = c.benchmark_group("paint_frame");
    let frames = [frame(100), frame(101)];
    let mut flip = false;
    group.bench_function("full_200x60", |b| {
        b.iter(|| {
            flip = !flip;
            paint(&mut cache, &dirty_row, &frames[usize::from(flip)]);
        });
    });
    let steady = frame(100);
    paint(&mut cache, &dirty_row, &steady);
    group.bench_function("one_dirty_row", |b| {
        b.iter(|| paint(&mut cache, &dirty_row, &steady));
    });
    paint(&mut cache, &cursor_a, &steady);
    group.bench_function("cursor_move", |b| {
        b.iter(|| {
            flip = !flip;
            paint(
                &mut cache,
                if flip { &cursor_b } else { &cursor_a },
                &steady,
            );
        });
    });
    group.finish();
}

criterion_group!(benches, bench_paint_frame);
criterion_main!(benches);

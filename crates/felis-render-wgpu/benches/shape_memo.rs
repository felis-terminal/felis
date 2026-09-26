//! Criterion benchmark: `GlyphIndex::shape_run_cached` memo hit.
//! Pairs with `felis-shaping`'s `shape_run` bench, which measures the
//! unmemoized swash cost the miss path pays.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{num::NonZeroU32, sync::Arc};

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_render_wgpu::glyphs::GlyphIndex;
use felis_shaping::{Font, FontStack, FontStyle, Shaper};

/// Same row as `felis-shaping`'s `shape_run` bench so the numbers
/// compare like for like.
const ROW: &str = "user@host:~/src/felis$ cargo test -p felis-shaping -- --nocapture # => != ->";

const FONT_SIZE_PX: f32 = 16.0;

fn features() -> Vec<String> {
    vec!["calt".to_owned(), "liga".to_owned()]
}

fn memo_hit(c: &mut Criterion) {
    let font = Arc::new(Font::load_test_font_or_default().expect("load bench font"));
    let mut idx = GlyphIndex::new(
        FontStack::new(font),
        FONT_SIZE_PX,
        NonZeroU32::new(1024).expect("non-zero atlas side"),
    );
    let features = features();
    let mut shaper = Shaper::new();
    let mut out = Vec::new();
    idx.shape_run_cached(&mut shaper, &features, FontStyle::REGULAR, ROW, &mut out);
    let mut group = c.benchmark_group("shape_memo");
    group.throughput(Throughput::Bytes(ROW.len() as u64));
    group.bench_function("hit", |b| {
        b.iter(|| {
            black_box(idx.shape_run_cached(
                &mut shaper,
                &features,
                FontStyle::REGULAR,
                ROW,
                &mut out,
            ));
        });
    });
    group.finish();
}

criterion_group!(shape_memo, memo_hit);
criterion_main!(shape_memo);

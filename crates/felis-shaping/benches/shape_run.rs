//! Criterion benchmark: steady-state `Shaper::shape_run` cost with a
//! reused shaper (swash's per-font caches warm) vs a fresh one per
//! iteration.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_shaping::{Font, Shaper};

/// Ligature-bait operators are included so feature lookups actually fire.
const ROW: &str = "user@host:~/src/felis$ cargo test -p felis-shaping -- --nocapture # => != ->";

const PX: f32 = 16.0;

fn features() -> Vec<String> {
    vec!["calt".to_owned(), "liga".to_owned()]
}

fn bench_shape_run(c: &mut Criterion) {
    let font = Font::load_test_font_or_default().expect("load bench font");
    let features = features();
    let mut shaper = Shaper::new();
    drop(shaper.shape_run(&font, PX, &features, ROW));
    let mut group = c.benchmark_group("shape_run");
    group.throughput(Throughput::Bytes(ROW.len() as u64));
    group.bench_function("reused_shaper", |b| {
        b.iter(|| black_box(shaper.shape_run(&font, PX, &features, ROW)));
    });
    group.bench_function("fresh_shaper", |b| {
        b.iter(|| {
            let mut shaper = Shaper::new();
            black_box(shaper.shape_run(&font, PX, &features, ROW))
        });
    });
    group.finish();
}

criterion_group!(shape_run, bench_shape_run);
criterion_main!(shape_run);

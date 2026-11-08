//! Criterion benchmark: `ShapeCache::get_or_insert` hit vs miss.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_shaping::{Font, ShapeCache};

#[path = "support/cache.rs"]
mod cache;

use cache::{GLYPHS, PX};

fn bench_shape_cache(c: &mut Criterion) {
    let font = Font::load_test_font_or_default().expect("load bench font");
    let mut group = c.benchmark_group("shape_cache");
    group.throughput(Throughput::Elements(GLYPHS.len() as u64));

    let mut cache = ShapeCache::default();
    for &ch in GLYPHS {
        let _ = cache.get_or_insert(&font, ch, PX);
    }
    group.bench_function("hit", |b| {
        b.iter(|| {
            for &ch in GLYPHS {
                black_box(cache.get_or_insert(&font, ch, PX));
            }
        });
    });

    group.bench_function("miss", |b| {
        b.iter(|| {
            let mut cache = ShapeCache::default();
            for &ch in GLYPHS {
                black_box(cache.get_or_insert(&font, ch, PX));
            }
        });
    });
    group.finish();
}

criterion_group!(shape_cache, bench_shape_cache);
criterion_main!(shape_cache);

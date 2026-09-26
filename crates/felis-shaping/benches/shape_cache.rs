//! Criterion benchmark: `ShapeCache::get_or_insert` hit vs miss.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_shaping::{Font, ShapeCache};

const WORKLOAD: &[char] = &[
    ' ', '!', '"', '#', '$', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', '0', '1', '2',
    '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', '@', 'A', 'B', 'C', 'D', 'E',
    'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X',
    'Y', 'Z', '[', '\\', ']', '^', '_', '`', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k',
    'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', '{', '|', '}', '~',
];

const PX: u32 = 16;

fn bench_shape_cache(c: &mut Criterion) {
    let font = Font::load_test_font_or_default().expect("load bench font");
    let mut group = c.benchmark_group("shape_cache");
    group.throughput(Throughput::Elements(WORKLOAD.len() as u64));

    let mut cache = ShapeCache::default();
    for &ch in WORKLOAD {
        let _ = cache.get_or_insert(&font, ch, PX);
    }
    group.bench_function("hit", |b| {
        b.iter(|| {
            for &ch in WORKLOAD {
                black_box(cache.get_or_insert(&font, ch, PX));
            }
        });
    });

    group.bench_function("miss", |b| {
        b.iter(|| {
            let mut cache = ShapeCache::default();
            for &ch in WORKLOAD {
                black_box(cache.get_or_insert(&font, ch, PX));
            }
        });
    });
    group.finish();
}

criterion_group!(shape_cache, bench_shape_cache);
criterion_main!(shape_cache);

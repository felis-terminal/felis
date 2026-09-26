//! Criterion benchmark: `GlyphIndex::ensure` on an atlas hit vs a miss.
//! Pairs with `crates/felis-shaping/benches/shape_cache.rs`, which
//! isolates the shape half of the miss cost.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{num::NonZeroU32, sync::Arc};

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_render_wgpu::glyphs::GlyphIndex;
use felis_shaping::{Font, FontStack, SizingKey};

const WORKLOAD: &[char] = &[
    ' ', '!', '"', '#', '$', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', '0', '1', '2',
    '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', '@', 'A', 'B', 'C', 'D', 'E',
    'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X',
    'Y', 'Z', '[', '\\', ']', '^', '_', '`', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k',
    'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', '{', '|', '}', '~',
];

const FONT_SIZE_PX: f32 = 16.0;
const ATLAS_SIDE: u32 = 1024;

fn make_index(font: &Arc<Font>) -> GlyphIndex {
    GlyphIndex::new(
        FontStack::new(Arc::clone(font)),
        FONT_SIZE_PX,
        NonZeroU32::new(ATLAS_SIDE).expect("non-zero atlas side"),
    )
}

fn bench_atlas(c: &mut Criterion) {
    let font = Arc::new(Font::load_test_font_or_default().expect("load bench font"));
    let sk = SizingKey::default();
    let cell_height = make_index(&font).cell_metrics().height;

    let mut group = c.benchmark_group("atlas");
    group.throughput(Throughput::Elements(WORKLOAD.len() as u64));

    let mut idx = make_index(&font);
    for &ch in WORKLOAD {
        idx.ensure(ch, cell_height, sk);
    }
    let _ = idx.drain_pending().count();

    group.bench_function("hit", |b| {
        b.iter(|| {
            for &ch in WORKLOAD {
                black_box(idx.ensure(ch, cell_height, sk));
            }
        });
    });

    group.bench_function("miss", |b| {
        b.iter(|| {
            let mut idx = make_index(&font);
            for &ch in WORKLOAD {
                black_box(idx.ensure(ch, cell_height, sk));
            }
        });
    });
    group.finish();
}

criterion_group!(atlas, bench_atlas);
criterion_main!(atlas);

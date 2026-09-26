//! Damage-tracker baseline for the row-granularity path `compose_diffs` walks
//! every frame.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_grid::Damage;

const ROWS: usize = 24;
const SPARSE_DIRTY: &[usize] = &[2, 7, 13, 21];

/// `Damage::default()` has zero row slots; only `Grid::resize` sizes it.
fn fresh(rows: usize) -> Damage {
    let g = felis_grid::Grid::new(u16::try_from(rows).expect("rows fits"), 1);
    g.damage().clone()
}

fn bench_damage_merge(c: &mut Criterion) {
    let mut group = c.benchmark_group("damage_merge");
    let mut damage = fresh(ROWS);
    group.throughput(Throughput::Elements(SPARSE_DIRTY.len() as u64));
    group.bench_function("sparse_per_frame", |b| {
        b.iter(|| {
            for &row in SPARSE_DIRTY {
                damage.mark(black_box(row));
            }
            let dirty: Vec<usize> = damage.dirty_rows().collect();
            black_box(dirty);
            damage.clear();
        });
    });
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("mark_all_per_frame", |b| {
        b.iter(|| {
            damage.mark_all();
            let dirty: Vec<usize> = damage.dirty_rows().collect();
            black_box(dirty);
            damage.clear();
        });
    });
    group.finish();
}

criterion_group!(damage_merge, bench_damage_merge);
criterion_main!(damage_merge);

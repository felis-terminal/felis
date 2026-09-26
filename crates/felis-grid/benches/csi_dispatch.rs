//! CSI-heavy grid throughput, the `kitten __benchmark__ csi` shape.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_grid::Grid;
use felis_vt::Parser;

const ROWS: u16 = 24;
const COLS: u16 = 80;

fn rep_runs() -> Vec<u8> {
    // Re-homed each line so the stream stays scroll-free; scroll has its
    // own bench.
    let mut out = Vec::with_capacity(16 * 1024);
    while out.len() < 16 * 1024 {
        out.extend_from_slice(b"\x1b[H\x1b[10`a\x1b[100b");
    }
    out
}

fn decset_toggles() -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * 1024);
    while out.len() < 16 * 1024 {
        out.extend_from_slice(b"\x1b[m\x1b[?1h\x1b[H\x1b[39m\x1b[10`a\x1b[?1l");
    }
    out
}

fn bench_csi_dispatch(c: &mut Criterion) {
    let inputs: [(&str, Vec<u8>); 2] = [
        ("rep_runs", rep_runs()),
        ("decset_toggles", decset_toggles()),
    ];
    let mut group = c.benchmark_group("csi_dispatch");
    for (name, bytes) in &inputs {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_function(*name, |b| {
            b.iter(|| {
                let mut parser = Parser::new();
                let mut grid = Grid::new(ROWS, COLS);
                parser.advance(&mut grid, black_box(bytes));
            });
        });
    }
    group.finish();
}

criterion_group!(csi_dispatch, bench_csi_dispatch);
criterion_main!(csi_dispatch);

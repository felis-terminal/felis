//! Erase-heavy streams over a mostly blank screen: the shell prompt
//! redraw and TUI repaint shapes, where most erased cells are already
//! blank.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_grid::Grid;
use felis_vt::Parser;

const TARGET: usize = 64 * 1024;

fn repeat(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(TARGET + frame.len());
    while out.len() < TARGET {
        out.extend_from_slice(frame);
    }
    out
}

fn per_row(rows: u16, line: impl Fn(u16) -> String) -> Vec<u8> {
    let frame: String = (1..=rows).map(line).collect();
    repeat(frame.as_bytes())
}

fn streams(rows: u16, cols: u16) -> [(&'static str, Vec<u8>); 4] {
    [
        (
            "lines_el",
            per_row(rows, |r| format!("\x1b[{r};1Hline {r:03} text\x1b[K")),
        ),
        (
            "blank_el2",
            per_row(rows, |r| format!("\x1b[{r};1H\x1b[2K")),
        ),
        ("prompt_ed0", repeat(b"\x1b[H~/src $ \x1b[J")),
        (
            "blank_ech",
            per_row(rows, |r| format!("\x1b[{r};1H\x1b[{cols}X")),
        ),
    ]
}

fn bench_erase(c: &mut Criterion) {
    let mut group = c.benchmark_group("erase");
    for (rows, cols) in [(24u16, 80u16), (80, 120), (100, 300)] {
        for (name, bytes) in streams(rows, cols) {
            group.throughput(Throughput::Bytes(bytes.len() as u64));
            group.bench_function(format!("{name}/{cols}x{rows}"), |b| {
                let mut parser = Parser::new();
                let mut grid = Grid::new(rows, cols);
                b.iter(|| {
                    parser.advance(&mut grid, black_box(&bytes));
                    grid.damage_mut().clear();
                });
            });
        }
    }
    group.finish();
}

criterion_group!(erase, bench_erase);
criterion_main!(erase);

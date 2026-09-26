//! ASCII print-path throughput against a real `Grid`, shaped like
//! `kitten __benchmark__ ascii`: printable bytes with `\n` and `\t` drawn
//! as often as any one printable, on the alternate screen.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_grid::Grid;
use felis_vt::Parser;

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// kitten writes a bare `\n` through a raw-mode tty, so the column
/// carries over and nearly every line starts past the recycled row's
/// watermark; `cat` through ONLCR gets `\r\n` and restarts at column 0.
fn stream(newline: &[u8]) -> Vec<u8> {
    let alphabet: Vec<u8> = (0x20..=0x7E).chain(*b"\n\t").collect();
    let mut state: u32 = 0x2545_F491;
    let mut out = b"\x1b[?1049h".to_vec();
    while out.len() < 64 * 1024 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        match alphabet[state as usize % alphabet.len()] {
            b'\n' => out.extend_from_slice(newline),
            b => out.push(b),
        }
    }
    out
}

fn bench_ascii(c: &mut Criterion) {
    let inputs: [(&str, Vec<u8>); 2] = [
        ("bare_lf_staircase", stream(b"\n")),
        ("crlf_lines", stream(b"\r\n")),
    ];
    let mut group = c.benchmark_group("ascii_throughput");
    for (name, bytes) in &inputs {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_function(*name, |b| {
            b.iter(|| {
                let mut grid = Grid::new(ROWS, COLS);
                let mut parser = Parser::new();
                parser.advance(&mut grid, black_box(bytes));
                black_box(&grid);
            });
        });
    }
    group.finish();
}

criterion_group!(ascii_throughput, bench_ascii);
criterion_main!(ascii_throughput);

//! End-to-end PTY -> daemon -> shadow throughput baseline.
//!
//! Evaluates compute cost from `compose_diffs` through `FrameWriter::write_frame`,
//! frame decode, `GridMsg` decode, and `ShadowScreen::apply`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use felis_client_core::ShadowScreen;
use felis_grid::Grid;
use felis_vt::Parser;

#[path = "support/workload.rs"]
mod workload;

use workload::{
    COLS, PTY_CHUNK_SIZE, ROWS, STREAM_BYTES, apply_diffs, csi_redraw, emit_diffs_with,
    plaintext_scroll, repeat_to, shell_prompt,
};

fn emit_diffs(grid: &mut Grid, scratch: &mut Vec<u8>) {
    emit_diffs_with(grid, scratch, /* coalesce */ true);
}

fn emit_diffs_individual(grid: &mut Grid, scratch: &mut Vec<u8>) {
    emit_diffs_with(grid, scratch, /* coalesce */ false);
}

fn bench_throughput(c: &mut Criterion) {
    let inputs: [(&str, Vec<u8>); 3] = [
        ("plaintext_scroll", plaintext_scroll()),
        ("csi_redraw", csi_redraw()),
        ("shell_prompt", shell_prompt()),
    ];

    let multi_cycle_inputs: [(&str, Vec<u8>); 3] = [
        (
            "plaintext_scroll",
            repeat_to(&plaintext_scroll(), 32 * STREAM_BYTES),
        ),
        ("csi_redraw", repeat_to(&csi_redraw(), 32 * STREAM_BYTES)),
        (
            "shell_prompt",
            repeat_to(&shell_prompt(), 32 * STREAM_BYTES),
        ),
    ];

    {
        let mut daemon = c.benchmark_group("daemon_side");
        for (name, bytes) in &inputs {
            daemon.throughput(Throughput::Bytes(bytes.len() as u64));
            daemon.bench_function(*name, |b| {
                let mut scratch: Vec<u8> = Vec::with_capacity(128 * 1024);
                b.iter_batched(
                    || (Parser::new(), Grid::new(ROWS, COLS)),
                    |(mut parser, mut grid)| {
                        parser.advance(&mut grid, black_box(bytes));
                        emit_diffs(&mut grid, &mut scratch);
                        black_box(scratch.len())
                    },
                    BatchSize::SmallInput,
                );
            });
        }
        daemon.finish();
    }

    {
        let mut e2e = c.benchmark_group("end_to_end");
        for (name, bytes) in &inputs {
            e2e.throughput(Throughput::Bytes(bytes.len() as u64));
            e2e.bench_function(*name, |b| {
                let mut scratch: Vec<u8> = Vec::with_capacity(128 * 1024);
                b.iter_batched(
                    || {
                        (
                            Parser::new(),
                            Grid::new(ROWS, COLS),
                            ShadowScreen::new(ROWS, COLS),
                        )
                    },
                    |(mut parser, mut grid, mut shadow)| {
                        parser.advance(&mut grid, black_box(bytes));
                        emit_diffs(&mut grid, &mut scratch);
                        apply_diffs(&scratch, &mut shadow);
                        black_box(scratch.len())
                    },
                    BatchSize::SmallInput,
                );
            });
        }
        e2e.finish();
    }

    {
        let mut multi = c.benchmark_group("multi_cycle_end_to_end");
        for (name, bytes) in &multi_cycle_inputs {
            multi.throughput(Throughput::Bytes(bytes.len() as u64));
            for (label, batched) in [("batched", true), ("individual", false)] {
                let id = format!("{name}_{label}");
                multi.bench_function(&id, |b| {
                    let mut scratch: Vec<u8> = Vec::with_capacity(128 * 1024);
                    b.iter_batched(
                        || {
                            (
                                Parser::new(),
                                Grid::new(ROWS, COLS),
                                ShadowScreen::new(ROWS, COLS),
                            )
                        },
                        |(mut parser, mut grid, mut shadow)| {
                            for chunk in bytes.chunks(PTY_CHUNK_SIZE) {
                                parser.advance(&mut grid, black_box(chunk));
                                if batched {
                                    emit_diffs(&mut grid, &mut scratch);
                                } else {
                                    emit_diffs_individual(&mut grid, &mut scratch);
                                }
                                apply_diffs(&scratch, &mut shadow);
                            }
                            black_box(scratch.len())
                        },
                        BatchSize::SmallInput,
                    );
                });
            }
        }
        multi.finish();
    }

    // Stacked layers of the plaintext_scroll daemon-side cost; read via
    // the deltas between parser_only, parser_grid_no_scroll,
    // parser_grid_with_scroll and daemon_side/plaintext_scroll.
    {
        let mut group = c.benchmark_group("decomposition");
        let bytes = plaintext_scroll();
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_function("parser_only", |b| {
            b.iter_batched(
                Parser::new,
                |mut parser| {
                    struct NoopSink;
                    impl felis_vt::Sink for NoopSink {}
                    let mut sink = NoopSink;
                    parser.advance(&mut sink, black_box(&bytes));
                },
                BatchSize::SmallInput,
            );
        });
        group.bench_function("parser_grid_no_scroll", |b| {
            // 200 rows: the 93-line input never reaches the bottom row,
            // so no scroll fires.
            b.iter_batched(
                || (Parser::new(), Grid::new(200, COLS)),
                |(mut parser, mut grid)| {
                    parser.advance(&mut grid, black_box(&bytes));
                },
                BatchSize::SmallInput,
            );
        });
        group.bench_function("parser_grid_with_scroll", |b| {
            b.iter_batched(
                || (Parser::new(), Grid::new(ROWS, COLS)),
                |(mut parser, mut grid)| {
                    parser.advance(&mut grid, black_box(&bytes));
                },
                BatchSize::SmallInput,
            );
        });
        group.finish();
    }

    // 1 MiB (~24k lines) overshoots the default 10 000-row scrollback
    // cap, so pushes hit the slab recycle path the smaller benches never
    // leave warmup to reach.
    {
        let mut sb = c.benchmark_group("scrollback_intensive");
        let long_bytes = repeat_to(&plaintext_scroll(), 1024 * 1024);
        sb.throughput(Throughput::Bytes(long_bytes.len() as u64));
        sb.bench_function("plaintext_scroll_long_1MiB", |b| {
            let mut scratch: Vec<u8> = Vec::with_capacity(128 * 1024);
            b.iter_batched(
                || {
                    (
                        Parser::new(),
                        Grid::new(ROWS, COLS),
                        ShadowScreen::new(ROWS, COLS),
                    )
                },
                |(mut parser, mut grid, mut shadow)| {
                    for chunk in long_bytes.chunks(PTY_CHUNK_SIZE) {
                        parser.advance(&mut grid, black_box(chunk));
                        emit_diffs(&mut grid, &mut scratch);
                        apply_diffs(&scratch, &mut shadow);
                    }
                    black_box(scratch.len())
                },
                BatchSize::SmallInput,
            );
        });
        sb.finish();
    }
}

criterion_group!(end_to_end_throughput, bench_throughput);
criterion_main!(end_to_end_throughput);

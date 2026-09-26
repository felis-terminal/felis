//! End-to-end PTY -> daemon -> shadow throughput baseline.
//!
//! Evaluates compute cost from `compose_diffs` through `FrameWriter::write_frame`,
//! frame decode, `GridMsg` decode, and `ShadowScreen::apply`.

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![expect(
    clippy::needless_pass_by_value,
    reason = "benchmark harness: by-value params keep the bench-fn signatures simple"
)]

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use felis_client_core::ShadowScreen;
use felis_grid::{Grid, PtyEffect, RowEncode, encode_row};
use felis_protocol::{
    MessageKind, RowPayload,
    codec::{decode, encode},
    frame::{Frame, decode as decode_frame},
    messages::GridMsg,
};
use felis_vt::Parser;

const ROWS: u16 = 24;
const COLS: u16 = 80;
const STREAM_BYTES: usize = 4 * 1024;

fn plaintext_scroll() -> Vec<u8> {
    let line = b"the quick brown fox jumps over the lazy dog\n";
    let mut out = Vec::with_capacity(STREAM_BYTES);
    while out.len() < STREAM_BYTES {
        out.extend_from_slice(line);
    }
    out.truncate(STREAM_BYTES);
    out
}

fn csi_redraw() -> Vec<u8> {
    let row = b"\x1b[H\x1b[2J\x1b[1;31mhello\x1b[0m\x1b[2;32mworld\x1b[0m";
    let mut out = Vec::with_capacity(STREAM_BYTES);
    while out.len() < STREAM_BYTES {
        out.extend_from_slice(row);
    }
    out.truncate(STREAM_BYTES);
    out
}

fn repeat_to(seed: Vec<u8>, target: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target);
    while out.len() < target {
        let take = (target - out.len()).min(seed.len());
        out.extend_from_slice(&seed[..take]);
    }
    out
}

fn shell_prompt() -> Vec<u8> {
    let cycle: &[u8] = concat!(
        "\x1b]133;A\x07$ \x1b]133;B\x07ls -la\n\x1b]133;C\x07",
        "\x1b[34mdrwxr-xr-x\x1b[0m  3 user user 4096 Feb  3 01:23 .\n",
        "\x1b[34mdrwxr-xr-x\x1b[0m 33 user user 4096 Feb  3 01:23 ..\n",
        "\x1b[32m-rwxr-xr-x\x1b[0m  1 user user 1024 Feb  3 01:23 run.sh\n",
        "\x1b]133;D;0\x07",
    )
    .as_bytes();
    let mut out = Vec::with_capacity(STREAM_BYTES);
    while out.len() < STREAM_BYTES {
        out.extend_from_slice(cycle);
    }
    out.truncate(STREAM_BYTES);
    out
}

/// Mirror of the daemon's `compose_diffs` steady-state loop
/// (docs/reference/ipc.md): `Scrolled` frames first, then one `RowDelta`
/// carrying every dirty row.
fn emit_diffs(grid: &mut Grid, scratch: &mut Vec<u8>) {
    emit_diffs_with(grid, scratch, /* coalesce */ true);
}

/// The counterfactual coalescing is measured against: one frame per
/// dirty row. Not a wire shape the daemon emits.
fn emit_diffs_individual(grid: &mut Grid, scratch: &mut Vec<u8>) {
    emit_diffs_with(grid, scratch, /* coalesce */ false);
}

fn scroll_ops(grid: &mut Grid) -> Vec<felis_grid::ScrollOp> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Scrolled { op, .. } => Some(op),
            _ => None,
        })
        .collect()
}

fn emit_diffs_with(grid: &mut Grid, scratch: &mut Vec<u8>, coalesce: bool) {
    scratch.clear();
    for op in scroll_ops(grid) {
        emit_grid_msg(
            scratch,
            &GridMsg::Scrolled {
                region_top: op.region_top,
                region_bottom: op.region_bottom,
                n_rows: op.n_rows,
                direction: op.direction,
            },
        );
    }
    let dirty: Vec<usize> = grid.damage().dirty_rows().collect();
    if coalesce {
        let mut rows: Vec<(u16, RowPayload)> = Vec::with_capacity(dirty.len());
        for row_idx in dirty {
            let r = row_idx as u16;
            let cells = grid.row_cells(r).unwrap_or(&[]);
            let sized = grid.row_sized_cells(r);
            let body = encode_row(
                RowEncode {
                    cells,
                    pad_to: cells.len(),
                    sized_cells: &sized,
                    soft_wrap_continued: grid.row_soft_wrap_continued(r),
                },
                grid.style_table(),
            )
            .unwrap();
            rows.push((r, RowPayload(body)));
        }
        if !rows.is_empty() {
            emit_grid_msg(scratch, &GridMsg::RowDelta { rows });
        }
    } else {
        for row_idx in dirty {
            let r = row_idx as u16;
            let cells = grid.row_cells(r).unwrap_or(&[]);
            let sized = grid.row_sized_cells(r);
            let body = encode_row(
                RowEncode {
                    cells,
                    pad_to: cells.len(),
                    sized_cells: &sized,
                    soft_wrap_continued: grid.row_soft_wrap_continued(r),
                },
                grid.style_table(),
            )
            .unwrap();
            emit_grid_msg(
                scratch,
                &GridMsg::RowDelta {
                    rows: vec![(r, RowPayload(body))],
                },
            );
        }
    }
    grid.damage_mut().clear();
}

fn emit_grid_msg(scratch: &mut Vec<u8>, msg: &GridMsg) {
    let body_bytes = encode(msg);
    let frame = Frame {
        kind: MessageKind::Grid.as_u16(),
        body: &body_bytes,
    };
    frame
        .encode_to(scratch)
        .expect("bench bodies are inside the ceiling");
}

fn apply_diffs(scratch: &[u8], shadow: &mut ShadowScreen) {
    let mut rest = scratch;
    while !rest.is_empty() {
        let (frame, n) = decode_frame(rest).expect("frame decodes");
        let msg: GridMsg = decode(frame.body).unwrap();
        shadow.apply(&msg).expect("shadow apply");
        rest = &rest[n..];
    }
}

/// The daemon's PTY read size (`READ_BUFFER_SIZE` in `felis-pty`); the
/// multi-cycle benches run one `compose_diffs` per chunk of this size.
const PTY_CHUNK_SIZE: usize = 8 * 1024;

fn bench_throughput(c: &mut Criterion) {
    let inputs: [(&str, Vec<u8>); 3] = [
        ("plaintext_scroll", plaintext_scroll()),
        ("csi_redraw", csi_redraw()),
        ("shell_prompt", shell_prompt()),
    ];

    let multi_cycle_inputs: [(&str, Vec<u8>); 3] = [
        (
            "plaintext_scroll",
            repeat_to(plaintext_scroll(), 32 * STREAM_BYTES),
        ),
        ("csi_redraw", repeat_to(csi_redraw(), 32 * STREAM_BYTES)),
        ("shell_prompt", repeat_to(shell_prompt(), 32 * STREAM_BYTES)),
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
        let long_bytes = repeat_to(plaintext_scroll(), 1024 * 1024);
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

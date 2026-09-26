//! Client-side consume-rate baseline over a real socket: the per-frame
//! `FrameReader::next_frame` -> `GridMsg` decode -> `ShadowScreen::apply`
//! pipeline, without the winit hop or vsync-bounded `present()`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[cfg(not(unix))]
fn main() {}

#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use std::hint::black_box;

#[cfg(unix)]
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
#[cfg(unix)]
use felis_client_core::ShadowScreen;
#[cfg(unix)]
use felis_grid::{Grid, PtyEffect, RowEncode, encode_row};
#[cfg(unix)]
use felis_protocol::{
    MessageKind, RowPayload,
    codec::{decode, encode},
    frame::Frame,
    messages::GridMsg,
};
#[cfg(unix)]
use felis_transport::FrameReader;
#[cfg(unix)]
use felis_vt::Parser;
#[cfg(unix)]
use tokio::io::AsyncWriteExt as _;
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
use tokio::runtime::Runtime;

#[cfg(unix)]
const ROWS: u16 = 24;
#[cfg(unix)]
const COLS: u16 = 80;
/// The daemon's PTY read size, so the stream has production frame
/// granularity (one `compose_diffs` cycle per chunk).
#[cfg(unix)]
const PTY_CHUNK_SIZE: usize = 8 * 1024;
#[cfg(unix)]
const STREAM_BYTES: usize = 1024 * 1024;

#[cfg(unix)]
fn plaintext_scroll(target: usize) -> Vec<u8> {
    let line = b"the quick brown fox jumps over the lazy dog\n";
    let mut out = Vec::with_capacity(target);
    while out.len() < target {
        out.extend_from_slice(line);
    }
    out.truncate(target);
    out
}

#[cfg(unix)]
fn scroll_ops(grid: &mut Grid) -> Vec<felis_grid::ScrollOp> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Scrolled { op, .. } => Some(op),
            _ => None,
        })
        .collect()
}

#[cfg(unix)]
fn daemon_frame_stream(input: &[u8]) -> Vec<u8> {
    let mut parser = Parser::new();
    let mut grid = Grid::new(ROWS, COLS);
    let mut out: Vec<u8> = Vec::new();
    for chunk in input.chunks(PTY_CHUNK_SIZE) {
        parser.advance(&mut grid, chunk);
        for op in scroll_ops(&mut grid) {
            push_frame(
                &mut out,
                &GridMsg::Scrolled {
                    region_top: op.region_top,
                    region_bottom: op.region_bottom,
                    n_rows: op.n_rows,
                    direction: op.direction,
                },
            );
        }
        let dirty: Vec<usize> = grid.damage().dirty_rows().collect();
        if !dirty.is_empty() {
            let mut rows: Vec<(u16, RowPayload)> = Vec::with_capacity(dirty.len());
            for row_idx in dirty {
                let r = row_idx as u16;
                let cells = grid.row_cells(r).unwrap_or(&[]);
                let sized = grid.row_sized_cells(r);
                rows.push((
                    r,
                    RowPayload(
                        encode_row(
                            RowEncode {
                                cells,
                                pad_to: cells.len(),
                                sized_cells: &sized,
                                soft_wrap_continued: grid.row_soft_wrap_continued(r),
                            },
                            grid.style_table(),
                        )
                        .unwrap(),
                    ),
                ));
            }
            push_frame(&mut out, &GridMsg::RowDelta { rows });
        }
        grid.damage_mut().clear();
    }
    out
}

#[cfg(unix)]
fn push_frame(out: &mut Vec<u8>, msg: &GridMsg) {
    let body = encode(msg);
    Frame {
        kind: MessageKind::Grid.as_u16(),
        body: &body,
    }
    .encode_to(out)
    .expect("bench bodies are inside the ceiling");
}

#[cfg(unix)]
fn bench_client_consume(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let input = plaintext_scroll(STREAM_BYTES);
    let frames = Arc::new(daemon_frame_stream(&input));

    let mut group = c.benchmark_group("client_consume");
    // Input bytes, so the number lines up with the sibling bench.
    group.throughput(Throughput::Bytes(input.len() as u64));

    group.bench_function("plaintext_scroll_1MiB_inmem", |b| {
        b.iter(|| {
            let mut shadow = ShadowScreen::new(ROWS, COLS);
            let mut applied = 0u64;
            let mut rest: &[u8] = &frames;
            while !rest.is_empty() {
                let (frame, used) = felis_protocol::frame::decode(rest).expect("frame decode");
                let msg: GridMsg = decode(frame.body).expect("decode GridMsg");
                shadow.apply(&msg).expect("shadow apply");
                applied += 1;
                rest = &rest[used..];
            }
            black_box(applied);
        });
    });

    group.bench_function("plaintext_scroll_1MiB_socket", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (mut tx, rx) = UnixStream::pair().expect("socket pair");
                let frames = Arc::clone(&frames);
                let writer = rt.spawn(async move {
                    tx.write_all(&frames).await.expect("write frames");
                    tx.shutdown().await.expect("shutdown");
                });

                let mut reader = FrameReader::new(rx);
                let mut shadow = ShadowScreen::new(ROWS, COLS);
                let mut applied = 0u64;
                while let Some(frame) = reader.next_frame().await.expect("frame read") {
                    let msg: GridMsg = decode(&frame.body).expect("decode GridMsg");
                    shadow.apply(&msg).expect("shadow apply");
                    applied += 1;
                }
                writer.await.expect("writer join");
                black_box(applied);
            });
        });
    });

    group.finish();
}

#[cfg(unix)]
criterion_group!(client_consume, bench_client_consume);
#[cfg(unix)]
criterion_main!(client_consume);

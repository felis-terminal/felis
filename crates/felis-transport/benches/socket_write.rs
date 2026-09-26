//! Socket-write throughput for the framing path over a `UnixStream`.
//!
//! Measures syscall cost unbuffered against the daemon's `BufWriter` wrapping
//! (`felis-daemon/src/serve.rs`). Flushes per cycle to mirror the daemon's
//! `compose_diffs` boundary.

#![allow(clippy::expect_used, clippy::unwrap_used)]

// No `tokio::net::UnixStream` on Windows.
#[cfg(not(unix))]
fn main() {}

#[cfg(unix)]
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
#[cfg(unix)]
use felis_protocol::frame::Frame;
#[cfg(unix)]
use felis_protocol::messages::MAX_IMAGE_CHUNK_PAYLOAD;
#[cfg(unix)]
use felis_transport::FrameWriter;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWrite};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
use tokio::runtime::Runtime;

/// The default daemon geometry, `Grid::new(24, 80)`.
#[cfg(unix)]
const ROWS: usize = 24;
/// An 80-column ASCII `RowDelta`: two bytes per column plus 18 fixed
/// (`docs/reference/row-codec.md`).
#[cfg(unix)]
const ROW_BODY_LEN: usize = 178;

#[cfg(unix)]
fn spawn_drain(rt: &Runtime, mut half: UnixStream) {
    rt.spawn(async move {
        // Heap-allocated to stay under `clippy::large_stack_arrays`.
        let mut sink = vec![0u8; 64 * 1024];
        loop {
            match half.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
}

#[cfg(unix)]
async fn write_cycle<W: AsyncWrite + Unpin>(writer: &mut FrameWriter<W>, body: &[u8], n: usize) {
    for _ in 0..n {
        writer
            .write_frame_unchecked(&Frame { kind: 1, body })
            .await
            .expect("frame write");
    }
    writer.flush().await.expect("cycle flush");
}

#[cfg(unix)]
fn bench_socket_write(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("socket_write");
    // One element is one screen-update cycle, so throughput reads as
    // cycles/s.
    group.throughput(Throughput::Elements(1));

    let small_body = vec![b'x'; ROW_BODY_LEN];
    let large_body = vec![b'x'; ROW_BODY_LEN * ROWS];
    let chunk_body = vec![b'x'; MAX_IMAGE_CHUNK_PAYLOAD];

    for buffered in [false, true] {
        let tag = if buffered { "buffered" } else { "unbuffered" };

        group.bench_function(BenchmarkId::new("rowdelta_24x_small", tag), |b| {
            let (near, far) = rt.block_on(async { UnixStream::pair().expect("socket pair") });
            spawn_drain(&rt, far);
            if buffered {
                let mut w = FrameWriter::at_build_minor(tokio::io::BufWriter::with_capacity(
                    64 * 1024,
                    near,
                ));
                b.iter(|| rt.block_on(write_cycle(&mut w, &small_body, ROWS)));
            } else {
                let mut w = FrameWriter::at_build_minor(near);
                b.iter(|| rt.block_on(write_cycle(&mut w, &small_body, ROWS)));
            }
        });

        group.bench_function(BenchmarkId::new("image_chunk_1x", tag), |b| {
            let (near, far) = rt.block_on(async { UnixStream::pair().expect("socket pair") });
            spawn_drain(&rt, far);
            if buffered {
                let mut w = FrameWriter::at_build_minor(tokio::io::BufWriter::with_capacity(
                    64 * 1024,
                    near,
                ));
                b.iter(|| rt.block_on(write_cycle(&mut w, &chunk_body, 1)));
            } else {
                let mut w = FrameWriter::at_build_minor(near);
                b.iter(|| rt.block_on(write_cycle(&mut w, &chunk_body, 1)));
            }
        });

        group.bench_function(BenchmarkId::new("batch_1x_large", tag), |b| {
            let (near, far) = rt.block_on(async { UnixStream::pair().expect("socket pair") });
            spawn_drain(&rt, far);
            if buffered {
                let mut w = FrameWriter::at_build_minor(tokio::io::BufWriter::with_capacity(
                    64 * 1024,
                    near,
                ));
                b.iter(|| rt.block_on(write_cycle(&mut w, &large_body, 1)));
            } else {
                let mut w = FrameWriter::at_build_minor(near);
                b.iter(|| rt.block_on(write_cycle(&mut w, &large_body, 1)));
            }
        });
    }

    group.finish();
}

#[cfg(unix)]
criterion_group!(socket_write, bench_socket_write);
#[cfg(unix)]
criterion_main!(socket_write);

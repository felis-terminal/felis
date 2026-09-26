//! IPC throughput baseline for the sync body codec stack: body encode,
//! frame wrap, frame decode, and body decode.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};
use felis_protocol::{
    ImageId, MessageKind, RowPayload,
    codec::{WireCodec, decode, encode},
    frame::{Frame, decode as decode_frame},
    messages::{ConnToClientMsg, GridMsg, ImageMsg, MAX_IMAGE_CHUNK_PAYLOAD},
};

fn small_row_delta() -> GridMsg {
    // 320 bytes: a lightly-colored 80-column row under the row codec
    // (`docs/reference/row-codec.md`).
    GridMsg::RowDelta {
        rows: vec![(7, RowPayload(vec![0xCDu8; 320]))],
    }
}

fn max_chunk() -> ImageMsg {
    ImageMsg::Chunk {
        id: ImageId(1),
        bytes: vec![0xABu8; MAX_IMAGE_CHUNK_PAYLOAD].into(),
    }
}

const fn welcome() -> ConnToClientMsg {
    ConnToClientMsg::Welcome { identity: None }
}

fn frame_round_trip<'a>(body: &[u8], kind: u16, scratch: &'a mut Vec<u8>) -> &'a [u8] {
    scratch.clear();
    let frame = Frame { kind, body };
    frame
        .encode_to(scratch)
        .expect("bench bodies are inside the ceiling");
    let (decoded, _) = decode_frame(scratch).expect("frame decodes");
    decoded.body
}

fn round_trip_protobuf<M: WireCodec>(msg: &M, kind: u16, scratch: &mut Vec<u8>) -> usize {
    let body = encode(msg);
    let inner = frame_round_trip(&body, kind, scratch);
    let decoded: M = decode(inner).unwrap();
    black_box(decoded);
    scratch.len()
}

fn bench_shape<M>(group: &mut BenchmarkGroup<'_, WallTime>, name: &str, msg: &M, kind: u16)
where
    M: WireCodec,
{
    let proto_len = encode(msg).len();
    group.throughput(Throughput::Bytes(proto_len as u64));
    group.bench_function(format!("{name}/protobuf"), |b| {
        let mut scratch = Vec::with_capacity(proto_len + 32);
        b.iter(|| round_trip_protobuf(msg, kind, &mut scratch));
    });
}

fn bench_ipc(c: &mut Criterion) {
    let mut group = c.benchmark_group("ipc_throughput");

    bench_shape(
        &mut group,
        "grid_row_delta",
        &small_row_delta(),
        MessageKind::Grid.as_u16(),
    );
    bench_shape(
        &mut group,
        "image_chunk_max",
        &max_chunk(),
        MessageKind::Image.as_u16(),
    );
    bench_shape(
        &mut group,
        "conn_welcome",
        &welcome(),
        MessageKind::Conn.as_u16(),
    );

    group.finish();
}

criterion_group!(ipc_throughput, bench_ipc);
criterion_main!(ipc_throughput);

//! Criterion benchmark: `image_atlas::rgba_pixels` (per-frame RGB to RGBA
//! expansion) against a naive push-per-byte baseline.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use felis_protocol::messages::ImageFormat;
use felis_render_wgpu::image_atlas::rgba_pixels;

const RESOLUTIONS: &[(u32, u32)] = &[(1920, 1080), (2560, 1440)];

fn rgb24_frame(width: u32, height: u32) -> Vec<u8> {
    let len = (width as usize) * (height as usize) * 3;
    (0..len).map(|i| (i & 0xFF) as u8).collect()
}

fn baseline_push(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let pixel_count = (width as usize) * (height as usize);
    let mut out = Vec::with_capacity(pixel_count * 4);
    for chunk in pixels.as_chunks::<3>().0.iter().take(pixel_count) {
        out.push(chunk[0]);
        out.push(chunk[1]);
        out.push(chunk[2]);
        out.push(0xFF);
    }
    let want = pixel_count * 4;
    if out.len() < want {
        out.resize(want, 0);
    }
    out
}

fn rgb_to_rgba(c: &mut Criterion) {
    let mut group = c.benchmark_group("video_frame");
    for &(w, h) in RESOLUTIONS {
        let src = rgb24_frame(w, h);
        let label = format!("{w}x{h}");
        group.throughput(Throughput::Bytes((w as u64) * (h as u64) * 4));

        let mut scratch = Vec::new();
        group.bench_with_input(BenchmarkId::new("rgba_pixels", &label), &src, |b, src| {
            b.iter(|| {
                black_box(rgba_pixels(ImageFormat::Rgb24, w, h, black_box(src), &mut scratch).len())
            });
        });

        group.bench_with_input(BenchmarkId::new("baseline_push", &label), &src, |b, src| {
            b.iter(|| black_box(baseline_push(w, h, black_box(src))));
        });
    }
    group.finish();
}

criterion_group!(video_frame, rgb_to_rgba);
criterion_main!(video_frame);

//! Outline rasterizer fuzz target.
//!
//! Input convention: the first four bytes are the size in pixels as a
//! little-endian f32; the rest is a path of commands, each one tag byte
//! (mod 5: move, line, quad, cubic, close) followed by its points as
//! little-endian f32 pairs, so any bit pattern (NaN, infinities, huge
//! values) reaches the rasterizer. Win condition: no panic, and every
//! bitmap stays inside the rasterizer's 16 MiB working set.

#![no_main]

use felis_shaping::rasterize_path;
use libfuzzer_sys::fuzz_target;
use skrifa::PathSink;

fuzz_target!(|data: &[u8]| {
    let Some((px, mut rest)) = data.split_first_chunk::<4>() else {
        return;
    };
    let px = f32::from_le_bytes(*px);
    let bitmap = rasterize_path(px, |sink: &mut dyn PathSink| {
        let point = |rest: &mut &[u8]| {
            let (xy, tail) = rest.split_first_chunk::<8>()?;
            *rest = tail;
            let x = f32::from_le_bytes([xy[0], xy[1], xy[2], xy[3]]);
            let y = f32::from_le_bytes([xy[4], xy[5], xy[6], xy[7]]);
            Some((x, y))
        };
        while let Some((&tag, tail)) = rest.split_first() {
            rest = tail;
            let drawn = match tag % 5 {
                0 => point(&mut rest).map(|(x, y)| sink.move_to(x, y)),
                1 => point(&mut rest).map(|(x, y)| sink.line_to(x, y)),
                2 => point(&mut rest)
                    .zip(point(&mut rest))
                    .map(|((cx, cy), (x, y))| sink.quad_to(cx, cy, x, y)),
                3 => point(&mut rest)
                    .zip(point(&mut rest))
                    .zip(point(&mut rest))
                    .map(|(((c0x, c0y), (c1x, c1y)), (x, y))| {
                        sink.curve_to(c0x, c0y, c1x, c1y, x, y)
                    }),
                _ => Some(sink.close()),
            };
            if drawn.is_none() {
                return;
            }
        }
    });
    let pixels = bitmap.width() as usize * bitmap.height() as usize;
    assert!(
        pixels <= 16 << 20,
        "{}x{} bitmap",
        bitmap.width(),
        bitmap.height()
    );
});

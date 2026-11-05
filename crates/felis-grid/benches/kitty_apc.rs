//! Kitty graphics traffic from the PTY read to the APC outbox: the parse,
//! the body copy into `PtyEffect::Apc`, and the yield/drain cycle the
//! daemon's parse sink runs, without the dispatcher behind it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_grid::{Grid, PtyEffect};
use felis_vt::Parser;

const ROWS: u16 = 54;
const COLS: u16 = 192;

/// The daemon's PTY read size.
const READ: usize = 64 * 1024;

const CHUNK: usize = 4096;

fn base64(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
    felis_protocol::base64::encode_into(input, &mut out);
    out
}

fn ui_pixels(width: u32, height: u32, bpp: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut out = Vec::with_capacity(width as usize * height as usize * bpp);
    for y in 0..height {
        let mut ink = 0;
        for x in 0..width {
            let rgb = if y < 40 {
                [0x2b, 0x2d, 0x31]
            } else if x < width / 6 {
                [0xf1, 0xf3, 0xf4]
            } else {
                if x % 8 == 0 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    ink = if (y / 4) % 5 < 3 && state % 3 != 0 {
                        8
                    } else {
                        0
                    };
                }
                let shade = (255 - y * 40 / height) as u8;
                if ink > 0 {
                    ink -= 1;
                    [0x20, 0x21, 0x24]
                } else {
                    [shade, shade, 0xff]
                }
            };
            out.extend_from_slice(&rgb);
            if bpp == 4 {
                out.push(0xff);
            }
        }
    }
    out
}

fn chunked_command(out: &mut Vec<u8>, controls: &str, payload: &[u8]) -> usize {
    let encoded = base64(payload);
    let chunks: Vec<&[u8]> = encoded.chunks(CHUNK).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            write!(VecWriter(out), "\x1b_G{controls},m={more};").unwrap();
        } else {
            write!(VecWriter(out), "\x1b_Gm={more};").unwrap();
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    chunks.len()
}

struct VecWriter<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for VecWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

struct Stream {
    bytes: Vec<u8>,
    bodies: usize,
}

fn full_frame(width: u32, height: u32) -> Stream {
    let pixels = ui_pixels(width, height, 4);
    let mut bytes = Vec::new();
    let controls = format!("a=T,f=32,t=d,i=1,q=2,s={width},v={height}");
    let bodies = chunked_command(&mut bytes, &controls, &pixels);
    Stream { bytes, bodies }
}

fn cell_frames(count: u16) -> Stream {
    let patch = ui_pixels(10, 20, 4);
    let mut bytes = Vec::new();
    for n in 0..count {
        let (x, y) = ((n % COLS) * 10, (n / COLS % ROWS) * 20);
        let controls = format!("a=f,r=1,X=1,i=1,x={x},y={y},s=10,v=20,q=2");
        chunked_command(&mut bytes, &controls, &patch);
    }
    Stream {
        bytes,
        bodies: count.into(),
    }
}

/// Without `C=1` each put moves the cursor, which stops the parse until
/// the daemon has dispatched it.
fn placements(count: u16, cursor_moving: bool) -> Stream {
    let mut out = String::new();
    let c = if cursor_moving { "" } else { ",C=1" };
    for n in 0..count {
        let (image, placement) = (n % 200 + 1, n / 200 + 1);
        write!(
            out,
            "\x1b_Ga=p,i={image},p={placement},c=4,r=2,q=2{c}\x1b\\"
        )
        .unwrap();
    }
    Stream {
        bytes: out.into_bytes(),
        bodies: count.into(),
    }
}

fn chrome_and_images(frames: u16) -> Stream {
    let text: String = "felis  kitty graphics │ "
        .chars()
        .cycle()
        .take(180)
        .collect();
    let patch = ui_pixels(64, 64, 4);
    let mut bytes = Vec::new();
    let mut bodies = 0;
    for frame in 0..frames {
        for row in 1..=ROWS {
            let color = (row + frame) % 8 + 30;
            write!(
                VecWriter(&mut bytes),
                "\x1b[{row};1H\x1b[{color};1m{text}\x1b[0m"
            )
            .unwrap();
        }
        for n in 0..16 {
            let (row, col) = (n / 4 * 12 + 2, n % 4 * 40 + 20);
            write!(
                VecWriter(&mut bytes),
                "\x1b[{row};{col}H\x1b_Ga=p,i={},p=1,c=30,r=10,C=1,q=2\x1b\\",
                n + 1
            )
            .unwrap();
            bodies += 1;
        }
        bodies += chunked_command(&mut bytes, "a=f,r=1,i=1,x=0,y=0,s=64,v=64,q=2", &patch);
    }
    Stream { bytes, bodies }
}

fn feed(parser: &mut Parser, grid: &mut Grid, bytes: &[u8]) -> (usize, usize) {
    let (mut bodies, mut yields) = (0, 0);
    for read in bytes.chunks(READ) {
        let mut rest = read;
        loop {
            let stopped = parser.advance_until_yield(grid, rest);
            bodies += grid
                .take_pty_effects()
                .into_iter()
                .filter(|e| matches!(e, PtyEffect::Apc(_)))
                .count();
            grid.damage_mut().clear();
            let Some(consumed) = stopped else {
                break;
            };
            yields += 1;
            rest = &rest[consumed..];
        }
    }
    (bodies, yields)
}

fn bench_kitty_apc(c: &mut Criterion) {
    // Expected yields: the final chunk of a cursor-moving `a=T`, each
    // cursor-moving put, and each filled `APC_OUTBOX_CAP`.
    let streams = [
        (
            "transmit/rgba32_chunked/1920x1080",
            full_frame(1920, 1080),
            1,
        ),
        (
            "transmit/rgba32_chunked/2560x1600",
            full_frame(2560, 1600),
            1,
        ),
        ("cell_frames/notcurses_1000/10x20", cell_frames(1000), 0),
        (
            "placements/no_cursor_move_1000/4x2",
            placements(1000, false),
            15,
        ),
        (
            "placements/cursor_moving_1000/4x2",
            placements(1000, true),
            1000,
        ),
        (
            "mixed/chrome_and_images_10/192x54",
            chrome_and_images(10),
            0,
        ),
    ];
    let mut group = c.benchmark_group("kitty_apc");
    for (name, stream, expected_yields) in &streams {
        let mut parser = Parser::new();
        let mut grid = Grid::new(ROWS, COLS);
        let (bodies, yields) = feed(&mut parser, &mut grid, &stream.bytes);
        assert_eq!(bodies, stream.bodies, "{name}: a body was dropped");
        assert!(
            !grid.take_bell_pending(),
            "{name}: the outbox refused a body"
        );
        assert_eq!(yields, *expected_yields, "{name}");
        group.throughput(
            if name.starts_with("transmit") || name.starts_with("mixed") {
                Throughput::Bytes(stream.bytes.len() as u64)
            } else {
                Throughput::Elements(stream.bodies as u64)
            },
        );
        group.bench_function(*name, |b| {
            b.iter(|| feed(&mut parser, &mut grid, black_box(&stream.bytes)));
        });
    }
    group.finish();
}

criterion_group!(kitty_apc, bench_kitty_apc);
criterion_main!(kitty_apc);

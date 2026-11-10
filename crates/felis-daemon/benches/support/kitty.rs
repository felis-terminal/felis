//! Kitty graphics producer traffic for the daemon dispatch benches: UI-like
//! pixels, the APC bodies a producer would send for them, and a session's
//! dispatch state.

use std::hint::black_box;
use std::time::{Duration, Instant};

use felis_daemon::graphics::{
    ApcCtx, ImageEvent, Prepared, ShmDeferral, begin_apc_body, commit_decoded, dispatch_apc_body,
};
use felis_grid::Grid;
use felis_grid::images::{ImageStore, Placements};
use felis_grid::kitty_graphics::Reassembler;
use felis_protocol::messages::MAX_SESSION_IMAGE_BYTES;

pub(crate) const CHUNK: usize = 4096;

pub(crate) const CELL_W: u16 = 10;
pub(crate) const CELL_H: u16 = 20;

pub(crate) struct Lcg(u64);

impl Lcg {
    pub(crate) const fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) const fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    pub(crate) const fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

/// A UI-like frame: a flat sidebar, a gradient header, and a body of text
/// lines over a photo-like region whose low-bit noise keeps zlib and PNG
/// near 2-4x, as video and anti-aliased UI pixels compress.
pub(crate) fn ui_frame(width: u32, height: u32, bpp: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg::new(seed);
    let mut out = Vec::with_capacity(width as usize * height as usize * bpp);
    let sidebar = width / 6;
    let header = height / 10;
    let mut glyphs = 0u32;
    for y in 0..height {
        if y % 20 == 0 {
            glyphs = rng.next();
        }
        for x in 0..width {
            let rgb: [u8; 3] = if x < sidebar {
                [0x2b, 0x2d, 0x31]
            } else if y < header {
                let t = (y * 255 / header.max(1)) as u8;
                [0x30 + t / 4, 0x60 + t / 3, 0xa0 + t / 4]
            } else if (y % 20) < 14 && x < width * 3 / 5 {
                let bit = glyphs.rotate_left((x / 7) % 32) & 1 == 1;
                let stroke = (x + y) % 3 != 0;
                if bit && stroke {
                    [0x20, 0x20, 0x20]
                } else {
                    [0xf8, 0xf8, 0xf8]
                }
            } else {
                let n = (rng.next() & 0x1f) as u8;
                [
                    (x * 255 / width) as u8 ^ n,
                    (y * 255 / height) as u8 ^ n,
                    0x80 | n,
                ]
            };
            out.extend_from_slice(&rgb);
            if bpp == 4 {
                out.push(0xff);
            }
        }
    }
    out
}

/// A text page as a terminal browser paints it: flat margins, card rules,
/// and lines drawn from a few dozen glyphs, so zlib lands near the 30-40x
/// that real pages reach.
pub(crate) fn page_frame(width: u32, height: u32, seed: u64) -> Vec<u8> {
    const GLYPHS: u32 = 48;
    let mut rng = Lcg::new(seed);
    let glyphs: Vec<u128> = (0..GLYPHS)
        .map(|_| (0..4).fold(0, |g, _| g << 32 | u128::from(rng.next())))
        .collect();
    let mut line: Vec<Option<u128>> = Vec::new();
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    let margin = width / 8;
    for y in 0..height {
        let gy = y % 24;
        if gy == 0 {
            line = (0..=width / 8)
                .map(|_| (rng.below(6) != 0).then(|| glyphs[rng.below(GLYPHS) as usize]))
                .collect();
        }
        for x in 0..width {
            let rgb: [u8; 3] = if x < margin {
                [0xf3, 0xf4, 0xf6]
            } else if (x - margin) % 640 < 2 || gy == 23 {
                [0xdd; 3]
            } else if gy < 16 {
                let bit = gy * 8 + x % 8;
                let ink = line[(x / 8) as usize].is_some_and(|g| g >> bit & 1 == 1);
                if ink && bit % 3 != 0 {
                    [0x22; 3]
                } else {
                    [0xfa; 3]
                }
            } else {
                [0xfa; 3]
            };
            out.extend_from_slice(&rgb);
            out.push(0xff);
        }
    }
    out
}

pub(crate) fn base64(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    felis_protocol::base64::encode_into(data, &mut out);
    out
}

pub(crate) fn zlib(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

pub(crate) fn png_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(rgba).unwrap();
    writer.finish().unwrap();
    out
}

pub(crate) fn command(controls: &str, payload: &[u8]) -> Vec<Vec<u8>> {
    let encoded = base64(payload);
    if encoded.len() <= CHUNK {
        return vec![[format!("G{controls};").as_bytes(), &encoded].concat()];
    }
    let chunks: Vec<&[u8]> = encoded.chunks(CHUNK).collect();
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let head = match i {
                0 => format!("G{controls},m=1;"),
                _ if i == last => "Gm=0;".to_owned(),
                _ => "Gm=1;".to_owned(),
            };
            [head.as_bytes(), chunk].concat()
        })
        .collect()
}

pub(crate) struct Session {
    pub(crate) grid: Grid,
    pub(crate) images: ImageStore,
    pub(crate) placements: Placements,
    pub(crate) events: Vec<ImageEvent>,
    shm: ShmDeferral,
    reassembler: Reassembler,
}

impl Session {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            grid: Grid::new(
                (height / u32::from(CELL_H)).max(1) as u16,
                (width / u32::from(CELL_W)).max(1) as u16,
            ),
            images: ImageStore::new(MAX_SESSION_IMAGE_BYTES),
            placements: Placements::new(),
            events: Vec::new(),
            shm: ShmDeferral::default(),
            reassembler: Reassembler::new(),
        }
    }

    /// `q=2` silences every reply, so a rejected workload shows only in
    /// the store and the events: each bench asserts on those before timing.
    pub(crate) fn feed(&mut self, bodies: &[Vec<u8>]) {
        for body in bodies {
            let (mut ctx, reassembler) = self.ctx();
            black_box(dispatch_apc_body(&mut ctx, reassembler, body));
        }
    }

    const fn ctx(&mut self) -> (ApcCtx<'_>, &mut Reassembler) {
        let ctx = ApcCtx {
            grid: &mut self.grid,
            images: &mut self.images,
            placements: &mut self.placements,
            events: &mut self.events,
            shm: &mut self.shm,
            cell_pixel_w: CELL_W,
            cell_pixel_h: CELL_H,
            anchor_cursor: Some((0, 0)),
        };
        (ctx, &mut self.reassembler)
    }

    pub(crate) fn feed_all(&mut self, commands: &[Vec<Vec<u8>>]) {
        for command in commands {
            self.feed(command);
        }
    }

    /// How long the session task holds the parse lock to dispatch one
    /// heavy command: every body of it, but not the decode the last one
    /// hands off.
    pub(crate) fn hold(&mut self, bodies: &[Vec<u8>]) -> Duration {
        let Some((last, head)) = bodies.split_last() else {
            return Duration::ZERO;
        };
        let start = Instant::now();
        self.feed(head);
        let (mut ctx, reassembler) = self.ctx();
        let Prepared::Decode(prepared) = begin_apc_body(&mut ctx, reassembler, last, None) else {
            panic!("the command decodes nothing");
        };
        let (job, plan) = prepared.into_job();
        let prepared = start.elapsed();
        let decoded = black_box(job.run());
        let start = Instant::now();
        black_box(commit_decoded(&mut ctx, &plan, decoded));
        prepared + start.elapsed()
    }
}

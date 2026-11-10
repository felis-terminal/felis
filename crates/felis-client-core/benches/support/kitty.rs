//! Kitty graphics producers driven through the daemon's own dispatcher,
//! so the `ImageMsg` streams the image benches replay are the ones a
//! client receives.

use std::fmt::Write as _;

use felis_daemon::graphics::{
    ApcCtx, ImageEvent, ShmDeferral, dispatch_apc_body, materialize_image_events,
};
use felis_grid::kitty_graphics::Reassembler;
use felis_grid::{
    Grid,
    images::{ImageStore, Placements},
};
use felis_protocol::{
    kitty_graphics::placeholder::{PLACEHOLDER, Rank, diacritic_for},
    messages::{ImageMsg, MAX_SESSION_IMAGE_BYTES},
};

pub(crate) const ROWS: u16 = 54;
pub(crate) const COLS: u16 = 192;
pub(crate) const CELL_PX: (f32, f32) = (10.0, 20.0);

const CHUNK: usize = 4096;

fn base64(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
    felis_protocol::base64::encode_into(input, &mut out);
    out
}

pub(crate) fn ui_pixels(width: u32, height: u32, bpp: usize) -> Vec<u8> {
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

pub(crate) struct Daemon {
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    events: Vec<ImageEvent>,
    shm: ShmDeferral,
    reassembler: Reassembler,
}

impl Daemon {
    pub(crate) fn new() -> Self {
        Self {
            grid: Grid::new(ROWS, COLS),
            images: ImageStore::new(MAX_SESSION_IMAGE_BYTES),
            placements: Placements::new(),
            events: Vec::new(),
            shm: ShmDeferral::default(),
            reassembler: Reassembler::new(),
        }
    }

    /// `at` is 0-based.
    pub(crate) fn send(&mut self, controls: &str, payload: &[u8], at: (u16, u16)) {
        let encoded = base64(payload);
        let chunks: Vec<&[u8]> = encoded.chunks(CHUNK).collect();
        let chunks = if chunks.is_empty() {
            vec![&[][..]]
        } else {
            chunks
        };
        for (i, chunk) in chunks.iter().enumerate() {
            let more = u8::from(i + 1 < chunks.len());
            let mut body = if i == 0 {
                format!("G{controls},q=2,m={more};")
            } else {
                format!("Gm={more};")
            }
            .into_bytes();
            body.extend_from_slice(chunk);
            let mut ctx = ApcCtx {
                grid: &mut self.grid,
                images: &mut self.images,
                placements: &mut self.placements,
                events: &mut self.events,
                shm: &mut self.shm,
                cell_pixel_w: CELL_PX.0 as u16,
                cell_pixel_h: CELL_PX.1 as u16,
                anchor_cursor: Some(at),
            };
            assert_eq!(
                dispatch_apc_body(&mut ctx, &mut self.reassembler, &body),
                None
            );
        }
    }

    pub(crate) fn ship(&mut self) -> Vec<ImageMsg> {
        let msgs = materialize_image_events(&self.events, &self.images, true);
        self.events.clear();
        msgs
    }
}

/// `at` is 1-based, a CUP target.
pub(crate) fn placeholder_grid(out: &mut String, image: u8, at: (u16, u16), cols: u32, rows: u32) {
    for r in 0..rows {
        write!(
            out,
            "\x1b[{};{}H\x1b[38;5;{image}m",
            u32::from(at.0) + r,
            at.1
        )
        .unwrap();
        for c in 0..cols {
            let rank = |n| diacritic_for(Rank::new(n).unwrap());
            out.extend([PLACEHOLDER, rank(r), rank(c)]);
        }
    }
    out.push_str("\x1b[0m");
}

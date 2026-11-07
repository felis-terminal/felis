//! Kitty graphics workloads shared by the Criterion benches and their
//! instruction-count twin: rich-TUI producer streams as PTY bytes, and
//! the pipeline they cross from the parse through the daemon's
//! dispatcher and fan-out to the client's image shadow and layout.

use std::fmt::Write as _;
use std::hint::black_box;
use std::num::NonZeroU32;

use bytes::{BufMut as _, BytesMut};
use felis_client_core::{ClientImage, ImageId, ImageLayout, ImageShadow, ShadowScreen};
use felis_daemon::graphics::{
    ApcCtx, ImageEvent, ShmDeferral, dispatch_apc_body, materialize_image_events,
};
use felis_grid::{
    Grid, PtyEffect,
    images::{ImageEntry, ImageStore, Placements},
};
use felis_protocol::{
    MessageKind,
    codec::{decode, encode, encode_image},
    frame::{Frame, HEADER_LEN, LEN_OVERHEAD, decode as decode_frame},
    kitty_graphics::placeholder::{PLACEHOLDER, Rank, diacritic_for},
    messages::{GridMsg, ImageMsg, MAX_SESSION_IMAGE_BYTES},
};
use felis_vt::{Parser, kitty_graphics::Reassembler};

use crate::workload::{apply_diffs, emit_diffs_with};

const ROWS: u16 = 54;
const COLS: u16 = 192;
const CELL_PX: (u16, u16) = (10, 20);

/// The daemon's PTY read size (`READ_BUFFER_SIZE` in `felis-pty`). A
/// parser yield inside a read ends a session cycle, as the end of the
/// read does: drain, compose, fan-out and one client redraw.
const PTY_READ: usize = 64 * 1024;

const CHUNK: usize = 4096;

#[derive(Clone, Copy)]
pub(crate) enum Graphics {
    VideoInline,
    BrowserReplace,
    BrowserDirtyRects,
    #[cfg(unix)]
    BrowserShm,
    PatchLayers,
    Thumbnails,
    CellWipes,
}

impl Graphics {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::VideoInline => "video_inline",
            Self::BrowserReplace => "browser_replace",
            Self::BrowserDirtyRects => "browser_dirty_rects",
            #[cfg(unix)]
            Self::BrowserShm => "browser_shm",
            Self::PatchLayers => "patch_layers",
            Self::Thumbnails => "thumbnails",
            Self::CellWipes => "cell_wipes",
        }
    }

    /// The bytes a session holds before the measured stream, and the
    /// stream as the producer writes it: each write is one PTY read, so
    /// one session cycle.
    pub(crate) fn streams(self) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut prelude = Vec::new();
        let mut stream = Vec::new();
        match self {
            Self::VideoInline => {
                for seed in 1..=4 {
                    stream.extend_from_slice(b"\x1b[2;3H");
                    command(
                        &mut stream,
                        "a=T,f=24,s=320,v=180,C=1,q=2",
                        &ui_frame(320, 180, 3, seed),
                    );
                }
            }
            Self::BrowserReplace => {
                browser_frame(&mut prelude, 0);
                let writes = BROWSER_SEEDS.map(|seed| {
                    let mut write = Vec::new();
                    browser_frame(&mut write, seed);
                    write
                });
                return (prelude, writes.into());
            }
            Self::BrowserDirtyRects => {
                browser_frame(&mut prelude, 0);
                let writes = browser_updates()
                    .iter()
                    .map(|update| {
                        let mut write = Vec::new();
                        browser_update(&mut write, update);
                        write
                    })
                    .collect();
                return (prelude, writes);
            }
            #[cfg(unix)]
            Self::BrowserShm => {
                shm_frame(&mut prelude, 0);
                let writes = (1..=SHM_FRAMES).map(|k| {
                    let mut write = Vec::new();
                    shm_frame(&mut write, k);
                    write
                });
                return (prelude, writes.collect());
            }
            Self::CellWipes => {
                let (base, edits) = self.edits();
                base_image(&mut prelude, base);
                for Edit { x, y, w, h, pixels } in edits {
                    let controls = format!("a=f,i=1,r=1,X=1,x={x},y={y},s={w},v={h},f=32,q=2");
                    command(&mut stream, &controls, &pixels);
                }
                command(&mut stream, "a=a,i=1,c=1,q=2", &[]);
            }
            Self::PatchLayers => {
                base_image(&mut prelude, (640, 360));
                for k in 0..64 {
                    patch(&mut prelude, k, false);
                }
                for k in 0..64 {
                    delete_patch(&mut stream, k);
                    patch(&mut stream, k, true);
                }
            }
            Self::Thumbnails => {
                for k in 0..64 {
                    let id = k + 1;
                    let controls = format!("a=T,U=1,i={id},f=32,s=32,v=32,c=6,r=3,q=2");
                    command(&mut stream, &controls, &ui_frame(32, 32, 4, u64::from(k)));
                    placeholder_tile(&mut stream, id, (k / 8 * 4 + 1, k % 8 * 7 + 1), 6, 3);
                }
            }
        }
        (prelude, vec![stream])
    }

    /// The segments a `t=s` workload names, alive while the guard is.
    pub(crate) fn segments(self) -> Segments {
        match self {
            #[cfg(unix)]
            Self::BrowserShm => Segments::create(
                (0..=SHM_FRAMES)
                    .map(|k| (shm_name(k), page_frame(BROWSER_PAGE.0, BROWSER_PAGE.1, k)))
                    .collect(),
            ),
            _ => Segments::default(),
        }
    }

    /// The `X=1` frame edits a workload sends over its base image; every
    /// one stays visible in the final frame, so a rejected edit shows.
    fn edits(self) -> ((u32, u32), Vec<Edit>) {
        match self {
            Self::CellWipes => {
                let (w, h) = (u32::from(CELL_PX.0), u32::from(CELL_PX.1));
                let (cols, rows) = (16, 8);
                let mut cells: Vec<u32> = (0..cols * rows).collect();
                let mut rng = Lcg::new(8);
                for i in (1..cells.len()).rev() {
                    cells.swap(i, rng.below(i as u32 + 1) as usize);
                }
                let edits = cells
                    .into_iter()
                    .enumerate()
                    .map(|(k, cell)| {
                        let rgba = [0x10, (k % 256) as u8, 0xa0, 0xff];
                        let (x, y) = (cell % cols * w, cell / cols * h);
                        let pixels = rgba.repeat((w * h) as usize);
                        Edit { x, y, w, h, pixels }
                    })
                    .collect();
                ((cols * w, rows * h), edits)
            }
            _ => ((0, 0), Vec::new()),
        }
    }

    /// `q=2` silences every rejection, so a workload the dispatcher
    /// stopped accepting would measure an empty pipeline.
    pub(crate) fn check(self, p: &Pipeline) {
        let (images, placements, virtual_placements, draws) = match self {
            Self::VideoInline => (4, 4, 0, 4),
            Self::BrowserReplace | Self::BrowserDirtyRects | Self::CellWipes => (1, 1, 0, 1),
            #[cfg(unix)]
            Self::BrowserShm => (1, 1, 0, 1),
            Self::PatchLayers => (65, 65, 0, 65),
            Self::Thumbnails => (64, 0, 64, 64 * 6 * 3),
        };
        let name = self.name();
        assert_eq!(p.images.len(), images, "{name}: daemon images");
        assert_eq!(
            p.image_shadow.images().len(),
            images,
            "{name}: client images"
        );
        let shown = p.image_shadow.placements().count();
        assert_eq!(shown, placements, "{name}: placements");
        let virtuals = p.image_shadow.virtual_placements().len();
        assert_eq!(virtuals, virtual_placements, "{name}: virtual placements");
        let laid_out =
            p.layout.under_bg().len() + p.layout.under_text().len() + p.layout.above_text().len();
        assert_eq!(laid_out, draws, "{name}: draws");
        match self {
            Self::BrowserReplace => {
                let mut replay = Replay::new(self);
                for seed in BROWSER_SEEDS {
                    let session = replay.step(|out| browser_frame(out, seed));
                    assert!(
                        session.holds(1, &browser_page(seed)),
                        "{name}: frame {seed} not replaced"
                    );
                }
                replay.finish();
                let last = BROWSER_SEEDS[BROWSER_SEEDS.len() - 1];
                assert!(p.holds(1, &browser_page(last)), "{name}: not replaced");
            }
            Self::BrowserDirtyRects => {
                let mut replay = Replay::new(self);
                let mut canvas = browser_page(0);
                for (k, update) in browser_updates().iter().enumerate() {
                    let session = replay.step(|out| browser_update(out, update));
                    paint_onto(&mut canvas, BROWSER_PAGE.0, update);
                    assert!(session.holds(1, &canvas), "{name}: update {k} did not land");
                }
                replay.finish();
                assert!(p.holds(1, &canvas), "{name}: an edit did not land");
            }
            #[cfg(unix)]
            Self::BrowserShm => {
                let mut replay = Replay::new(self);
                for k in 1..=SHM_FRAMES {
                    let session = replay.step(|out| shm_frame(out, k));
                    assert!(
                        session.holds(1, &browser_page(k)),
                        "{name}: frame {k} not read"
                    );
                }
                replay.finish();
                assert!(
                    p.holds(1, &browser_page(SHM_FRAMES)),
                    "{name}: not replaced"
                );
            }
            Self::CellWipes => {
                let (base, edits) = self.edits();
                let expected = paint(base, &edits, None);
                for skipped in 0..edits.len() {
                    assert_ne!(
                        paint(base, &edits, Some(skipped)),
                        expected,
                        "{name}: edit hidden"
                    );
                }
                assert!(p.holds(1, &expected), "{name}: an edit did not land");
            }
            Self::PatchLayers => {
                let mut replay = Replay::new(self);
                for k in 0..64 {
                    let id = 1000 + k;
                    let session = replay.step(|out| delete_patch(out, k));
                    assert!(session.lacks(id), "{name}: id {id} not deleted");
                    let reused = patch_pixels(k, true);
                    assert_ne!(reused, patch_pixels(k, false));
                    let session = replay.step(|out| patch(out, k, true));
                    assert!(session.holds(id, &reused), "{name}: id {id} not reused");
                    assert!(p.holds(id, &reused), "{name}: id {id} not reused");
                }
                replay.finish();
            }
            Self::VideoInline | Self::Thumbnails => {}
        }
    }
}

struct Edit {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    pixels: Vec<u8>,
}

/// The base image after `X=1` edits, which replace pixels outright.
fn paint((width, height): (u32, u32), edits: &[Edit], skipped: Option<usize>) -> Vec<u8> {
    let mut canvas = ui_frame(width, height, 4, 0);
    for (k, edit) in edits.iter().enumerate() {
        if Some(k) != skipped {
            paint_onto(&mut canvas, width, std::slice::from_ref(edit));
        }
    }
    canvas
}

fn paint_onto(canvas: &mut [u8], width: u32, edits: &[Edit]) {
    for edit in edits {
        let row = edit.w as usize * 4;
        for (r, src) in edit.pixels.chunks(row).enumerate() {
            let at = ((edit.y as usize + r) * width as usize + edit.x as usize) * 4;
            canvas[at..at + row].copy_from_slice(src);
        }
    }
}

/// terminal-browser's frame at an 8x16-cell 240x67 window.
const BROWSER_PAGE: (u32, u32) = (1920, 1072);

const BROWSER_SEEDS: [u64; 2] = [1, 2];

fn browser_page(seed: u64) -> Vec<u8> {
    page_frame(BROWSER_PAGE.0, BROWSER_PAGE.1, seed)
}

/// Every write terminal-browser makes is one synchronized update.
fn synchronized(out: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) {
    out.extend_from_slice(b"\x1b[?2026h");
    body(out);
    out.extend_from_slice(b"\x1b[?2026l");
}

fn browser_frame(out: &mut Vec<u8>, seed: u64) {
    let (w, h) = BROWSER_PAGE;
    let pixels = zlib(&browser_page(seed));
    let controls = format!("a=T,f=32,o=z,s={w},v={h},t=d,i=1,p=1,C=1,z=0,q=2");
    synchronized(out, |out| {
        out.extend_from_slice(b"\x1b[H");
        command(out, &controls, &pixels);
    });
}

/// Each update's rectangles as terminal-browser's frame-edit presenter
/// sent them for a hover, typed text, a CSS spinner beside a ticking
/// counter, and a scroll, cut from one capture of each.
const BROWSER_TRACE: [&[(u32, u32, u32, u32)]; 22] = [
    &[(652, 133, 81, 30)],
    &[(318, 140, 1, 17)],
    &[(652, 133, 81, 30)],
    &[(318, 140, 1, 17)],
    &[(318, 139, 67, 17)],
    &[(326, 139, 10, 17)],
    &[(335, 139, 5, 17)],
    &[(339, 139, 4, 17)],
    &[(342, 139, 10, 17)],
    &[(351, 139, 6, 17)],
    &[(262, 130, 54, 54)],
    &[(262, 130, 54, 54)],
    &[(262, 130, 54, 54)],
    &[(262, 129, 54, 55)],
    &[(262, 129, 153, 55)],
    &[(262, 130, 55, 54)],
    &[(262, 130, 54, 54)],
    &[(262, 130, 54, 54)],
    &[(262, 130, 153, 54)],
    &[(262, 130, 54, 54)],
    &[(250, 25, 900, 1045), (263, 25, 1649, 176)],
    &[(250, 25, 1662, 328), (250, 139, 900, 931)],
];

/// Glyph-sized rectangles carry anti-aliased edges, which compress like a
/// photo; a scrolled block of page compresses like the page.
fn browser_updates() -> Vec<Vec<Edit>> {
    let mut seed = 0;
    BROWSER_TRACE
        .iter()
        .map(|rects| {
            rects
                .iter()
                .map(|&(x, y, w, h)| {
                    seed += 1;
                    let pixels = if w * h < 1 << 16 {
                        ui_frame(w, h, 4, seed)
                    } else {
                        page_frame(w, h, seed)
                    };
                    Edit { x, y, w, h, pixels }
                })
                .collect()
        })
        .collect()
}

fn browser_update(out: &mut Vec<u8>, edits: &[Edit]) {
    synchronized(out, |out| {
        for Edit { x, y, w, h, pixels } in edits {
            let controls = format!("a=f,f=32,o=z,i=1,r=1,X=1,x={x},y={y},s={w},v={h},t=d,q=2");
            command(out, &controls, &zlib(pixels));
        }
    });
}

#[cfg(unix)]
const SHM_FRAMES: u64 = 2;

/// terminal-browser names a fresh segment for every frame.
#[cfg(unix)]
fn shm_name(k: u64) -> String {
    format!("/felis-bench-{}-browser-{k}", std::process::id())
}

#[cfg(unix)]
fn shm_frame(out: &mut Vec<u8>, k: u64) {
    let (w, h) = BROWSER_PAGE;
    let controls = format!("a=T,f=32,s={w},v={h},t=s,i=1,p=1,C=1,z=0,q=2");
    synchronized(out, |out| {
        out.extend_from_slice(b"\x1b[H");
        command(out, &controls, shm_name(k).as_bytes());
    });
}

/// The daemon defers a `t=s` unlink to session teardown, which a bench
/// pipeline never reaches, so one set of segments serves every run.
#[derive(Default)]
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "only Unix names shared-memory segments")
)]
pub(crate) struct Segments(Vec<String>);

impl Segments {
    #[cfg(unix)]
    fn create(segments: Vec<(String, Vec<u8>)>) -> Self {
        use std::io::Write as _;
        let mut names = Vec::new();
        for (name, data) in segments {
            let fd = rustix::shm::open(
                name.as_str(),
                rustix::shm::OFlags::CREATE
                    | rustix::shm::OFlags::RDWR
                    | rustix::shm::OFlags::TRUNC,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .expect("shm segment opens");
            std::fs::File::from(fd)
                .write_all(&data)
                .expect("shm segment fills");
            names.push(name);
        }
        Self(names)
    }
}

impl Drop for Segments {
    fn drop(&mut self) {
        #[cfg(unix)]
        for name in &self.0 {
            let _ = rustix::shm::unlink(name.as_str());
        }
    }
}

const PATCHES: [(u32, u32); 3] = [(16, 16), (32, 16), (64, 32)];

fn patch_pixels(k: u32, reused: bool) -> Vec<u8> {
    let (w, h) = PATCHES[k as usize % PATCHES.len()];
    ui_frame(w, h, 4, u64::from(k) + if reused { 64 } else { 0 })
}

fn patch(out: &mut Vec<u8>, k: u32, reused: bool) {
    let (w, h) = PATCHES[k as usize % PATCHES.len()];
    let id = 1000 + k;
    write!(Bytes(out), "\x1b[{};{}H", k / 8 * 2 + 1, k % 8 * 8 + 1).unwrap();
    let controls = format!("a=T,i={id},f=32,s={w},v={h},z={},C=1,q=2", k + 1);
    command(out, &controls, &patch_pixels(k, reused));
}

fn delete_patch(out: &mut Vec<u8>, k: u32) {
    command(out, &format!("a=d,d=I,i={},q=2", 1000 + k), &[]);
}

/// A fresh session fed a workload's stream one step at a time, for the
/// steps a later one overwrites: the final state cannot show them.
struct Replay {
    name: &'static str,
    session: Pipeline,
    stream: Vec<u8>,
    at: usize,
}

impl Replay {
    fn new(workload: Graphics) -> Self {
        let (prelude, writes) = workload.streams();
        let mut session = Pipeline::new();
        session.feed(&prelude);
        Self {
            name: workload.name(),
            session,
            stream: writes.concat(),
            at: 0,
        }
    }

    fn step(&mut self, write: impl FnOnce(&mut Vec<u8>)) -> &Pipeline {
        let mut bytes = Vec::new();
        write(&mut bytes);
        let name = self.name;
        assert!(
            self.stream[self.at..].starts_with(&bytes),
            "{name}: replayed step not next in the stream"
        );
        self.at += bytes.len();
        self.session.feed(&bytes);
        &self.session
    }

    fn finish(self) {
        let name = self.name;
        assert_eq!(
            self.at,
            self.stream.len(),
            "{name}: stream tail not replayed"
        );
    }
}

fn base_image(out: &mut Vec<u8>, (w, h): (u32, u32)) {
    let controls = format!("a=T,i=1,p=1,f=32,s={w},v={h},C=1,q=2");
    command(out, &controls, &ui_frame(w, h, 4, 0));
}

struct Bytes<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Bytes<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

fn command(out: &mut Vec<u8>, controls: &str, payload: &[u8]) {
    let mut encoded = Vec::with_capacity(payload.len().div_ceil(3) * 4);
    felis_protocol::base64::encode_into(payload, &mut encoded);
    if encoded.len() <= CHUNK {
        write!(Bytes(out), "\x1b_G{controls};").unwrap();
        out.extend_from_slice(&encoded);
        out.extend_from_slice(b"\x1b\\");
        return;
    }
    let chunks: Vec<&[u8]> = encoded.chunks(CHUNK).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            write!(Bytes(out), "\x1b_G{controls},m={more};").unwrap();
        } else {
            write!(Bytes(out), "\x1b_Gm={more};").unwrap();
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// `at` is 1-based, a CUP target.
fn placeholder_tile(out: &mut Vec<u8>, image: u32, at: (u32, u32), cols: u32, rows: u32) {
    let mut text = String::new();
    for r in 0..rows {
        write!(text, "\x1b[{};{}H\x1b[38;5;{image}m", at.0 + r, at.1).unwrap();
        for c in 0..cols {
            let rank = |n| diacritic_for(Rank::new(n).unwrap());
            text.extend([PLACEHOLDER, rank(r), rank(c)]);
        }
    }
    text.push_str("\x1b[0m");
    out.extend_from_slice(text.as_bytes());
}

fn zlib(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

struct Lcg(u64);

impl Lcg {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    const fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    const fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

/// A UI-like frame: a flat sidebar, a gradient header, and text lines
/// over a noisy photo region, so zlib lands near a real UI's ratio.
fn ui_frame(width: u32, height: u32, bpp: usize, seed: u64) -> Vec<u8> {
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
                if glyphs.rotate_left((x / 7) % 32) & 1 == 1 && (x + y) % 3 != 0 {
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
/// that real pages reach, where [`ui_frame`]'s noise holds it near 3x.
fn page_frame(width: u32, height: u32, seed: u64) -> Vec<u8> {
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

fn frame_into(out: &mut Vec<u8>, kind: MessageKind, body: &[u8]) {
    Frame {
        kind: kind.as_u16(),
        body,
    }
    .encode_to(out)
    .expect("bench bodies are inside the ceiling");
}

/// The daemon's fan-out writes an image body as its head and its
/// uncopied payload (`CheckedFrame::encode_image`).
fn image_frame_onto(out: &mut BytesMut, msg: &ImageMsg) {
    let (head, tail) = encode_image(msg);
    let len = u32::try_from(head.len() + tail.len()).expect("bench bodies are inside the ceiling");
    out.put_u32_le(len + LEN_OVERHEAD);
    out.put_u16_le(MessageKind::Image.as_u16());
    out.extend_from_slice(&head);
    out.extend_from_slice(&tail);
}

/// One session's daemon state and one attached client's mirror of it.
pub(crate) struct Pipeline {
    parser: Parser,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    events: Vec<ImageEvent>,
    shm: ShmDeferral,
    reassembler: Reassembler,
    scratch: Vec<u8>,
    wire: BytesMut,
    clusters_sent: usize,
    shadow: ShadowScreen,
    image_shadow: ImageShadow,
    layout: ImageLayout,
}

impl Pipeline {
    pub(crate) fn new() -> Self {
        Self {
            parser: Parser::new(),
            grid: Grid::new(ROWS, COLS),
            images: ImageStore::new(MAX_SESSION_IMAGE_BYTES),
            placements: Placements::new(),
            events: Vec::new(),
            shm: ShmDeferral::default(),
            reassembler: Reassembler::new(),
            scratch: Vec::with_capacity(PTY_READ),
            wire: BytesMut::new(),
            clusters_sent: 0,
            shadow: ShadowScreen::new(ROWS, COLS),
            image_shadow: ImageShadow::new(),
            layout: ImageLayout::new(),
        }
    }

    fn lacks(&self, id: u32) -> bool {
        self.image_shadow.image(ImageId(id)).is_none() && self.images.get(ImageId(id)).is_none()
    }

    fn holds(&self, id: u32, expected: &[u8]) -> bool {
        let client = self
            .image_shadow
            .image(ImageId(id))
            .map(ClientImage::pixels);
        let daemon = self.images.get(ImageId(id)).map(ImageEntry::pixels);
        client == Some(expected) && daemon == Some(expected)
    }

    pub(crate) fn feed_writes(&mut self, writes: &[Vec<u8>]) {
        for write in writes {
            self.feed(write);
        }
    }

    /// The streams never scroll, so dropping the non-APC effects loses
    /// no `Scrolled` op the grid diffs would need.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for read in bytes.chunks(PTY_READ) {
            let mut rest = read;
            loop {
                let stopped = self.parser.advance_until_yield(&mut self.grid, rest);
                self.dispatch();
                self.compose();
                self.redraw();
                let Some(consumed) = stopped else {
                    break;
                };
                rest = &rest[consumed..];
            }
        }
    }

    fn dispatch(&mut self) {
        for effect in self.grid.take_pty_effects() {
            let PtyEffect::Apc(apc) = effect else {
                continue;
            };
            let mut ctx = ApcCtx {
                grid: &mut self.grid,
                images: &mut self.images,
                placements: &mut self.placements,
                events: &mut self.events,
                shm: &mut self.shm,
                cell_pixel_w: CELL_PX.0,
                cell_pixel_h: CELL_PX.1,
                anchor_cursor: Some((apc.cursor_row, apc.cursor_col)),
            };
            black_box(dispatch_apc_body(
                &mut ctx,
                &mut self.reassembler,
                &apc.body,
            ));
        }
    }

    /// Every new cluster ships ahead of the rows, where the daemon ships
    /// only the referenced ones and drips the rest: every cluster these
    /// streams register is on screen.
    fn compose(&mut self) {
        self.scratch.clear();
        let registered = self.grid.cluster_count();
        for id in self.clusters_sent + 1..=registered {
            let id = NonZeroU32::new(u32::try_from(id).unwrap()).unwrap();
            let Some(text) = self.grid.cluster_str(id) else {
                continue;
            };
            let msg = GridMsg::Cluster {
                id: id.get(),
                text: text.to_owned(),
            };
            frame_into(&mut self.scratch, MessageKind::Grid, &encode(&msg));
        }
        self.clusters_sent = registered;
        apply_diffs(&self.scratch, &mut self.shadow);
        emit_diffs_with(&mut self.grid, &mut self.scratch, true);
        apply_diffs(&self.scratch, &mut self.shadow);
        for msg in materialize_image_events(&self.events, &self.images, true) {
            image_frame_onto(&mut self.wire, &msg);
        }
        self.events.clear();
        let mut rest = self.wire.split().freeze();
        while !rest.is_empty() {
            let (_, n) = decode_frame(&rest).expect("frame decodes");
            let frame = rest.split_to(n);
            let msg: ImageMsg = decode(frame.slice(HEADER_LEN..)).unwrap();
            self.image_shadow.apply(&msg).expect("image shadow apply");
        }
    }

    fn redraw(&mut self) {
        black_box(self.image_shadow.take_dirty_images());
        self.image_shadow.layout_into(
            &mut self.layout,
            self.shadow.screen(),
            (f32::from(CELL_PX.0), f32::from(CELL_PX.1)),
            self.shadow.viewport(),
        );
    }
}

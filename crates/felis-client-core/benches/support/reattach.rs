//! A long-lived session's daemon state and the attach burst a window
//! reattaching to it receives, shared by the Criterion benches and their
//! instruction-count twin. The burst is the daemon's own composition, so
//! a change to what an attach ships moves both.

use std::fmt::Write as _;
use std::hint::black_box;

use bytes::{BufMut as _, Bytes, BytesMut};
use felis_client_core::{ClientImage, ImageLayout, ImageShadow, ShadowScreen};
use felis_daemon::graphics::{ApcCtx, ImageEvent, ShmDeferral, dispatch_apc_body};
use felis_daemon::serve::{session_task::OutEvent, window_rehydrate};
use felis_grid::{
    Grid, HyperlinkEntry, PtyEffect,
    images::{ImageEntry, ImageStore, Placements},
    push_cell_text,
};
use felis_protocol::{
    MessageKind,
    codec::{decode, encode, encode_image},
    frame::{HEADER_LEN, LEN_OVERHEAD, decode as decode_frame},
    messages::{GridMsg, ImageMsg, MAX_SESSION_IMAGE_BYTES},
};
use felis_vt::{Parser, kitty_graphics::Reassembler};

const ROWS: u16 = 54;
const COLS: u16 = 192;
const CELL_PX: (u16, u16) = (10, 20);
const PTY_READ: usize = 64 * 1024;
const IMAGE_PX: (u32, u32) = (400, 240);
const IMAGE_CELLS: (u32, u32) = (40, 12);

#[derive(Clone, Copy)]
pub(crate) enum Reattach {
    Scrollback10k,
    Scrollback100k,
    Scrollback10kImages,
}

impl Reattach {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Scrollback10k => "scrollback_10k",
            Self::Scrollback100k => "scrollback_100k",
            Self::Scrollback10kImages => "scrollback_10k_images",
        }
    }

    const fn scrollback(self) -> usize {
        match self {
            Self::Scrollback10k | Self::Scrollback10kImages => 10_000,
            Self::Scrollback100k => 100_000,
        }
    }

    const fn images(self) -> u32 {
        match self {
            Self::Scrollback10k | Self::Scrollback100k => 0,
            Self::Scrollback10kImages => 4,
        }
    }

    /// A session whose scrollback is full, so the history has also
    /// evicted, as it has in a session worth reattaching to.
    pub(crate) fn session(self) -> Session {
        let mut session = Session::new(self.scrollback());
        session.feed(&shell_history(self.scrollback() + 2 * usize::from(ROWS)));
        if self.images() > 0 {
            session.feed(&images(self.images()));
        }
        let name = self.name();
        assert_eq!(session.grid.scrollback().len(), self.scrollback(), "{name}");
        assert_eq!(session.images.len(), self.images() as usize, "{name}");
        assert_eq!(session.placements.len(), self.images() as usize, "{name}");
        session
    }
}

/// One cycle per shell command: OSC 133 marks around a prompt, then
/// output of the shapes a build or a log produces (SGR runs, 256-color
/// and truecolor text, wide CJK, combining clusters, hyperlinks, and
/// lines long enough to soft-wrap).
fn shell_history(lines: usize) -> Vec<u8> {
    let mut out = String::with_capacity(lines * 110);
    let mut line = 0;
    let mut cmd = 0usize;
    while line < lines {
        write!(
            out,
            "\x1b]133;A\x07\x1b[1;32muser@host\x1b[0m:\x1b[1;34m~/src/felis\x1b[0m$ \
             \x1b]133;B\x07cargo build -p crate-{cmd}\r\n\x1b]133;C\x07"
        )
        .unwrap();
        line += 1;
        let output = 18 + cmd % 13;
        for n in line..line + output {
            match n % 9 {
                0 => write!(
                    out,
                    "\x1b[33mwarning\x1b[0m: 未使用の変数 `値{n}` は削除できます（{}行目）",
                    n % 400
                ),
                1 => write!(
                    out,
                    "\x1b[38;5;{}m{n:>8}\x1b[0m \x1b[38;2;{};{};200mtrace\x1b[0m span={} \
                     elapsed={}µs",
                    n % 256,
                    n % 200,
                    (n * 7) % 200,
                    n % 97,
                    n % 1000
                ),
                2 => write!(
                    out,
                    "  see \x1b]8;;https://docs.rs/felis/{}\x1b\\docs.rs/felis/{}\x1b]8;;\x1b\\ \
                     for café\u{301} and 👩\u{200d}💻 details",
                    n % 64,
                    n % 64
                ),
                3 => write!(out, "{}", "long-wrapped-output ".repeat(12)),
                4 => write!(out, "\x1b[1m\x1b[4merror[E0{}]\x1b[0m\x1b[2m: mismatched types\x1b[0m", n % 1000),
                _ => write!(
                    out,
                    "\x1b[1;32m   Compiling\x1b[0m felis-grid-{} v0.{}.{} (/home/user/src/felis/crates)",
                    n % 31,
                    n % 9,
                    n % 17
                ),
            }
            .unwrap();
            out.push_str("\r\n");
        }
        line += output;
        out.push_str("\x1b]133;D;0\x07");
        cmd += 1;
    }
    out.into_bytes()
}

fn images(count: u32) -> Vec<u8> {
    let (w, h) = IMAGE_PX;
    let mut out = String::new();
    for id in 1..=count {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                pixels.extend_from_slice(&[(x * id) as u8, (y * 3) as u8, (x ^ y) as u8, 0xff]);
            }
        }
        let mut encoded = Vec::new();
        felis_protocol::base64::encode_into(&pixels, &mut encoded);
        let encoded = String::from_utf8(encoded).unwrap();
        let row = 2 + (id - 1) / 2 * (IMAGE_CELLS.1 + 2);
        let col = 2 + (id - 1) % 2 * (IMAGE_CELLS.0 + 4);
        write!(out, "\x1b[{row};{col}H").unwrap();
        let chunks: Vec<&str> = encoded
            .as_bytes()
            .chunks(4096)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let more = u8::from(i + 1 < chunks.len());
            if i == 0 {
                let (c, r) = IMAGE_CELLS;
                write!(
                    out,
                    "\x1b_Ga=T,f=32,s={w},v={h},i={id},c={c},r={r},C=1,q=2,m={more};"
                )
                .unwrap();
            } else {
                write!(out, "\x1b_Gm={more};").unwrap();
            }
            write!(out, "{chunk}\x1b\\").unwrap();
        }
    }
    out.into_bytes()
}

/// The state a parked session holds for the next window.
pub(crate) struct Session {
    parser: Parser,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    events: Vec<ImageEvent>,
    shm: ShmDeferral,
    reassembler: Reassembler,
}

impl Session {
    fn new(scrollback: usize) -> Self {
        Self {
            parser: Parser::new(),
            grid: Grid::with_scrollback(ROWS, COLS, scrollback),
            images: ImageStore::new(MAX_SESSION_IMAGE_BYTES),
            placements: Placements::new(),
            events: Vec::new(),
            shm: ShmDeferral::default(),
            reassembler: Reassembler::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for read in bytes.chunks(PTY_READ) {
            let mut rest = read;
            loop {
                let stopped = self.parser.advance_until_yield(&mut self.grid, rest);
                self.dispatch();
                let Some(consumed) = stopped else {
                    break;
                };
                rest = &rest[consumed..];
            }
        }
        self.grid.damage_mut().clear();
        self.events.clear();
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
            assert_eq!(
                dispatch_apc_body(&mut ctx, &mut self.reassembler, &apc.body),
                None
            );
        }
    }

    /// The daemon side of an attach: the burst composed and framed as
    /// the connection writes it.
    pub(crate) fn burst(&self) -> Bytes {
        let mut events = Vec::new();
        window_rehydrate(&self.grid, &self.images, &self.placements, &mut events)
            .expect("rehydrate composes");
        let mut wire = BytesMut::new();
        for event in &events {
            let (kind, head, tail) = match event {
                OutEvent::Grid(msg) => (MessageKind::Grid, encode(msg), Bytes::new()),
                OutEvent::Image(msg) => {
                    let (head, tail) = encode_image(msg);
                    (MessageKind::Image, head, tail)
                }
                _ => unreachable!("an attach burst carries grid and image messages only"),
            };
            let len = u32::try_from(head.len() + tail.len())
                .expect("bench bodies are inside the ceiling");
            wire.put_u32_le(len + LEN_OVERHEAD);
            wire.put_u16_le(kind.as_u16());
            wire.extend_from_slice(&head);
            wire.extend_from_slice(&tail);
        }
        wire.freeze()
    }
}

/// A freshly launched window's mirrors, before the burst lands.
pub(crate) struct Window {
    shadow: ShadowScreen,
    image_shadow: ImageShadow,
    layout: ImageLayout,
    /// The shadow keeps no prompt marks, so the check counts them here.
    marks: usize,
}

impl Window {
    pub(crate) fn new() -> Self {
        Self {
            shadow: ShadowScreen::new(ROWS, COLS),
            image_shadow: ImageShadow::new(),
            layout: ImageLayout::new(),
            marks: 0,
        }
    }

    /// The client side of an attach: every frame of the burst decoded
    /// and applied, up to the layout the first frame paints from.
    pub(crate) fn rehydrate(&mut self, mut burst: Bytes) {
        while !burst.is_empty() {
            let (frame, n) = decode_frame(&burst).expect("frame decodes");
            let kind = frame.kind;
            let frame = burst.split_to(n);
            if kind == MessageKind::Grid.as_u16() {
                let msg: GridMsg = decode(&frame[HEADER_LEN..]).unwrap();
                self.marks += usize::from(matches!(msg, GridMsg::PromptMark { .. }));
                self.shadow.apply(&msg).expect("shadow apply");
            } else {
                let msg: ImageMsg = decode(frame.slice(HEADER_LEN..)).unwrap();
                self.image_shadow.apply(&msg).expect("image shadow apply");
            }
        }
        black_box(self.image_shadow.take_dirty_images());
        self.image_shadow.layout_into(
            &mut self.layout,
            self.shadow.screen(),
            (f32::from(CELL_PX.0), f32::from(CELL_PX.1)),
            self.shadow.viewport(),
        );
    }

    /// The window holds the session's screen, its prompt marks and every
    /// image on screen, so a burst that shipped less fails the bench
    /// instead of reading as a win.
    pub(crate) fn check(&self, session: &Session) {
        let screen = self.shadow.screen();
        let grid = &session.grid;
        for r in 0..ROWS {
            for c in 0..COLS {
                let shown = screen.cell(r, c).copied().unwrap_or_default();
                let held = grid.cell(r, c).copied().unwrap_or_default();
                let (mut shown_text, mut held_text) = (String::new(), String::new());
                push_cell_text(&shown, screen.cluster_table(), &mut shown_text);
                push_cell_text(&held, grid.cluster_table(), &mut held_text);
                assert_eq!(shown_text, held_text, "text at {r},{c}");
                assert_eq!(
                    screen.style(shown.style),
                    grid.style(held.style),
                    "style at {r},{c}"
                );
                let uri = |e: Option<&HyperlinkEntry>| e.map(|e| e.uri.as_str().to_owned());
                assert_eq!(
                    uri(shown.link.and_then(|id| screen.hyperlink(id))),
                    uri(held.link.and_then(|id| grid.hyperlink(id))),
                    "link at {r},{c}"
                );
            }
        }
        assert!(self.marks > 0);
        assert_eq!(self.marks, grid.prompt_marks().len(), "prompt marks");
        for id in session.images.iter_ids() {
            let shown = self.image_shadow.image(id).map(ClientImage::pixels);
            let held = session.images.get(id).map(ImageEntry::pixels);
            assert!(shown.is_some() && shown == held, "image {id:?} pixels");
        }
        let laid_out = self.layout.under_bg().len()
            + self.layout.under_text().len()
            + self.layout.above_text().len();
        assert_eq!(laid_out, session.placements.len(), "image draws");
    }
}

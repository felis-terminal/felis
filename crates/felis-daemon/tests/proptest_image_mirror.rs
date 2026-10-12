//! A window mirrors the daemon's images and placements from the events
//! alone across arbitrary Kitty graphics command and screen sequences
//! (`docs/reference/protocols/kitty-graphics.md`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_client_core::image_shadow::ImageShadow;
use felis_daemon::graphics::{
    ApcCtx, ImageEvent, ShmDeferral, apply_screen_switch, clear_images, dispatch_apc_body,
    free_unreferenced_anonymous, materialize_image_events,
};
use std::fmt::Write as _;

use felis_grid::images::{ImageStore, Placements};
use felis_grid::{Grid, PtyEffect};
use felis_vt::Parser;
use felis_vt::kitty_graphics::Reassembler;
use proptest::prelude::*;

mod common;
use common::{b64, body};

/// Three of the 2×2 images the ops send, so eviction runs too.
fn store_cap() -> usize {
    3 * felis_grid::images::ImageEntry::new(
        2,
        2,
        felis_protocol::messages::ImageFormat::Rgba32,
        vec![0; 16],
    )
    .byte_len()
}

#[derive(Debug, Clone)]
enum Op {
    Transmit {
        id: Option<u32>,
        number: bool,
        put: Option<Put>,
    },
    Put {
        id: u32,
        put: Put,
    },
    Delete {
        mode: char,
        id: u32,
        p: Option<u32>,
    },
    Screen(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct Put {
    virtual_: bool,
    p: Option<u32>,
    cols: u16,
}

fn put() -> impl Strategy<Value = Put> {
    (any::<bool>(), proptest::option::of(1u32..4), 1u16..5).prop_map(|(virtual_, p, cols)| Put {
        virtual_,
        p,
        cols,
    })
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (
            proptest::option::weighted(0.8, 1u32..5),
            proptest::bool::weighted(0.2),
            proptest::option::of(put())
        )
            .prop_map(|(id, number, put)| Op::Transmit { id, number, put }),
        (1u32..5, put()).prop_map(|(id, put)| Op::Put { id, put }),
        (
            prop_oneof![
                Just('i'),
                Just('I'),
                Just('r'),
                Just('R'),
                Just('a'),
                Just('A'),
                Just('n'),
                Just('N'),
                Just('w'),
            ],
            1u32..5,
            proptest::option::of(1u32..4),
        )
            .prop_map(|(mode, id, p)| Op::Delete { mode, id, p }),
        prop_oneof![
            Just("\x1b[?1049h"),
            Just("\x1b[?1049l"),
            Just("\x1b[2J"),
            Just("\x1b[3J"),
            Just("\x1bc"),
            Just("\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n"),
        ]
        .prop_map(Op::Screen),
    ]
}

struct Session {
    reassembler: Reassembler,
    parser: Parser,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    saved_primary: Option<Placements>,
    events: Vec<ImageEvent>,
    shm: ShmDeferral,
    window: ImageShadow,
}

impl Session {
    fn new() -> Self {
        Self {
            reassembler: Reassembler::new(),
            parser: Parser::new(),
            grid: Grid::new(6, 20),
            images: ImageStore::new(store_cap()),
            placements: Placements::new(),
            saved_primary: None,
            events: Vec::new(),
            shm: ShmDeferral::default(),
            window: ImageShadow::new(),
        }
    }

    fn apc(&mut self, controls: &str, payload: &[u8]) {
        dispatch_apc_body(
            &mut ApcCtx {
                grid: &mut self.grid,
                images: &mut self.images,
                placements: &mut self.placements,
                events: &mut self.events,
                shm: &mut self.shm,
                cell_pixel_w: 8,
                cell_pixel_h: 16,
                anchor_cursor: None,
                saved_primary: self.saved_primary.as_ref(),
            },
            &mut self.reassembler,
            &body(controls, payload),
        );
    }

    fn screen(&mut self, bytes: &str) {
        self.parser.advance(&mut self.grid, bytes.as_bytes());
        for effect in self.grid.take_pty_effects() {
            match effect {
                PtyEffect::ScreenSwitch(switch) => apply_screen_switch(
                    switch,
                    &mut self.images,
                    &mut self.placements,
                    &mut self.saved_primary,
                    &mut self.events,
                ),
                PtyEffect::ImagesCleared(scope) => clear_images(
                    &mut self.images,
                    &mut self.placements,
                    self.saved_primary.as_ref(),
                    &mut self.events,
                    scope,
                ),
                PtyEffect::HardReset if self.saved_primary.is_some() => apply_screen_switch(
                    felis_grid::ScreenSwitch::LeftAlternate,
                    &mut self.images,
                    &mut self.placements,
                    &mut self.saved_primary,
                    &mut self.events,
                ),
                PtyEffect::ScrolledIntoScrollback(n) => {
                    let retain = u32::try_from(self.grid.scrollback().len()).unwrap();
                    self.shift(n, retain);
                }
                PtyEffect::AltScreenScrolled(n) => self.shift(n, 0),
                _ => {}
            }
        }
    }

    /// `serve::streaming::shift_placements`, which is crate-private.
    fn shift(&mut self, n: u32, retain: u32) {
        if !self.placements.is_empty() {
            self.events.push(ImageEvent::PlacementsShifted { lines: n });
        }
        for p in self.placements.shift_up(n, retain) {
            self.images.release(p.image_id);
            self.events.push(ImageEvent::PlacementRemoved {
                image_id: p.image_id,
                placement_id: p.placement_id,
            });
        }
        free_unreferenced_anonymous(
            &mut self.images,
            &mut self.placements,
            self.saved_primary.as_ref(),
            &mut self.events,
        );
    }

    fn run(&mut self, op: &Op) {
        let put_keys = |put: Put| {
            let mut keys = format!("c={},r=1", put.cols);
            if put.virtual_ {
                keys.push_str(",U=1");
            }
            if let Some(p) = put.p {
                write!(keys, ",p={p}").unwrap();
            }
            keys
        };
        match op {
            Op::Transmit { id, number, put } => {
                let mut controls = String::from("Ga=");
                controls.push(if put.is_some() { 'T' } else { 't' });
                if let Some(id) = id {
                    let key = if *number { 'I' } else { 'i' };
                    write!(controls, ",{key}={id}").unwrap();
                }
                if let Some(put) = put {
                    controls.push(',');
                    controls.push_str(&put_keys(*put));
                }
                controls.push_str(",f=32,s=2,v=2,q=2");
                self.apc(&controls, &b64(&[7; 16]));
            }
            Op::Put { id, put } => {
                self.apc(&format!("Ga=p,i={id},{},q=2", put_keys(*put)), b"");
            }
            Op::Delete { mode, id, p } => {
                let mut controls = match mode {
                    'r' | 'R' => format!("Ga=d,d={mode},x={id},y={}", id + 1),
                    'w' => format!("Ga=d,d=R,x=1,y={}", u32::MAX),
                    'n' | 'N' => format!("Ga=d,d={mode},I={id}"),
                    _ => format!("Ga=d,d={mode},i={id}"),
                };
                if let Some(p) = p {
                    write!(controls, ",p={p}").unwrap();
                }
                self.apc(&controls, b"");
            }
            Op::Screen(bytes) => self.screen(bytes),
        }
    }

    /// The session task drains once per cycle, after however many
    /// commands the PTY read carried.
    fn flush(&mut self) {
        let events = std::mem::take(&mut self.events);
        for msg in materialize_image_events(&events, &self.images) {
            self.window
                .apply(&msg)
                .expect("the daemon sends a well-formed stream");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn a_window_mirrors_the_session_from_its_events(
        ops in proptest::collection::vec((op(), any::<bool>()), 0..24),
    ) {
        let mut s = Session::new();
        for (i, (op, flush)) in ops.iter().enumerate() {
            s.run(op);
            if !flush && i + 1 < ops.len() {
                continue;
            }
            s.flush();

            let mut daemon: Vec<_> = s
                .placements
                .iter()
                .map(|p| (p.image_id.0, p.placement_id.map(|id| id.0), p.anchor.row))
                .collect();
            let mut window: Vec<_> = s
                .window
                .placements()
                .iter()
                .map(|p| (p.image_id.0, p.placement_id.map(|id| id.0), p.anchor_row))
                .collect();
            daemon.sort();
            window.sort();
            prop_assert_eq!(daemon, window, "placements after {:?}", op);

            let wire = |v: &felis_grid::images::VirtualPlacement| (v.image_id, v.placement_id, v.cols, v.rows, v.z_index);
            let daemon: Vec<_> = s.placements.iter_virtual().map(wire).collect();
            let window: Vec<_> = s.window.virtual_placements().iter().map(wire).collect();
            prop_assert_eq!(daemon, window, "virtual placements after {:?}", op);

            // Named ids, I= ids from the lowest free one up, anonymous
            // ids from the top down.
            for id in (1..=16).chain(u32::MAX - 64..=u32::MAX).map(felis_grid::images::ImageId) {
                prop_assert_eq!(
                    s.images.get(id).is_some(),
                    s.window.image(id).is_some(),
                    "image {:?} after {:?}",
                    id,
                    op
                );
            }
        }
    }
}

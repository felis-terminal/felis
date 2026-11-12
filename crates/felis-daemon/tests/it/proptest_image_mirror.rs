//! A window mirrors the daemon's images, frames and placements from the
//! events alone across arbitrary Kitty graphics command and screen
//! sequences (`docs/reference/protocols/kitty-graphics.md`), whether it
//! takes frame edits as regions or as whole frames.

use felis_client_core::image_shadow::ImageShadow;
use felis_daemon::graphics::{
    ApcCtx, ImageEvent, SavedScreens, ShmDeferral, apply_screen_switch, clear_images,
    dispatch_apc_body, free_unreferenced_anonymous, materialize_image_events,
    scroll_region_placements,
};
use std::fmt::Write as _;

use felis_grid::images::{ImageStore, Placements};
use felis_grid::kitty_graphics::Reassembler;
use felis_grid::{Grid, PtyEffect};
use felis_vt::Parser;
use proptest::prelude::*;

use crate::common::{b64, body};

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
    /// `a=f`: append when `edit` is `None`, else edit frame `r=` at
    /// `x`, `y` over `w` × `h`.
    Frame {
        id: u32,
        edit: Option<(u32, u32, u32, u32, u32)>,
    },
    /// `a=c`: `r=` onto `c=`, `w` × `h` from (0, 0) to (`x`, `y`).
    Compose {
        id: u32,
        src: u32,
        dst: u32,
        rect: (u32, u32, u32, u32),
    },
    DeleteFrame {
        id: u32,
        r: u32,
    },
    Jump {
        id: u32,
        c: u32,
    },
}

/// A rectangle that fits the 2×2 images the ops send.
fn rect() -> impl Strategy<Value = (u32, u32, u32, u32)> {
    (0u32..2, 0u32..2).prop_flat_map(|(x, y)| (Just(x), Just(y), 1..=2 - x, 1..=2 - y))
}

#[derive(Debug, Clone, Copy)]
struct Put {
    virtual_: bool,
    p: Option<u32>,
    cols: u16,
    rows: u16,
}

fn put() -> impl Strategy<Value = Put> {
    (
        any::<bool>(),
        proptest::option::of(1u32..4),
        1u16..5,
        1u16..4,
    )
        .prop_map(|(virtual_, p, cols, rows)| Put {
            virtual_,
            p,
            cols,
            rows,
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
            Just("\x1b[?47h"),
            Just("\x1b[?47l"),
            Just("\x1b[2J"),
            Just("\x1b[3J"),
            Just("\x1bc"),
            Just("\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n"),
            Just("\x1b[2;5r\x1b[5;1H\n\x1b[r"),
            Just("\x1b[2;5r\x1b[2;1H\x1bM\x1bM\x1b[r"),
            Just("\x1b[1;4r\x1b[4;1H\n\n\x1b[r"),
            Just("\x1b[H\x1bM"),
        ]
        .prop_map(Op::Screen),
        (1u32..5, proptest::option::of((1u32..4, rect()))).prop_map(|(id, edit)| Op::Frame {
            id,
            edit: edit.map(|(r, (x, y, w, h))| (r, x, y, w, h)),
        }),
        (1u32..5, 1u32..4, 1u32..4, rect()).prop_map(|(id, src, dst, rect)| Op::Compose {
            id,
            src,
            dst,
            rect,
        }),
        (1u32..5, 1u32..4).prop_map(|(id, r)| Op::DeleteFrame { id, r }),
        (1u32..5, 1u32..4).prop_map(|(id, c)| Op::Jump { id, c }),
    ]
}

struct Session {
    reassembler: Reassembler,
    parser: Parser,
    grid: Grid,
    images: ImageStore,
    placements: Placements,
    saved: SavedScreens,
    events: Vec<ImageEvent>,
    shm: ShmDeferral,
    /// Takes frame edits as regions.
    window: ImageShadow,
    /// Below the region minor or not opted in: takes them as whole frames.
    whole: ImageShadow,
    /// Varies every payload, so a stale pixel is told apart.
    fill: u8,
}

impl Session {
    fn new() -> Self {
        Self {
            reassembler: Reassembler::new(),
            parser: Parser::new(),
            grid: Grid::new(6, 20),
            images: ImageStore::new(store_cap()),
            placements: Placements::with_cap(4),
            saved: SavedScreens::default(),
            events: Vec::new(),
            shm: ShmDeferral::default(),
            window: ImageShadow::new(),
            whole: ImageShadow::new(),
            fill: 0,
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
                    &mut self.saved,
                    &mut self.events,
                ),
                PtyEffect::ImagesCleared(scope) => clear_images(
                    &mut self.images,
                    &mut self.placements,
                    &mut self.events,
                    scope,
                ),
                PtyEffect::HardReset => {
                    apply_screen_switch(
                        felis_grid::ScreenSwitch::LeftAlternate { kept: false },
                        &mut self.images,
                        &mut self.placements,
                        &mut self.saved,
                        &mut self.events,
                    );
                    self.saved.alternate = None;
                }
                PtyEffect::ScrolledIntoScrollback(n) => {
                    let retain = u32::try_from(self.grid.scrollback().len()).unwrap();
                    self.shift(n, retain);
                }
                PtyEffect::AltScreenScrolled(n) => self.shift(n, 0),
                PtyEffect::RegionScrolled { op, into_history } => {
                    let retain = u32::try_from(self.grid.scrollback().len()).unwrap();
                    scroll_region_placements(
                        &mut self.images,
                        &mut self.placements,
                        &mut self.events,
                        (op, into_history),
                        (8, 16),
                        retain,
                    );
                }
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
        free_unreferenced_anonymous(&mut self.images, &mut self.placements, &mut self.events);
    }

    fn run(&mut self, op: &Op) {
        let put_keys = |put: Put| {
            let mut keys = format!("c={},r={}", put.cols, put.rows);
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
            Op::Frame { id, edit } => {
                self.fill = self.fill.wrapping_add(1);
                let (keys, w, h) = match edit {
                    None => (String::new(), 2, 2),
                    Some((frame, left, top, w, h)) => {
                        (format!(",r={frame},x={left},y={top}"), *w, *h)
                    }
                };
                let pixels = vec![self.fill; (w * h * 4) as usize];
                self.apc(
                    &format!("Ga=f,i={id}{keys},s={w},v={h},f=32,X=1,q=2"),
                    &b64(&pixels),
                );
            }
            Op::Compose {
                id,
                src,
                dst,
                rect: (x, y, w, h),
            } => {
                self.apc(
                    &format!("Ga=c,i={id},r={src},c={dst},x={x},y={y},w={w},h={h},C=1,q=2"),
                    b"",
                );
            }
            Op::DeleteFrame { id, r } => self.apc(&format!("Ga=d,d=f,i={id},r={r},q=2"), b""),
            Op::Jump { id, c } => self.apc(&format!("Ga=a,i={id},c={c},q=2"), b""),
        }
    }

    /// The session task drains once per cycle, after however many
    /// commands the PTY read carried.
    fn flush(&mut self) {
        let events = std::mem::take(&mut self.events);
        for (window, regions) in [(&mut self.window, true), (&mut self.whole, false)] {
            for msg in materialize_image_events(&events, &self.images, regions) {
                window
                    .apply(&msg)
                    .expect("the daemon sends a well-formed stream");
            }
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
            // The unreferenced-image sweep walks only refcount-0 images.
            for id in s.images.iter_ids() {
                prop_assert_eq!(
                    s.images.get(id).map(|e| e.refcount() as usize),
                    Some(s.placements.for_image(id).count()),
                    "refcount of image {:?} after {:?}", id, op
                );
            }
            if !flush && i + 1 < ops.len() {
                continue;
            }
            s.flush();

            for (shadow, name) in [(&s.window, "window"), (&s.whole, "whole-frame window")] {
                let mut daemon: Vec<_> = s
                    .placements
                    .iter()
                    .map(|p| {
                        (
                            p.image_id.0,
                            p.placement_id.map(|id| id.0),
                            p.anchor.row,
                            p.extent(),
                            p.source.map(|r| (r.x, r.y, r.width, r.height)),
                        )
                    })
                    .collect();
                let mut window: Vec<_> = shadow
                    .placements()
                    .map(|p| {
                        (
                            p.image_id.0,
                            p.placement_id.map(|id| id.0),
                            p.anchor_row,
                            (p.cols, p.rows),
                            p.source.map(|r| (r.x, r.y, r.width, r.height)),
                        )
                    })
                    .collect();
                daemon.sort();
                window.sort();
                prop_assert_eq!(daemon, window, "{} placements after {:?}", name, op);

                let wire = |v: &felis_grid::images::VirtualPlacement| (v.image_id, v.placement_id, v.cols, v.rows, v.z_index);
                let daemon: Vec<_> = s.placements.iter_virtual().map(wire).collect();
                let window: Vec<_> = shadow.virtual_placements().iter().map(wire).collect();
                prop_assert_eq!(daemon, window, "{} virtual placements after {:?}", name, op);

                // Named ids, I= ids from the lowest free one up, anonymous
                // ids from the top down.
                for id in (1..=16).chain(u32::MAX - 64..=u32::MAX).map(felis_grid::images::ImageId) {
                    prop_assert_eq!(
                        s.images.get(id).is_some(),
                        shadow.image(id).is_some(),
                        "image {:?} after {:?}",
                        id,
                        op
                    );
                }

                let shown = shadow.placements().map(|p| p.image_id)
                    .chain(shadow.virtual_placements().iter().map(|v| v.image_id));
                for id in shown {
                    prop_assert!(shadow.image(id).is_some(), "image {:?} is shown but not stored after {:?}", id, op);
                }

                for id in s.images.iter_ids() {
                    let entry = s.images.get(id).unwrap();
                    let held = shadow.image(id).unwrap();
                    prop_assert_eq!(held.frames.len(), entry.frame_count(), "{} frames of {:?} after {:?}", name, id, op);
                    prop_assert_eq!(held.current_frame(), entry.current_frame(), "{} shown frame of {:?} after {:?}", name, id, op);
                    for (i, frame) in held.frames.iter().enumerate() {
                        prop_assert_eq!(&frame.pixels[..], &entry.frame(i).unwrap().pixels[..], "{} frame {} of {:?} after {:?}", name, i, id, op);
                    }
                }
            }
        }
    }
}

//! Kitty graphics on the daemon: dispatch, decode and store (the work the
//! session task does under the parse lock), then the fan-out of a frame's
//! events to subscribers. Workloads replay rich-TUI producers: video
//! players, terminal browsers, image file managers, notcurses.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/kitty.rs"]
mod kitty;

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};
use felis_daemon::graphics::{ImageEvent, image_sync_messages, materialize_image_events};
use felis_grid::images::{AnimationMode, Frame, ImageEntry, ImageFormat, ImageStore};
use felis_protocol::messages::ImageMsg;
use felis_protocol::{ImageId, PROTOCOL_MINOR};
use felis_transport::{CheckedFrame, FrameWriter};
use kitty::{Lcg, Session, command, page_frame, png_rgba, ui_frame, zlib};

type Group<'a> = BenchmarkGroup<'a, WallTime>;
type Command = Vec<Vec<u8>>;

const FULL_FRAMES: [(u32, u32); 2] = [(1920, 1080), (2560, 1600)];

fn dims(w: u32, h: u32) -> String {
    format!("{w}x{h}")
}

fn has_event(s: &Session, pred: impl Fn(&ImageEvent) -> bool) -> bool {
    s.events.iter().any(pred)
}

fn warm_to_quota(s: &mut Session, frame: &Command) {
    while !has_event(s, |e| matches!(e, ImageEvent::Delete { .. })) {
        s.feed(frame);
    }
    s.events.clear();
}

fn video_inline_rgb(g: &mut Group<'_>) {
    for (w, h) in FULL_FRAMES {
        let frame = command(
            &format!("a=T,f=24,s={w},v={h},C=1,q=2"),
            &ui_frame(w, h, 3, 1),
        );
        let mut s = Session::new(w, h);
        warm_to_quota(&mut s, &frame);
        g.throughput(Throughput::Elements(1));
        g.bench_function(format!("video_inline_rgb/at_quota/{}", dims(w, h)), |b| {
            b.iter(|| {
                s.feed(&frame);
                s.events.clear();
            });
        });
    }
}

#[cfg(unix)]
struct ShmSegment(String);

#[cfg(unix)]
impl ShmSegment {
    fn create(name: String, data: &[u8]) -> Self {
        use rustix::shm;
        use std::io::Write;
        let fd = shm::open(
            name.as_str(),
            shm::OFlags::CREATE | shm::OFlags::RDWR | shm::OFlags::TRUNC,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .unwrap();
        std::fs::File::from(fd).write_all(data).unwrap();
        Self(name)
    }
}

#[cfg(unix)]
impl Drop for ShmSegment {
    fn drop(&mut self) {
        let _ = rustix::shm::unlink(self.0.as_str());
    }
}

/// One segment and one file serve every iteration: the daemon defers a
/// `t=s` unlink to session teardown and never deletes a `t=f` file, as
/// mpv relies on when it rewrites one named segment per frame. The
/// anonymous variants keep `vo_kitty`'s own shape: `m=1` with no closing
/// chunk and no `S=`, so the read runs to the end of the segment.
#[cfg(unix)]
fn video_shm(g: &mut Group<'_>) {
    let dir = tempfile::tempdir().unwrap();
    let variants = [
        ("shm_rgb", 24, 3, false),
        ("shm_rgba", 32, 4, false),
        ("file_rgba", 32, 4, false),
        ("shm_replace_rgba", 32, 4, true),
    ];
    for (w, h) in FULL_FRAMES {
        for (variant, f, bpp, replace) in variants {
            let pixels = ui_frame(w, h, bpp, 2);
            let id = if replace { "i=1,p=1," } else { "" };
            let base = format!("a=T,{id}f={f},s={w},v={h},C=1,q=2");
            let (frame, _segment) = if variant == "file_rgba" {
                let path = dir.path().join(format!("frame-{w}x{h}.rgba"));
                std::fs::write(&path, &pixels).unwrap();
                let frame = command(&format!("{base},t=f"), path.to_str().unwrap().as_bytes());
                (frame, None)
            } else {
                let name = format!("/felis-bench-{}-{variant}-{w}", std::process::id());
                let controls = if replace {
                    format!("{base},t=s,S={}", pixels.len())
                } else {
                    format!("{base},t=s,m=1")
                };
                let frame = command(&controls, name.as_bytes());
                (frame, Some(ShmSegment::create(name, &pixels)))
            };
            let mut s = Session::new(w, h);
            if replace {
                s.feed(&frame);
                assert!(s.images.get(ImageId(1)).is_some(), "{variant} rejected");
                s.events.clear();
            } else {
                warm_to_quota(&mut s, &frame);
            }
            g.throughput(Throughput::Elements(1));
            g.bench_function(format!("video_shm/{variant}/{}", dims(w, h)), |b| {
                b.iter(|| {
                    s.feed(&frame);
                    s.events.clear();
                });
            });
        }
    }
}

fn browser_replace(g: &mut Group<'_>) {
    for (w, h) in FULL_FRAMES {
        let pixels = page_frame(w, h, 3);
        let base = format!("a=T,i=1,p=1,f=32,s={w},v={h},C=1,q=2");
        let variants = [
            ("plain", command(&base, &pixels)),
            ("zlib", command(&format!("{base},o=z"), &zlib(&pixels))),
        ];
        for (variant, frame) in variants {
            let mut s = Session::new(w, h);
            s.feed(&frame);
            assert!(s.images.get(ImageId(1)).is_some(), "{variant} rejected");
            g.throughput(Throughput::Elements(1));
            g.bench_function(format!("browser_replace/{variant}/{}", dims(w, h)), |b| {
                b.iter(|| {
                    s.feed(&frame);
                    s.events.clear();
                });
            });
        }
    }
}

fn with_base_image(w: u32, h: u32) -> Session {
    let mut s = Session::new(w, h);
    s.feed(&command(
        &format!("a=T,i=1,p=1,f=32,s={w},v={h},C=1,q=2"),
        &ui_frame(w, h, 4, 4),
    ));
    s.events.clear();
    s
}

fn bench_frame_edits(g: &mut Group<'_>, name: String, mut s: Session, edits: &[Command]) {
    s.feed_all(edits);
    assert!(
        has_event(&s, |e| matches!(e, ImageEvent::TransmitRegion { .. })),
        "{name}: frame edits rejected"
    );
    s.events.clear();
    g.bench_function(name, |b| {
        b.iter(|| {
            s.feed_all(edits);
            s.events.clear();
        });
    });
}

fn browser_dirty_rects(g: &mut Group<'_>) {
    const RECTS: [(u32, u32); 4] = [(64, 64), (128, 128), (256, 128), (512, 256)];
    for (w, h) in FULL_FRAMES {
        for count in [1, 10, 100] {
            for compressed in [false, true] {
                let mut rng = Lcg::new(5);
                let edits: Vec<Command> = (0..count)
                    .map(|k| {
                        let (rw, rh) = RECTS[k % RECTS.len()];
                        let (x, y) = (rng.below(w - rw), rng.below(h - rh));
                        let pixels = page_frame(rw, rh, k as u64);
                        let controls =
                            format!("a=f,i=1,r=1,X=1,x={x},y={y},s={rw},v={rh},f=32,q=2");
                        if compressed {
                            command(&format!("{controls},o=z"), &zlib(&pixels))
                        } else {
                            command(&controls, &pixels)
                        }
                    })
                    .collect();
                let encoding = if compressed { "zlib" } else { "plain" };
                g.throughput(Throughput::Elements(1));
                bench_frame_edits(
                    g,
                    format!("browser_dirty_rects/n{count}_{encoding}/{}", dims(w, h)),
                    with_base_image(w, h),
                    &edits,
                );
            }
        }
    }
}

fn patch_layers(g: &mut Group<'_>) {
    const PATCHES: [(u32, u32); 3] = [(32, 32), (64, 32), (128, 64)];
    let (w, h) = FULL_FRAMES[0];
    for count in [50, 200] {
        for (variant, delete) in [("free", 'I'), ("keep", 'i')] {
            let transmits: Vec<Command> = (0..count)
                .map(|k| {
                    let (pw, ph) = PATCHES[k % PATCHES.len()];
                    let id = 1000 + k;
                    command(
                        &format!("a=T,i={id},f=32,s={pw},v={ph},C=1,z={},q=2", k + 1),
                        &ui_frame(pw, ph, 4, k as u64),
                    )
                })
                .collect();
            let deletes =
                (0..count).map(|k| command(&format!("a=d,d={delete},i={},q=2", 1000 + k), &[]));
            let frame: Vec<Command> = deletes.chain(transmits.iter().cloned()).collect();
            let mut s = with_base_image(w, h);
            s.feed_all(&transmits);
            s.feed_all(&frame);
            assert_eq!(s.images.len(), count + 1, "{variant}: patches rejected");
            s.events.clear();
            g.throughput(Throughput::Elements(1));
            g.bench_function(
                format!("patch_layers/n{count}_{variant}/{}", dims(w, h)),
                |b| {
                    b.iter(|| {
                        s.feed_all(&frame);
                        s.events.clear();
                    });
                },
            );
        }
    }
}

fn png_pingpong(g: &mut Group<'_>) {
    let (w, h) = (1280, 720);
    let frames: Vec<Command> = [1u32, 2]
        .map(|id| {
            let png = png_rgba(w, h, &ui_frame(w, h, 4, u64::from(id)));
            command(&format!("a=T,i={id},p=1,f=100,C=1,q=2"), &png)
        })
        .into();
    let mut s = Session::new(w, h);
    s.feed_all(&frames);
    assert_eq!(s.images.len(), 2, "PNG frames rejected");
    s.events.clear();
    g.throughput(Throughput::Elements(2));
    g.bench_function(format!("png_pingpong/two_ids/{}", dims(w, h)), |b| {
        b.iter(|| {
            s.feed_all(&frames);
            s.events.clear();
        });
    });
}

fn thumbnail_grid(g: &mut Group<'_>) {
    let (w, h) = (800, 600);
    for (variant, f, bpp) in [("rgb", 24, 3), ("rgba", 32, 4)] {
        let mut page: Vec<Command> = (0..20)
            .map(|k| {
                command(
                    &format!("a=T,U=1,i={},f={f},s={w},v={h},c=20,r=10,q=2", 100 + k),
                    &ui_frame(w, h, bpp, k),
                )
            })
            .collect();
        let mut s = Session::new(1920, 1080);
        s.feed_all(&page);
        assert!(
            has_event(&s, |e| matches!(e, ImageEvent::VirtualPlacement { .. })),
            "{variant}: virtual placements rejected"
        );
        page.push(command("a=d,d=A,q=2", &[]));
        s.events.clear();
        g.throughput(Throughput::Elements(1));
        g.bench_function(format!("thumbnail_grid/{variant}/{}", dims(w, h)), |b| {
            b.iter(|| {
                s.feed_all(&page);
                s.events.clear();
            });
        });
    }
}

fn cell_wipes(g: &mut Group<'_>) {
    let (width, height) = FULL_FRAMES[0];
    let (cell_w, cell_h) = (u32::from(kitty::CELL_W), u32::from(kitty::CELL_H));
    let cell = vec![0u8; (cell_w * cell_h * 4) as usize];
    for count in [256, 1000] {
        let mut rng = Lcg::new(8);
        let mut edits: Vec<Command> = (0..count)
            .map(|_| {
                let (x, y) = (
                    rng.below(width / cell_w) * cell_w,
                    rng.below(height / cell_h) * cell_h,
                );
                command(
                    &format!("a=f,i=1,r=1,X=1,x={x},y={y},s={cell_w},v={cell_h},f=32,q=2"),
                    &cell,
                )
            })
            .collect();
        edits.push(command("a=a,i=1,c=1,q=2", &[]));
        g.throughput(Throughput::Elements(count + 1));
        bench_frame_edits(
            g,
            format!("cell_wipes/n{count}/{}", dims(width, height)),
            with_base_image(width, height),
            &edits,
        );
    }
}

fn store_pressure(g: &mut Group<'_>) {
    let tile = command("a=T,f=32,s=128,v=128,C=1,q=2", &ui_frame(128, 128, 4, 9));
    let mut s = Session::new(1920, 1080);
    for _ in 0..4200 {
        s.feed(&tile);
        s.events.clear();
    }
    let resident = s.images.len();
    assert!(
        (4000..=4096).contains(&resident),
        "{resident} images resident"
    );
    g.throughput(Throughput::Elements(1));
    g.bench_function(format!("store_pressure/insert_at_quota/{resident}"), |b| {
        b.iter(|| {
            s.feed(&tile);
            s.events.clear();
        });
    });
    animation(g, s.images);
}

/// The tile under new controls: only the first chunk names them, so a
/// fresh id costs one header rather than a re-encode.
fn with_controls(tile: &Command, controls: &str) -> Command {
    let mut out = tile.clone();
    let body = out[0].iter().position(|&b| b == b';').unwrap() + 1;
    let more = if out.len() > 1 { ",m=1" } else { "" };
    out[0] = [format!("G{controls}{more};").as_bytes(), &out[0][body..]].concat();
    out
}

/// A thumbnail wall drawn with Unicode placeholders: virtual placements
/// take no refcount, so every image is a refcount-0 one that a
/// placement still shows.
fn store_pressure_virtual(g: &mut Group<'_>) {
    let tile = command("a=T,f=32,s=128,v=128,q=2", &ui_frame(128, 128, 4, 11));
    let named = |id: u32| {
        with_controls(
            &tile,
            &format!("a=T,U=1,i={id},f=32,s=128,v=128,c=4,r=2,q=2"),
        )
    };
    let mut s = Session::new(1920, 1080);
    let mut id = 1;
    while !has_event(&s, |e| matches!(e, ImageEvent::Delete { .. })) {
        s.feed(&named(id));
        id += 1;
    }
    s.events.clear();
    let resident = s.images.len();
    assert_eq!(
        resident,
        s.placements.iter_virtual().count(),
        "virtual placements rejected"
    );
    g.throughput(Throughput::Elements(1));
    g.bench_function(
        format!("store_pressure/virtual_insert_at_quota/{resident}"),
        |b| {
            b.iter(|| {
                s.feed(&named(id));
                id += 1;
                s.events.clear();
            });
        },
    );
    g.bench_function(
        format!("store_pressure/virtual_scroll_sweep/{resident}"),
        |b| {
            b.iter(|| {
                felis_daemon::graphics::free_unreferenced_anonymous(
                    &mut s.images,
                    &mut s.placements,
                    &mut s.events,
                );
            });
        },
    );
}

/// `I=` transmissions, each given the lowest id no image holds.
fn store_pressure_numbered(g: &mut Group<'_>) {
    let tile = command(
        "a=T,I=7,f=32,s=128,v=128,C=1,q=2",
        &ui_frame(128, 128, 4, 12),
    );
    let mut s = Session::new(1920, 1080);
    warm_to_quota(&mut s, &tile);
    let resident = s.images.len();
    assert!(resident >= 4000, "{resident} images resident");
    g.throughput(Throughput::Elements(1));
    g.bench_function(
        format!("store_pressure/numbered_insert_at_quota/{resident}"),
        |b| {
            b.iter(|| {
                s.feed(&tile);
                s.events.clear();
            });
        },
    );
}

/// A producer that names its images by `I=` alone and re-places or
/// replaces them by number, over a store of thousands of numbered ones.
fn numbered_churn(g: &mut Group<'_>) {
    let tile = ui_frame(16, 16, 4, 13);
    for resident in [100, 1000, 4000] {
        let transmit = |n: u32| command(&format!("a=T,I={n},p=1,f=32,s=16,v=16,C=1,q=2"), &tile);
        let mut s = Session::new(1920, 1080);
        for n in 1..=resident {
            s.feed(&transmit(n));
        }
        s.events.clear();
        assert_eq!(
            s.images.len(),
            resident as usize,
            "numbered images rejected"
        );
        let mut n = 0;
        g.throughput(Throughput::Elements(1));
        g.bench_function(format!("numbered_churn/put/{resident}"), |b| {
            b.iter(|| {
                n = n % resident + 1;
                s.feed(&command(&format!("a=p,I={n},p=1,C=1,q=2"), &[]));
                s.events.clear();
            });
        });
        g.bench_function(
            format!("numbered_churn/delete_retransmit/{resident}"),
            |b| {
                b.iter(|| {
                    n = n % resident + 1;
                    s.feed(&command(&format!("a=d,d=N,I={n},q=2"), &[]));
                    s.feed(&transmit(n));
                    s.events.clear();
                });
            },
        );
        assert_eq!(s.images.len(), resident as usize, "churn lost images");
    }
}

/// A Unicode-placeholder screen: `placements` virtual placements, and a
/// full grid of placeholder cells tiling 100 of their images.
fn placeholder_screen(g: &mut Group<'_>) {
    use felis_grid::image_layout::{ImageLayout, LayoutImages};
    use felis_protocol::kitty_graphics::placeholder::{PLACEHOLDER, Rank, diacritic_for};

    struct Store<'a>(&'a ImageStore);
    impl LayoutImages for Store<'_> {
        fn drawable_size(&self, id: ImageId) -> Option<(u32, u32)> {
            self.0.get(id).map(|e| (e.width, e.height))
        }
        fn stack_rank(&self, id: ImageId) -> u64 {
            self.0.get(id).map_or(u64::MAX, ImageEntry::serial)
        }
    }

    const TILE: (u32, u32) = (20, 6);
    let pixels = ui_frame(8, 8, 4, 14);
    for placements in [100u32, 1000] {
        let mut s = Session::new(2000, 1200);
        let (rows, cols) = (s.grid.screen().rows(), s.grid.screen().cols());
        for id in 1..=placements {
            s.feed(&command(
                &format!(
                    "a=T,U=1,i={id},p=1,f=32,s=8,v=8,c={},r={},q=2",
                    TILE.0, TILE.1
                ),
                &pixels,
            ));
        }
        let mut text = String::new();
        let rank = |n| diacritic_for(Rank::new(n).unwrap());
        for row in 0..u32::from(rows) {
            write!(text, "\x1b[{};1H", row + 1).unwrap();
            for col in 0..u32::from(cols) {
                let tile = (row / TILE.1) * (u32::from(cols) / TILE.0) + col / TILE.0;
                let id = 1 + tile * placements / 100;
                let [_, r, g, b] = id.to_be_bytes();
                write!(text, "\x1b[38;2;{r};{g};{b}m").unwrap();
                text.extend([PLACEHOLDER, rank(row % TILE.1), rank(col % TILE.0)]);
            }
        }
        felis_vt::Parser::new().advance(&mut s.grid, text.as_bytes());
        let cells = s.grid.screen().placeholder_cells();
        assert_eq!(
            cells.len(),
            usize::from(rows) * usize::from(cols),
            "placeholders dropped"
        );
        assert!(
            cells.iter().all(|c| s
                .placements
                .virtual_for_cell(c.image_id, c.placement_id)
                .is_some()),
            "a placeholder cell resolves to no placement"
        );
        let screen = format!("{placements}vp/{cols}x{rows}");
        g.throughput(Throughput::Elements(cells.len() as u64));
        g.bench_function(
            format!("placeholder_screen/virtual_for_cell/{screen}"),
            |b| {
                b.iter(|| {
                    for c in &cells {
                        black_box(s.placements.virtual_for_cell(c.image_id, c.placement_id));
                    }
                });
            },
        );
        let virtuals: Vec<_> = s.placements.iter_virtual().copied().collect();
        let mut layout = ImageLayout::new();
        g.bench_function(format!("placeholder_screen/layout/{screen}"), |b| {
            b.iter(|| {
                layout.rebuild(
                    s.grid.screen(),
                    &Store(&s.images),
                    [],
                    &virtuals,
                    (f32::from(kitty::CELL_W), f32::from(kitty::CELL_H)),
                    0,
                );
                black_box(layout.under_text().len());
            });
        });
        g.throughput(Throughput::Elements(1));
        let mut id = 0;
        g.bench_function(
            format!("placeholder_screen/put_virtual/{placements}vp"),
            |b| {
                b.iter(|| {
                    id = id % placements + 1;
                    s.feed(&command(
                        &format!("a=p,U=1,i={id},p=1,c={},r={},q=2", TILE.0, TILE.1),
                        &[],
                    ));
                    s.events.clear();
                });
            },
        );
    }
}

/// The session loop asks for the next due frame on every wake-up and
/// advances on every due one, over a store of placed stills with a few
/// animations among them.
fn animation(g: &mut Group<'_>, mut images: ImageStore) {
    const ANIMATED: u32 = 16;
    let stills = images.len();
    for k in 0..ANIMATED {
        let id = ImageId(1_000_000 + k);
        images
            .insert(id, ImageEntry::new(8, 8, ImageFormat::Rgba32, vec![0; 256]))
            .unwrap();
        for _ in 0..2 {
            images
                .push_frame(
                    id,
                    Frame {
                        pixels: vec![0; 256].into(),
                        gap_ms: 40,
                    },
                )
                .unwrap();
        }
        images
            .animation_control(id)
            .unwrap()
            .set_mode(AnimationMode::Running);
        images.retain(id);
    }
    images.advance_animations(0);
    let name = format!("{stills}_stills_{ANIMATED}_animated");
    assert_eq!(
        images.next_animation_due_ms(0),
        Some(0),
        "{name}: nothing animates"
    );
    g.throughput(Throughput::Elements(1));
    g.bench_function(format!("store_pressure/animation_due/{name}"), |b| {
        b.iter(|| black_box(images.next_animation_due_ms(black_box(0))));
    });
    let mut now = 0;
    g.bench_function(format!("store_pressure/animation_tick/{name}"), |b| {
        b.iter(|| {
            now += 40;
            black_box(images.advance_animations(now))
        });
    });
}

/// The parse lock's hold per heavy command, the time the session task
/// can serve no pull, keystroke or other window while it dispatches.
fn lock_hold(c: &mut Criterion) {
    let mut g = c.benchmark_group("graphics_lock_hold");
    g.sample_size(10);
    let (w, h) = (1280, 720);
    let png = png_rgba(w, h, &ui_frame(w, h, 4, 1));
    let png_frame = command("a=T,i=1,p=1,f=100,C=1,q=2", &png);
    bench_hold(&mut g, format!("png/{}", dims(w, h)), (w, h), &png_frame);
    let (w, h) = FULL_FRAMES[0];
    let pixels = ui_frame(w, h, 4, 3);
    let zlib_frame = command(
        &format!("a=T,i=1,p=1,f=32,s={w},v={h},C=1,q=2,o=z"),
        &zlib(&pixels),
    );
    bench_hold(
        &mut g,
        format!("zlib_rgba/{}", dims(w, h)),
        (w, h),
        &zlib_frame,
    );
    #[cfg(unix)]
    {
        let name = format!("/felis-bench-{}-lock-hold", std::process::id());
        let _segment = ShmSegment::create(name.clone(), &pixels);
        let shm_frame = command(
            &format!(
                "a=T,i=1,p=1,f=32,s={w},v={h},C=1,q=2,t=s,S={}",
                pixels.len()
            ),
            name.as_bytes(),
        );
        bench_hold(
            &mut g,
            format!("shm_rgba/{}", dims(w, h)),
            (w, h),
            &shm_frame,
        );
    }
    for (w, h) in FULL_FRAMES {
        frame_holds(&mut g, w, h);
    }
    g.finish();
}

/// `a=f` over a full canvas, the frame's pixels zlib-compressed so its
/// inflate leaves the lock: a new frame composed on frame 1, in RGBA and
/// in RGB the canvas converts, and frame 1 rewritten in place.
fn frame_holds(g: &mut Group<'_>, w: u32, h: u32) {
    let rgba = zlib(&ui_frame(w, h, 4, 15));
    let rgb = zlib(&ui_frame(w, h, 3, 15));
    let geometry = format!("s={w},v={h},o=z,q=2");
    let cases = [
        ("append_rgba", format!("a=f,i=1,c=1,f=32,{geometry}"), &rgba),
        ("append_rgb", format!("a=f,i=1,c=1,f=24,{geometry}"), &rgb),
        (
            "edit_rgba",
            format!("a=f,i=1,r=1,X=1,f=32,{geometry}"),
            &rgba,
        ),
    ];
    for (variant, controls, payload) in cases {
        let edit = command(&controls, payload);
        let mut s = with_base_image(w, h);
        let pop = |s: &mut Session| {
            while s.images.get(ImageId(1)).unwrap().frame_count() > 1 {
                s.images.remove_frame(ImageId(1), 1).unwrap();
            }
            s.events.clear();
        };
        s.feed(&edit);
        assert!(
            has_event(&s, |e| matches!(
                e,
                ImageEvent::TransmitFrame { .. } | ImageEvent::TransmitRegion { .. }
            )),
            "{variant}: frame rejected"
        );
        pop(&mut s);
        g.throughput(Throughput::Elements(1));
        g.bench_function(format!("frame_{variant}/{}", dims(w, h)), |b| {
            b.iter_custom(|iters| {
                (0..iters)
                    .map(|_| {
                        let held = s.hold(&edit);
                        pop(&mut s);
                        held
                    })
                    .sum()
            });
        });
    }
}

fn bench_hold(g: &mut Group<'_>, name: String, (w, h): (u32, u32), frame: &Command) {
    let mut s = Session::new(w, h);
    s.feed(frame);
    assert!(s.images.get(ImageId(1)).is_some(), "{name} rejected");
    s.events.clear();
    g.throughput(Throughput::Elements(1));
    g.bench_function(name, |b| {
        b.iter_custom(|iters| {
            (0..iters)
                .map(|_| {
                    let held = s.hold(frame);
                    s.events.clear();
                    held
                })
                .sum()
        });
    });
}

struct Subscribers {
    runtime: tokio::runtime::Runtime,
    writers: Vec<FrameWriter<tokio::io::Sink>>,
}

impl Subscribers {
    fn new(count: usize) -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            writers: (0..count)
                .map(|_| FrameWriter::new(tokio::io::sink(), PROTOCOL_MINOR))
                .collect(),
        }
    }

    fn send(&mut self, msgs: &[ImageMsg]) {
        let writers = &mut self.writers;
        self.runtime.block_on(async {
            for writer in writers.iter_mut() {
                for msg in msgs {
                    let body = CheckedFrame::encode(msg).unwrap();
                    writer.send_checked_unflushed(&body).await.unwrap();
                }
                writer.flush().await.unwrap();
            }
        });
    }
}

fn fanout_frame(g: &mut Group<'_>) {
    for (w, h) in FULL_FRAMES {
        let mut s = Session::new(w, h);
        s.feed(&command(
            &format!("a=T,i=1,p=1,f=32,s={w},v={h},C=1,q=2"),
            &ui_frame(w, h, 4, 10),
        ));
        let events = std::mem::take(&mut s.events);
        assert!(s.images.get(ImageId(1)).is_some(), "fan-out frame rejected");
        assert!(
            events.iter().any(|e| matches!(e, ImageEvent::Transmit(_))),
            "no transmit event to fan out"
        );
        assert!(
            materialize_image_events(&events, &s.images, true)
                .iter()
                .any(|m| matches!(m, ImageMsg::Chunk { .. })),
            "fan-out materializes no pixel chunks"
        );
        for subscribers in [1, 4] {
            let mut subs = Subscribers::new(subscribers);
            g.throughput(Throughput::Elements(1));
            g.bench_function(format!("frame_rgba/sub{subscribers}/{}", dims(w, h)), |b| {
                b.iter(|| {
                    subs.send(&materialize_image_events(&events, &s.images, true));
                });
            });
        }
    }
}

/// One fan-out of a browser's dirty rectangles: the bytes a window is
/// sent for what the dispatch benches above store.
fn fanout_dirty_rects(g: &mut Group<'_>) {
    const RECTS: [(u32, u32); 4] = [(64, 64), (128, 128), (256, 128), (512, 256)];
    let (width, height) = FULL_FRAMES[0];
    for count in [1, 10] {
        let mut rng = Lcg::new(5);
        let mut s = with_base_image(width, height);
        for k in 0..count {
            let (rect_w, rect_h) = RECTS[k % RECTS.len()];
            let (left, top) = (rng.below(width - rect_w), rng.below(height - rect_h));
            s.feed(&command(
                &format!("a=f,i=1,r=1,X=1,x={left},y={top},s={rect_w},v={rect_h},f=32,q=2"),
                &ui_frame(rect_w, rect_h, 4, k as u64),
            ));
        }
        let events = std::mem::take(&mut s.events);
        assert!(
            materialize_image_events(&events, &s.images, true)
                .iter()
                .any(|m| matches!(m, ImageMsg::Chunk { .. })),
            "dirty rectangles materialize no pixel chunks"
        );
        let mut subs = Subscribers::new(1);
        g.throughput(Throughput::Elements(1));
        g.bench_function(
            format!("dirty_rects/n{count}/{}", dims(width, height)),
            |b| {
                b.iter(|| {
                    subs.send(&materialize_image_events(&events, &s.images, true));
                });
            },
        );
    }
}

fn fanout_rehydrate(g: &mut Group<'_>) {
    let (w, h) = FULL_FRAMES[0];
    let mut s = Session::new(w, h);
    let mut id: u32 = 1;
    while !has_event(&s, |e| matches!(e, ImageEvent::Delete { .. })) {
        s.feed(&command(
            &format!("a=T,i={id},p=1,f=32,s={w},v={h},C=1,q=2"),
            &ui_frame(w, h, 4, u64::from(id)),
        ));
        id += 1;
    }
    let msgs = rehydrate_messages(&s);
    assert!(
        msgs.iter().any(|m| matches!(m, ImageMsg::Placement { .. })),
        "rehydrate carries no placements"
    );
    let mut subs = Subscribers::new(1);
    g.throughput(Throughput::Elements(1));
    g.bench_function(
        format!("rehydrate/near_quota_{}/{}", s.images.len(), dims(w, h)),
        |b| b.iter(|| subs.send(&msgs)),
    );
}

/// Mirrors the daemon's private `compose_image_rehydrate`: every image,
/// then anchored placements, then virtual ones.
fn rehydrate_messages(s: &Session) -> Vec<ImageMsg> {
    let mut msgs = Vec::new();
    for id in s.images.iter_ids() {
        image_sync_messages(&s.images, id, &mut msgs);
    }
    msgs.extend(s.placements.iter().map(|p| ImageMsg::Placement {
        image_id: p.image_id,
        placement_id: p.placement_id,
        anchor_row: p.anchor.row,
        anchor_col: p.anchor.col,
        cols: p.extent().0,
        rows: p.extent().1,
        source: p.source,
        z_index: p.z_index,
        requested: Some(p.requested()),
        serial: Some(p.serial),
    }));
    msgs.extend(
        s.placements
            .iter_virtual()
            .map(|v| ImageMsg::VirtualPlacement {
                image_id: v.image_id,
                placement_id: v.placement_id,
                cols: v.cols,
                rows: v.rows,
                z_index: v.z_index,
            }),
    );
    msgs
}

fn dispatch(c: &mut Criterion) {
    let mut g = c.benchmark_group("graphics_dispatch");
    g.sample_size(10);
    video_inline_rgb(&mut g);
    #[cfg(unix)]
    video_shm(&mut g);
    browser_replace(&mut g);
    browser_dirty_rects(&mut g);
    patch_layers(&mut g);
    png_pingpong(&mut g);
    thumbnail_grid(&mut g);
    cell_wipes(&mut g);
    store_pressure(&mut g);
    store_pressure_virtual(&mut g);
    store_pressure_numbered(&mut g);
    numbered_churn(&mut g);
    placeholder_screen(&mut g);
    g.finish();
}

fn fanout(c: &mut Criterion) {
    let mut g = c.benchmark_group("graphics_fanout");
    g.sample_size(10);
    fanout_frame(&mut g);
    fanout_dirty_rects(&mut g);
    fanout_rehydrate(&mut g);
    g.finish();
}

criterion_group!(benches, dispatch, lock_hold, fanout);
criterion_main!(benches);

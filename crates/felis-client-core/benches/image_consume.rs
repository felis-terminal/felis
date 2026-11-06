//! The GPU-free half of the client's Kitty graphics path: folding
//! `ImageMsg`s into the `ImageShadow`, draining its dirty set, and the
//! per-redraw `ImageLayout` rebuild the main thread runs every frame.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::num::NonZeroU32;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_client_core::{ImageId, ImageLayout, ImageShadow};
use felis_grid::Grid;
use felis_protocol::messages::{ImageMsg, ImageTarget};
use felis_vt::Parser;

#[path = "support/kitty.rs"]
mod kitty;

use kitty::{CELL_PX, COLS, Daemon, ROWS, placeholder_grid, ui_pixels};

fn apply_all(shadow: &mut ImageShadow, msgs: &[ImageMsg]) {
    for msg in msgs {
        shadow.apply(msg).unwrap();
    }
}

fn retransmit(controls: &str, width: u32, height: u32, bpp: usize) -> (ImageShadow, Vec<ImageMsg>) {
    let mut daemon = Daemon::new();
    let controls = format!("{controls},i=1,s={width},v={height},C=1");
    daemon.send(&controls, &ui_pixels(width, height, bpp), (0, 0));
    let msgs = daemon.ship();
    let mut shadow = ImageShadow::new();
    apply_all(&mut shadow, &msgs);
    assert_eq!(shadow.take_dirty_images(), vec![ImageId(1)]);
    (shadow, msgs)
}

/// The daemon ships an `r=1` edit's rectangle alone.
fn subrect_patch() -> (ImageShadow, Vec<ImageMsg>) {
    let mut daemon = Daemon::new();
    daemon.send(
        "a=T,f=32,i=1,s=1920,v=1080,C=1",
        &ui_pixels(1920, 1080, 4),
        (0, 0),
    );
    let mut shadow = ImageShadow::new();
    apply_all(&mut shadow, &daemon.ship());
    daemon.send(
        "a=f,r=1,X=1,i=1,x=640,y=320,s=64,v=64",
        &ui_pixels(64, 64, 4),
        (0, 0),
    );
    (shadow, daemon.ship())
}

/// `ShowFrame` comes from the daemon's playback timer, not a dispatch.
fn animated() -> ImageShadow {
    let mut daemon = Daemon::new();
    daemon.send(
        "a=T,f=32,i=1,s=1920,v=1080,C=1",
        &ui_pixels(1920, 1080, 4),
        (0, 0),
    );
    for x in [0, 64] {
        let controls = format!("a=f,i=1,x={x},y=0,s=64,v=64");
        daemon.send(&controls, &ui_pixels(64, 64, 4), (0, 0));
    }
    let mut shadow = ImageShadow::new();
    apply_all(&mut shadow, &daemon.ship());
    assert_eq!(shadow.image(ImageId(1)).unwrap().frame_count(), 3);
    shadow
        .apply(&ImageMsg::ShowFrame {
            id: ImageId(1),
            number: NonZeroU32::new(3).unwrap(),
        })
        .unwrap();
    assert_eq!(shadow.take_dirty_images(), vec![ImageId(1)]);
    shadow
}

const THUMBNAILS: u16 = 200;
const PLACEMENTS: u16 = 1000;

fn placement_at(n: u16) -> (String, (u16, u16)) {
    let (image, placement) = (n % THUMBNAILS + 1, n / THUMBNAILS + 1);
    let at = ((n * 7) % (ROWS - 2), (n * 13) % (COLS - 4));
    (format!("a=p,i={image},p={placement},c=4,r=2,C=1"), at)
}

fn thumbnail_daemon() -> Daemon {
    let mut daemon = Daemon::new();
    let thumb = ui_pixels(40, 40, 4);
    for id in 1..=THUMBNAILS {
        daemon.send(&format!("a=t,f=32,i={id},s=40,v=40"), &thumb, (0, 0));
    }
    for n in 0..PLACEMENTS {
        let (controls, at) = placement_at(n);
        daemon.send(&controls, &[], at);
    }
    daemon
}

const PLACEHOLDER_IMAGES: u8 = 20;

fn placeholder_screen(daemon: &mut Daemon) -> Grid {
    let pixels = ui_pixels(250, 200, 4);
    let mut text = String::new();
    for k in 0..PLACEHOLDER_IMAGES {
        let id = 201 + k;
        let controls = format!("a=T,U=1,f=32,i={id},s=250,v=200,c=25,r=10");
        daemon.send(&controls, &pixels, (0, 0));
        let at = (u16::from(k / 7) * 10 + 1, u16::from(k % 7) * 25 + 1);
        placeholder_grid(&mut text, id, at, 25, 10);
    }
    let mut grid = Grid::new(ROWS, COLS);
    Parser::new().advance(&mut grid, text.as_bytes());
    grid
}

fn redraw(shadow: &mut ImageShadow, layout: &mut ImageLayout, grid: &Grid) -> Vec<ImageId> {
    let dirty = shadow.take_dirty_images();
    shadow.layout_into(layout, grid.screen(), CELL_PX, 0);
    dirty
}

fn draws(layout: &ImageLayout) -> usize {
    layout.under_bg().len() + layout.under_text().len() + layout.above_text().len()
}

fn bench_transfers(c: &mut Criterion) {
    let mut group = c.benchmark_group("image_consume");
    let cases = [
        ("retransmit/rgba32/1920x1080", "a=T,f=32", 1920, 1080, 4),
        ("retransmit/rgba32/2560x1600", "a=T,f=32", 2560, 1600, 4),
        ("retransmit/rgb24/1920x1080", "a=T,f=24", 1920, 1080, 3),
    ];
    for (name, controls, width, height, bpp) in cases {
        let (mut shadow, msgs) = retransmit(controls, width, height, bpp);
        group.throughput(Throughput::Bytes((width * height) as u64 * bpp as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                apply_all(&mut shadow, black_box(&msgs));
                shadow.take_dirty_images()
            });
        });
    }

    group.finish();

    let mut group = c.benchmark_group("image_consume");
    let (mut shadow, msgs) = subrect_patch();
    assert!(matches!(
        msgs[0],
        ImageMsg::Header {
            target: ImageTarget::Region { .. },
            ..
        }
    ));
    let shipped: usize = msgs
        .iter()
        .map(|m| match m {
            ImageMsg::Chunk { bytes, .. } => bytes.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(shipped, 64 * 64 * 4);
    apply_all(&mut shadow, &msgs);
    assert_eq!(shadow.take_dirty_images(), vec![ImageId(1)]);
    group.bench_function("subrect/rgba32_64x64_patch/1920x1080", |b| {
        b.iter(|| {
            apply_all(&mut shadow, black_box(&msgs));
            shadow.take_dirty_images()
        });
    });

    let mut shadow = animated();
    let shows: Vec<ImageMsg> = (1..=3)
        .map(|n| ImageMsg::ShowFrame {
            id: ImageId(1),
            number: NonZeroU32::new(n).unwrap(),
        })
        .collect();
    let mut next = shows.iter().cycle();
    group.bench_function("animation/show_frame/1920x1080", |b| {
        b.iter(|| {
            shadow.apply(next.next().unwrap()).unwrap();
            shadow.take_dirty_images()
        });
    });
    group.finish();
}

fn bench_redraw(c: &mut Criterion) {
    let mut group = c.benchmark_group("image_consume");
    let blank = Grid::new(ROWS, COLS);

    let mut daemon = thumbnail_daemon();
    let mut placed = ImageShadow::new();
    apply_all(&mut placed, &daemon.ship());
    let mut placeholder_daemon = Daemon::new();
    let placeholder_grid = placeholder_screen(&mut placeholder_daemon);
    let mut tiled = ImageShadow::new();
    apply_all(&mut tiled, &placeholder_daemon.ship());
    let mut both_daemon = thumbnail_daemon();
    placeholder_screen(&mut both_daemon);
    let mut both = ImageShadow::new();
    apply_all(&mut both, &both_daemon.ship());

    let mut layout = ImageLayout::new();
    let idle = [
        (
            "idle_redraw/placements_1000/192x54",
            &mut placed,
            &blank,
            1000,
        ),
        (
            "idle_redraw/placeholders_5000/192x54",
            &mut tiled,
            &placeholder_grid,
            5000,
        ),
        (
            "idle_redraw/both/192x54",
            &mut both,
            &placeholder_grid,
            6000,
        ),
    ];
    for (name, shadow, grid, expected) in idle {
        redraw(shadow, &mut layout, grid);
        assert_eq!(draws(&layout), expected, "{name}");
        group.bench_function(name, |b| {
            b.iter(|| redraw(shadow, &mut layout, black_box(grid)));
        });
    }

    for n in 0..200 {
        let (controls, at) = placement_at(n * 5);
        let (image, placement) = (n * 5 % THUMBNAILS + 1, n * 5 / THUMBNAILS + 1);
        daemon.send(&format!("a=d,d=i,i={image},p={placement}"), &[], (0, 0));
        daemon.send(&controls, &[], (at.0 + 1, at.1 + 1));
    }
    let churn = daemon.ship();
    assert_eq!(churn.len(), 400, "{churn:?}");
    apply_all(&mut placed, &churn);
    redraw(&mut placed, &mut layout, &blank);
    assert_eq!(draws(&layout), 1000);
    group.throughput(Throughput::Elements(churn.len() as u64));
    group.bench_function("churn/placements_200_of_1000/192x54", |b| {
        b.iter(|| {
            apply_all(&mut placed, black_box(&churn));
            redraw(&mut placed, &mut layout, &blank)
        });
    });
    group.finish();
}

criterion_group!(image_consume, bench_transfers, bench_redraw);
criterion_main!(image_consume);

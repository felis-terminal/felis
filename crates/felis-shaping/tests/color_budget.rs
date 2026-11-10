//! Verifies the rasterizer holds its peak heap to the raster budget:
//! glyphs of every source whose charged buffers sit just under it draw,
//! and no buffer they allocate goes uncharged, whatever an earlier glyph
//! left behind and a shrinking reallocation included.

#![cfg(not(windows))]
// Relaxes the workspace `unsafe_code = "deny"` for the counting allocator,
// which forwards every call to `System` unchanged.
#![allow(unsafe_code)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering},
};

use felis_shaping::{FontStack, GlyphBitmap, GlyphPixels, ShapeCache, StyleFaces, test_font_dir};
use skrifa::{
    FontRef, GlyphId, MetadataProvider,
    instance::{LocationRef, Size},
    outline::{DrawSettings, OutlinePen},
};

/// Live and peak heap bytes. A reallocation counts its old block and its
/// new one as live at once, as a relocating one holds them, where dhat
/// counts only the larger.
struct Peak {
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl Peak {
    fn grow(&self, bytes: usize) {
        let live = self.live.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(live, Ordering::Relaxed);
    }

    fn shrink(&self, bytes: usize) {
        self.live.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// Restarts the peak at what is live now, and returns that.
    fn reset(&self) -> usize {
        let live = self.live.load(Ordering::Relaxed);
        self.peak.store(live, Ordering::Relaxed);
        live
    }
}

// SAFETY: every method forwards its arguments to `System` unchanged and
// returns what `System` returned; the counters touch no allocation.
unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` is `System.alloc`'s.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() {
            self.grow(layout.size());
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc_zeroed` is `System.alloc_zeroed`'s.
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() {
            self.grow(layout.size());
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract for `dealloc` is `System.dealloc`'s.
        unsafe { System.dealloc(block, layout) };
        self.shrink(layout.size());
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.grow(new_size);
        // SAFETY: the caller's contract for `realloc` is `System.realloc`'s.
        let moved = unsafe { System.realloc(block, layout, new_size) };
        self.shrink(if moved.is_null() {
            new_size
        } else {
            layout.size()
        });
        moved
    }
}

#[global_allocator]
static HEAP: Peak = Peak {
    live: AtomicUsize::new(0),
    peak: AtomicUsize::new(0),
};

const WORKING_SET: usize = 16 << 20;

/// Room beyond the budget for what no glyph source sizes from the image:
/// the parsed face, its hinting state, and the outline path a rasterizer
/// keeps from its last outline, which follows the font, not the PTY.
/// Measured, no probe here passes the budget by more than 8 KiB.
const OVERHEAD: usize = 64 << 10;

/// Counts an outline's path elements.
#[derive(Default)]
struct Elements(usize);

impl OutlinePen for Elements {
    fn move_to(&mut self, _: f32, _: f32) {
        self.0 += 1;
    }
    fn line_to(&mut self, _: f32, _: f32) {
        self.0 += 1;
    }
    fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {
        self.0 += 1;
    }
    fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {
        self.0 += 1;
    }
    fn close(&mut self) {
        self.0 += 1;
    }
}

/// The pinned CJK subset's glyph with the longest outline.
fn most_complex_cjk_glyph(file: &str) -> Option<u32> {
    let bytes = face(file);
    let font = FontRef::new(&bytes).ok()?;
    let outlines = font.outline_glyphs();
    let settings = || DrawSettings::unhinted(Size::unscaled(), LocationRef::default());
    let elements = |glyph: GlyphId| {
        let mut pen = Elements::default();
        outlines
            .get(glyph)
            .and_then(|outline| outline.draw(settings(), &mut pen).ok())
            .map_or(0, |_| pen.0)
    };
    font.charmap()
        .mappings()
        .map(|(_, glyph)| glyph)
        .max_by_key(|&glyph| elements(glyph))
        .map(GlyphId::to_u32)
}

fn stack(file: &str, family: &str) -> FontStack {
    FontStack::discover_in_files(
        &[test_font_dir().join(file)],
        Some(family),
        &[],
        &[],
        &StyleFaces::default(),
    )
    .unwrap_or_else(|error| panic!("{file}: {error:?}"))
}

fn face(path: &str) -> Vec<u8> {
    let path = test_font_dir().join(path);
    std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn glyph_of(file: &str, c: char) -> Option<u32> {
    let bytes = face(file);
    let glyph = FontRef::new(&bytes).ok()?.charmap().map(c)?;
    Some(glyph.to_u32())
}

fn color(bitmap: &GlyphBitmap) -> (u32, u32) {
    assert!(bitmap.pixels().is_color());
    (bitmap.width(), bitmap.height())
}

fn coverage(bitmap: &GlyphBitmap) -> (u32, u32) {
    assert!(matches!(bitmap.pixels(), GlyphPixels::Coverage(_)));
    (bitmap.width(), bitmap.height())
}

const CJK: &str = "NotoSansCJKjp-Kuzu.otf";
const COLR: &str = "../color-probe/FelisColorProbe.ttf";
const MASK: &str = "../color-probe/FelisBitmapProbe.ttf";
const NOTO: &str = "NotoColorEmoji.ttf";
const COLR_V1: &str = "../colrv1/FelisColrV1Probe.ttf";
const MONASPACE: &str = "opentype/MonaspaceNeon-Regular.otf";

/// A flag ligature, reached by glyph id as no one code point maps it.
const DEEPEST_V1_GLYPH: u32 = 190;

/// One glyph drawn in turn, what its bitmap must be, and the peak heap
/// it may reach.
struct Draw {
    label: &'static str,
    face: usize,
    glyph: u32,
    px: f32,
    expect: fn(&GlyphBitmap),
    peak: usize,
}

const PEAK: usize = WORKING_SET + OVERHEAD;

/// The segment scratch a rasterizer keeps between glyphs, at its cap.
#[cfg(target_os = "macos")]
const SEGMENT_SCRATCH: usize = 16_000 * 16;

/// Every source near the budget, through one rasterizer, after the draws
/// that leave it holding the most between glyphs: the subset's longest
/// outline, flattened into over 2 000 segments, leaves its path, and a
/// `COLR` glyph reserves the segment scratch to its cap.
#[test]
fn color_glyphs_charged_just_under_the_budget_peak_within_it() {
    let faces = [
        stack(CJK, "Noto Sans CJK JP"),
        stack(COLR, "Felis Color Probe"),
        stack(MASK, "Felis Bitmap Probe"),
        stack(NOTO, "Noto Color Emoji"),
        stack(COLR_V1, "Noto Color Emoji"),
        stack(MONASPACE, "Monaspace Neon"),
    ];
    let (cjk, colr, mask, noto, colr_v1) = (0, 1, 2, 3, 4);
    #[cfg(target_os = "macos")]
    let monaspace = 5;
    let colr_glyph = |c| glyph_of(COLR, c).expect("the probe maps it");
    let mask_glyph = |c| glyph_of(MASK, c).expect("the probe maps it");
    #[cfg_attr(not(target_os = "macos"), expect(unused_mut))]
    let mut draws = vec![
        Draw {
            label: "the longest outline",
            face: cjk,
            glyph: most_complex_cjk_glyph(CJK).expect("the CJK subset maps glyphs"),
            px: 40_000.0,
            expect: |_| {},
            peak: PEAK,
        },
        Draw {
            label: "COLR at 16 px",
            face: colr,
            glyph: colr_glyph('A'),
            px: 16.0,
            expect: |b| assert!(color(b).0 > 0),
            peak: PEAK,
        },
        // `C`'s layers lie too far apart for the budget, so it draws its
        // 500 × 700-unit outline: 1140 × 1596 cells, 99.1% of the budget.
        Draw {
            label: "outline near the budget",
            face: colr,
            glyph: colr_glyph('C'),
            px: 2280.0,
            expect: |b| assert!(coverage(b).0 > 1100),
            peak: PEAK,
        },
        // A 2032-pixel-square canvas, 99.96% of the budget, inked across
        // its span: the trimmed image is too large to shrink beside it.
        Draw {
            label: "COLR inked edge to edge",
            face: colr,
            glyph: colr_glyph('E'),
            px: 2032.0,
            expect: |b| assert!(color(b).0 > 2000),
            peak: PEAK,
        },
        // The same canvas inked in one corner: the trimmed image is a
        // speck, shrunk beside the canvas.
        Draw {
            label: "COLR inked in a corner",
            face: colr,
            glyph: colr_glyph('F'),
            px: 2032.0,
            expect: |b| assert!(color(b).0 < 40),
            peak: PEAK,
        },
        // The Noto subset's glyph whose paint nests the most composites,
        // three, at the largest size whose canvas and stacked layers the
        // budget admits.
        Draw {
            label: "COLRv1 layers near the budget",
            face: colr_v1,
            glyph: DEEPEST_V1_GLYPH,
            px: 760.0,
            expect: |b| assert!(color(b).0 > 800),
            peak: PEAK,
        },
        // Noto's 136 × 128 strike scaled to 2023 × 1904: the source, the
        // scratch and the scaled image are 99.95% of the budget.
        Draw {
            label: "PNG strike near the budget",
            face: noto,
            glyph: glyph_of(NOTO, '😀').expect("Noto maps it"),
            px: 1622.0,
            expect: |b| assert_eq!(color(b), (2023, 1904)),
            peak: PEAK,
        },
        // A 255-pixel mask scaled to 1888 pixels: 99.5% of the budget, and
        // the scaled image with its coverage copy would pass it.
        Draw {
            label: "mask near the budget",
            face: mask,
            glyph: mask_glyph('D'),
            px: 237.0,
            expect: |b| assert_eq!(coverage(b), (1888, 1888)),
            peak: PEAK,
        },
        // Scaled to 1593 pixels, the scaled image and its coverage copy
        // fit the budget together, so the coverage is shrunk.
        Draw {
            label: "mask shrunk",
            face: mask,
            glyph: mask_glyph('D'),
            px: 200.0,
            expect: |b| assert_eq!(coverage(b), (1593, 1593)),
            peak: PEAK,
        },
    ];
    // CoreText draws into one canvas, its only image-sized buffer: `M`'s
    // at 5655 px is 3414 × 4839 with its pad, 99.97% of the budget. A
    // pixel larger passes it, so it draws nothing and allocates no image.
    #[cfg(target_os = "macos")]
    draws.extend([
        Draw {
            label: "CoreText canvas near the budget",
            face: monaspace,
            glyph: glyph_of(MONASPACE, 'M').expect("Monaspace maps M"),
            px: 5655.0,
            expect: |b| assert!(coverage(b).0 > 2700),
            peak: PEAK,
        },
        Draw {
            label: "CoreText canvas past the budget",
            face: monaspace,
            glyph: glyph_of(MONASPACE, 'M').expect("Monaspace maps M"),
            px: 5656.0,
            expect: |b| assert_eq!(coverage(b), (0, 0)),
            peak: SEGMENT_SCRATCH + OVERHEAD,
        },
    ]);
    let mut cache = ShapeCache::default();
    let before = HEAP.reset();
    for draw in draws {
        HEAP.reset();
        let bitmap = cache.rasterize_glyph_id(faces[draw.face].primary(), draw.glyph, draw.px);
        let peak = HEAP.peak.load(Ordering::Relaxed) - before;
        (draw.expect)(&bitmap);
        assert!(
            peak <= draw.peak,
            "{}: peak {peak} bytes against a {}-byte bound",
            draw.label,
            draw.peak
        );
    }
}

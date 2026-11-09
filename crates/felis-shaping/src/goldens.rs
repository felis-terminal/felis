//! Cell metrics, face metadata, shaping and color glyphs, pinned against
//! `tests/goldens/`. `UPDATE_GOLDEN=1` rewrites the files (`just golden`).

use std::{fmt::Write as _, path::PathBuf, sync::Arc};

use skrifa::{Tag, raw::types::NameId};

use crate::{
    CellMetrics, Font, FontBytes, FontStack, FontStyle, GlyphBitmap, GlyphId, ShapedGlyph, Shaper,
    StyleFaces,
    metadata::{self, Attributes},
    provider::{FaceSource, ResolvedFace, files::FileSet},
    raster::{RasterOptions, Rasterizer},
    test_font_dir,
};

/// 64-bit FNV-1a.
#[derive(Clone, Copy)]
pub(crate) struct Digest(u64);

impl Default for Digest {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Digest {
    pub(crate) fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
        self
    }

    pub(crate) fn bitmap(&mut self, bitmap: &GlyphBitmap) -> &mut Self {
        let kind = u8::from(bitmap.pixels().is_color());
        self.bytes(&bitmap.left.to_le_bytes())
            .bytes(&bitmap.top.to_le_bytes())
            .bytes(&bitmap.width().to_le_bytes())
            .bytes(&bitmap.height().to_le_bytes())
            .bytes(&[kind])
            .bytes(bitmap.pixels().as_bytes())
    }

    fn glyphs(&mut self, glyphs: &[ShapedGlyph]) -> &mut Self {
        for g in glyphs {
            self.bytes(&g.glyph_id.to_le_bytes())
                .bytes(&g.source_byte_start.to_le_bytes())
                .bytes(&g.source_byte_len.to_le_bytes())
                .bytes(&g.advance_px.to_bits().to_le_bytes())
                .bytes(&g.x_offset_px.to_bits().to_le_bytes())
                .bytes(&g.y_offset_px.to_bits().to_le_bytes());
        }
        self
    }

    pub(crate) fn hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

/// Asserts `actual` equals `tests/goldens/{name}`, or rewrites the file
/// under `UPDATE_GOLDEN`.
pub(crate) fn assert_golden(name: &str, actual: &str) {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests/goldens", name]
        .iter()
        .collect();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; regenerate with `UPDATE_GOLDEN=1`", path.display()));
    let first = expected
        .lines()
        .zip(actual.lines())
        .find(|(e, a)| e != a)
        .map_or_else(
            || {
                format!(
                    "{} lines pinned, {} produced",
                    expected.lines().count(),
                    actual.lines().count()
                )
            },
            |(e, a)| format!("pinned {e:?}, produced {a:?}"),
        );
    assert!(
        expected == actual,
        "`{name}` diverged ({first}); regenerate with `UPDATE_GOLDEN=1` after checking the change is wanted",
    );
}

/// Every face of every file in the pinned set and the color probes, by
/// file name and collection index.
pub(crate) fn every_face() -> Vec<(String, FontBytes, u32)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|entry| entry.expect("a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&test_font_dir(), &mut files);
    walk(&test_font_dir().join("../color-probe"), &mut files);
    let mut faces = Vec::new();
    for path in files {
        let bytes: FontBytes = Arc::new(std::fs::read(&path).expect("a pinned file reads"));
        let name = path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        for index in 0..metadata::face_count(bytes.as_ref().as_ref()) {
            faces.push((format!("{name}#{index}"), bytes.clone(), index));
        }
    }
    faces
}

pub(crate) fn load(bytes: &FontBytes, index: u32) -> Font {
    Font::load_resolved(ResolvedFace {
        source: FaceSource::Shared(bytes.clone()),
        index,
        instance: None,
        slanted: false,
        hinting: None,
    })
    .expect("a pinned face loads")
}

/// The default, every named instance, and 37 steps along each axis.
pub(crate) fn positions(font: &Font) -> Vec<(String, Font)> {
    let mut out = vec![(
        "default".to_owned(),
        font.with_coords(Vec::new(), Vec::new()),
    )];
    for index in 0..font.face.named_instances().len() as usize {
        let view = font.at_instance(index).expect("a listed instance");
        out.push((format!("instance {index}"), view));
    }
    for (tag, value) in axis_sweep(font) {
        let coords = font.normalized_coords(&[(tag, value)]);
        out.push((
            format!("{tag}={value}"),
            font.with_coords(coords, vec![(tag, value)]),
        ));
    }
    out
}

fn axis_sweep(font: &Font) -> Vec<(Tag, f32)> {
    let mut out = Vec::new();
    for axis in font.face.axes().iter() {
        let (min, max) = (axis.min_value(), axis.max_value());
        out.extend((0..=37).map(|step| (axis.tag(), min + (max - min) * step as f32 / 37.0)));
    }
    out
}

/// Every eighth of a pixel to 200, 4 000 seeded sizes in 4–200, and each
/// point size from 6 to 36 at eight scales, as pixels and as points.
#[expect(
    clippy::suboptimal_flops,
    reason = "the sizes the golden was generated at"
)]
pub(crate) fn px_grid() -> Vec<f32> {
    let mut sizes: Vec<f32> = (8..=1600).map(|i| i as f32 / 8.0).collect();
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..4000 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        sizes.push(4.0 + (seed >> 40) as f32 / (1_u64 << 24) as f32 * 196.0);
    }
    for pt in 6..=36 {
        for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 3.0] {
            sizes.push(pt as f32 * 96.0 / 72.0 * scale);
            sizes.push(pt as f32 * scale);
        }
    }
    sizes
}

/// One line per face and position: a digest of every field of the cell
/// `cell` gives at every size of [`px_grid`].
pub(crate) fn cell_metrics(mut cell: impl FnMut(&Font, f32) -> CellMetrics) -> String {
    let sizes = px_grid();
    let mut out = String::new();
    for (name, bytes, index) in every_face() {
        let font = load(&bytes, index);
        for (position, view) in positions(&font) {
            let mut digest = Digest::default();
            for &px in &sizes {
                let CellMetrics {
                    width,
                    height,
                    ascent,
                    glyph_box,
                    pad,
                } = cell(&view, px);
                for value in [width, height, ascent].into_iter().chain(glyph_box) {
                    digest.bytes(&value.to_le_bytes());
                }
                for value in pad {
                    digest.bytes(&value.to_le_bytes());
                }
            }
            let _ = writeln!(out, "{name} {position} {}", digest.hex());
        }
    }
    out
}

fn metadata() -> String {
    let mut out = String::new();
    for (name, bytes, index) in every_face() {
        let face = metadata::model_face(&bytes, index).expect("a pinned face parses");
        let font = load(&bytes, index);
        let Attributes {
            weight,
            italic,
            off_width,
        } = metadata::attributes(bytes.as_ref().as_ref(), index);
        let _ = writeln!(out, "{name}");
        let _ = writeln!(out, "  display {}", font.display_name());
        let _ = writeln!(out, "  families {:?}", metadata::family_names(&face));
        let _ = writeln!(
            out,
            "  every family {:?}",
            metadata::all_family_names(&face)
        );
        let _ = writeln!(
            out,
            "  postscript {:?}",
            metadata::any_name(&face, NameId::POSTSCRIPT_NAME)
        );
        let _ = writeln!(
            out,
            "  weight {weight} italic {italic} off-width {off_width}"
        );
        let _ = writeln!(out, "  color {}", font.has_color_glyphs());
        let mut map = Digest::default();
        let mut mapped = 0_usize;
        for (cp, glyph) in face.charmap().unicodes() {
            if glyph != skrifa::GlyphId::NOTDEF {
                mapped += 1;
                map.bytes(&cp.to_le_bytes())
                    .bytes(&glyph.to_u32().to_le_bytes());
            }
        }
        let _ = writeln!(out, "  cmap {mapped} {}", map.hex());
        for axis in face.axes().iter() {
            let _ = writeln!(
                out,
                "  axis {} {} {} {}",
                axis.tag(),
                axis.min_value(),
                axis.default_value(),
                axis.max_value()
            );
        }
        for instance in face.named_instances().iter() {
            let values: Vec<f32> = instance.user_coords().collect();
            let _ = writeln!(
                out,
                "  instance {:?} {:?} {values:?}",
                metadata::english_name(&face, instance.subfamily_name()),
                instance
                    .postscript_name()
                    .and_then(|id| metadata::any_name(&face, id)),
            );
        }
        let mut coords = Digest::default();
        for (tag, value) in axis_sweep(&font) {
            for coord in font.normalized_coords(&[(tag, value)]) {
                coords.bytes(&coord.to_bits().to_le_bytes());
            }
        }
        if !face.axes().is_empty() {
            let _ = writeln!(out, "  normalized {}", coords.hex());
        }
    }
    out
}

const ROW: &str = "user@host:~/src/felis$ cargo test -p felis-shaping -- --nocapture # => != ->";

const OPERATORS: [&str; 25] = [
    "->", "=>", "!=", "==", "===", "<=", ">=", "<-", "<->", "::", "//", "/*", "&&", "||", "www",
    "mim", "fi", "ffi", "|>", "<|", "-->", "<!--", "...", "#{", "0x1F",
];

const FEATURE_SETS: [&[&str]; 6] = [
    &[],
    &["-calt"],
    &["calt", "liga"],
    &[
        "ss01", "ss02", "ss03", "ss04", "ss05", "ss06", "ss07", "ss08", "ss09",
    ],
    &["ss03"],
    &["-calt", "liga 0", "dlig"],
];

/// Keycaps, ZWJ and modifier sequences, flags, presentation and
/// ideographic variation selectors, and precomposed and decomposed marks.
const CLUSTERS: [&str; 19] = [
    "#\u{FE0F}\u{20E3}",
    "1\u{FE0F}\u{20E3}",
    "*\u{FE0F}\u{20E3}",
    "#\u{20E3}",
    "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
    "\u{2764}\u{FE0F}",
    "\u{2B50}\u{FE0F}",
    "\u{1F004}\u{FE0F}",
    "\u{2B50}\u{FE0E}",
    "\u{1F004}\u{FE0E}",
    "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
    "\u{1F44D}\u{1F3FD}",
    "\u{1F1EF}\u{1F1F5}",
    "葛\u{E0100}",
    "葛\u{E0101}",
    "葛\u{E0102}",
    "\u{E9}",
    "\u{F1}",
    "e\u{301}",
];

/// Printable ASCII, a prompt row and the operators each Monaspace
/// ligature set draws, per face, feature set and size; then each cluster
/// against the pinned fallback chain.
fn shaping() -> String {
    let regular = crate::test_support::pinned("opentype/MonaspaceNeon-Regular.otf");
    let bold = crate::test_support::pinned("truetype/Monaspace Neon Var.ttf")
        .at_style(700, false)
        .expect("the variable face moves to wght 700");
    let ascii: String = (0x20_u8..0x7f).map(char::from).collect();
    let texts: Vec<&str> = [ascii.as_str(), ROW].into_iter().chain(OPERATORS).collect();
    let mut shaper = Shaper::new();
    let mut out = String::new();
    for (face, font) in [("regular", &regular), ("wght 700", &bold)] {
        for set in FEATURE_SETS {
            let features: Vec<String> = set.iter().map(|&f| f.to_owned()).collect();
            for px in [10.0, 13.0, 16.0, 21.5, 40.0] {
                let mut digest = Digest::default();
                for &text in &texts {
                    digest.glyphs(&shaper.shape_run(font, px, &features, text));
                }
                let _ = writeln!(out, "{face} {set:?} {px} {}", digest.hex());
            }
        }
    }
    let mut db = FileSet::default();
    db.add_dir(test_font_dir());
    let stack = FontStack::auto_discover_in(&db, None, &[], &[], &StyleFaces::default())
        .expect("the pinned set loads");
    for text in CLUSTERS {
        let cluster = shaper.shape_cluster(&stack, 18.0, FontStyle::REGULAR, text);
        let mut digest = Digest::default();
        let _ = writeln!(
            out,
            "{text:?} face {} {}",
            cluster.font_id,
            digest.glyphs(&cluster.glyphs).hex()
        );
    }
    out
}

/// One strike glyph of Noto Color Emoji in this many, since the golden
/// runs on every CI platform in a debug build.
const NOTO_STRIDE: usize = 36;

/// The gate sizes, and 16 px at OSC 66 scales 2 and 7.
pub(crate) const CBDT_SIZES: [f32; 8] = [10.0, 13.0, 16.0, 21.5, 24.0, 32.0, 40.0, 112.0];

pub(crate) fn strike_glyphs(font: &Font) -> Vec<GlyphId> {
    let strikes = font
        .face
        .bitmap_strikes_with(skrifa::GlyphSourceOrder::COLOR_BITMAPS);
    (0..font.face.num_glyphs())
        .filter(|&glyph| {
            strikes
                .select_bitmap(skrifa::GlyphId::new(glyph), None)
                .is_some()
        })
        .collect()
}

fn paint_glyphs(font: &Font) -> Vec<GlyphId> {
    let paints = font.face.color_glyphs();
    (0..font.face.num_glyphs())
        .filter(|&glyph| {
            paints
                .get_with_version(
                    skrifa::GlyphId::new(glyph),
                    skrifa::font::glyph::color::ColorGlyphVersion::ColrV1,
                )
                .is_some()
        })
        .collect()
}

/// The hinting a face draws its layers with on each platform and under
/// each fontconfig setting, and unhinted.
pub(crate) fn raster_modes() -> [(&'static str, RasterOptions); 4] {
    use crate::provider::FaceHinting;
    [
        ("unhinted", RasterOptions::NEVER_HINTED),
        ("lcd", RasterOptions::hinted()),
        (
            "slight",
            RasterOptions::for_face(Some(FaceHinting::Slight { autohint: false })),
        ),
        (
            "full",
            RasterOptions::for_face(Some(FaceHinting::Full { autohint: false })),
        ),
    ]
}

pub(crate) fn with_raster(font: &Font, raster: RasterOptions) -> Font {
    let mut font = font.with_coords(font.coords.clone(), font.position.clone());
    font.raster = raster;
    font
}

fn color() -> String {
    let mut out = String::new();
    let mut line = |label: String, font: &Font, glyphs: &[GlyphId], px: f32| {
        let mut rasterizer = Rasterizer::default();
        let mut digest = Digest::default();
        for &glyph in glyphs {
            digest.bitmap(&rasterizer.rasterize(font, glyph, px));
        }
        let _ = writeln!(out, "{label} {px} {}", digest.hex());
    };
    let noto = crate::test_support::pinned("NotoColorEmoji.ttf");
    let glyphs: Vec<GlyphId> = strike_glyphs(&noto)
        .into_iter()
        .step_by(NOTO_STRIDE)
        .collect();
    // Strikes take no hinting, and the face has no outline to fall to.
    let noto = with_raster(&noto, RasterOptions::NEVER_HINTED);
    for px in CBDT_SIZES {
        line("NotoColorEmoji unhinted".to_owned(), &noto, &glyphs, px);
    }
    let colr = crate::test_support::color_probe("FelisColrProbe.ttf");
    let layered: Vec<GlyphId> = colr
        .face
        .charmap()
        .unicodes()
        .map(|(_, glyph)| glyph.to_u32())
        .collect();
    for (mode, raster) in raster_modes() {
        let font = with_raster(&colr, raster);
        for px in 10..=40 {
            line(format!("FelisColrProbe {mode}"), &font, &layered, px as f32);
        }
    }
    for name in [
        "FelisColrV1Probe.ttf",
        "test_glyphs-glyf_colr_1.ttf",
        "test_glyphs-glyf_colr_1_variable.ttf",
    ] {
        let face = crate::test_support::colrv1(name);
        let painted = paint_glyphs(&face);
        for (mode, raster) in raster_modes() {
            let font = with_raster(&face, raster);
            for px in [10.0, 13.0, 16.0, 24.0, 40.0, 100.0] {
                line(format!("{name} {mode}"), &font, &painted, px);
            }
        }
    }
    for (probe, chars) in [
        ("FelisColorProbe.ttf", "ABCD"),
        ("FelisBitmapProbe.ttf", "ABCFGHIJKL"),
        ("FelisSbixProbe.ttf", "ABC"),
        ("FelisHintedProbe.ttf", "AB"),
    ] {
        let face = crate::test_support::color_probe(probe);
        let glyphs: Vec<GlyphId> = chars.chars().filter_map(|c| face.glyph_for(c)).collect();
        for (mode, raster) in raster_modes() {
            let font = with_raster(&face, raster);
            for px in [10.0, 13.0, 16.0, 20.0, 20.5, 21.0, 26.5, 40.0, 100.0] {
                line(format!("{probe} {mode}"), &font, &glyphs, px);
            }
        }
    }
    out
}

#[test]
fn cell_metrics_match_their_golden() {
    assert_golden("cell_metrics.txt", &cell_metrics(Font::cell_metrics));
}

#[test]
fn face_metadata_matches_its_golden() {
    assert_golden("metadata.txt", &metadata());
}

#[test]
fn shaping_matches_its_golden() {
    assert_golden("shaping.txt", &shaping());
}

#[test]
fn color_glyphs_match_their_golden() {
    assert_golden("color.txt", &color());
}

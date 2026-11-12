#![doc = include_str!("../README.md")]
// Relaxes the workspace `unsafe_code = "deny"` for the `Mmap::map` in
// `Font::load_face_id` and the libfontconfig calls in
// `provider::fontconfig` (docs/explanation/rendering/text-shaping.md
// "Font loading").
#![allow(unsafe_code)]

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use harfrust::{
    Buffer, BufferFlags, Direction, Feature, Script, ShapeOptions, ShapePlan, ShaperFont, Tag,
};
use skrifa::{
    GlyphSourceOrder, MetadataProvider, PathSink, ScalerFont,
    instance::{LocationRef, Size},
    raw::{
        model::{Blob, TableFunction},
        types::{F2Dot14, NameId},
    },
};
use thiserror::Error;

pub type GlyphId = u32;

use crate::raster::{RasterOptions, Rasterizer};
use tracing::{debug, warn};

#[cfg(test)]
#[cfg(not(windows))]
mod goldens;
mod metadata;
mod presentation;
mod provider;
mod raster;
#[cfg(test)]
#[cfg(not(windows))]
mod test_support;

use presentation::is_emoji_presentation;
pub use provider::MONOSPACE_FALLBACK_FAMILIES;
use provider::{
    BOLD_WEIGHT, FaceHinting, FaceProvider, FaceSource, LoadedAs, REGULAR_WEIGHT, ResolvedFace,
    SystemFallback, files::FileSet,
};

#[derive(Debug, Error)]
pub enum ShapingError {
    #[error("no monospace font installed")]
    NoFont,
    #[error("parse font face: {0}")]
    BadFont(String),
    #[error("fontconfig: {0}")]
    Fontconfig(String),
    #[error("DirectWrite: {0}")]
    DirectWrite(String),
    #[error("load font {}: {detail}", path.display())]
    FontPath {
        path: std::path::PathBuf,
        detail: String,
    },
}

type FontBytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// Single-sourced so `flake.nix`'s pinned test-font dir, the loader, and
/// every feature test agree on the face.
#[doc(hidden)]
pub const TEST_FONT_FAMILY: &str = "Monaspace Neon";

/// Rasterizes the outline `draw` emits at `px` through the outline path's
/// rasterizer, for the fuzz target.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn rasterize_path(px: f32, draw: impl FnOnce(&mut dyn PathSink)) -> GlyphBitmap {
    raster::rasterize_path(px, draw)
}

/// Draws `glyph` of the first face in `bytes` at `px` through the whole
/// rasterizer, color sources included, for the fuzz target and the heap
/// tests; `None` when the bytes hold no face.
#[cfg(feature = "testing")]
#[doc(hidden)]
#[must_use]
pub fn rasterize_face_glyph(bytes: Vec<u8>, glyph: GlyphId, px: f32) -> Option<GlyphBitmap> {
    let font = Font::load_resolved(ResolvedFace {
        source: FaceSource::Shared(Arc::new(bytes)),
        index: 0,
        instance: None,
        slanted: false,
        hinting: None,
    })
    .ok()?;
    Some(Rasterizer::default().rasterize(&font, glyph, px))
}

/// Draws each of `glyphs` of the first face in `bytes` at `px` unhinted,
/// as a reference renderer drawing without hinting does, for the color
/// reference comparison; `None` when the bytes hold no face.
#[cfg(feature = "testing")]
#[doc(hidden)]
#[must_use]
pub fn rasterize_face_glyphs_unhinted(
    bytes: Vec<u8>,
    glyphs: &[GlyphId],
    px: f32,
) -> Option<Vec<GlyphBitmap>> {
    let mut font = Font::load_resolved(ResolvedFace {
        source: FaceSource::Shared(Arc::new(bytes)),
        index: 0,
        instance: None,
        slanted: false,
        hinting: None,
    })
    .ok()?;
    font.raster = RasterOptions::NEVER_HINTED;
    let mut rasterizer = Rasterizer::default();
    Some(
        glyphs
            .iter()
            .map(|&glyph| rasterizer.rasterize(&font, glyph, px))
            .collect(),
    )
}

/// The pinned test-font set `$FELIS_TEST_FONT_DIR` names; panics when
/// it is unset.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
#[must_use]
pub fn test_font_dir() -> std::path::PathBuf {
    std::env::var_os("FELIS_TEST_FONT_DIR")
        .unwrap_or_else(|| {
            panic!("FELIS_TEST_FONT_DIR is unset: run the tests in the dev shell, which sets it")
        })
        .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FontStyle {
    pub bold: bool,
    pub italic: bool,
}

impl FontStyle {
    pub const REGULAR: Self = Self {
        bold: false,
        italic: false,
    };

    #[must_use]
    pub const fn index(self) -> usize {
        (self.bold as usize) | ((self.italic as usize) << 1)
    }

    #[must_use]
    pub const fn from_index(i: usize) -> Self {
        Self {
            bold: i & 0b01 != 0,
            italic: i & 0b10 != 0,
        }
    }
}

pub struct Font {
    bytes: FontBytes,
    index: u32,
    /// Whether the color sources are tried for this view.
    color: bool,
    /// Whether the face has color tables, which an [`Font::outlines_only`]
    /// view keeps.
    color_tables: bool,
    /// Variation-axis position, one entry per axis; empty for a static
    /// face. Every scaler, shaper and metrics call must pass it, or a
    /// variable face draws its default instance whatever weight was asked.
    coords: Vec<F2Dot14>,
    /// The axis settings behind `coords`, clamped to each axis's range,
    /// in design units: what [`Font::display_name`] names. Normalizing
    /// passes through `avar`, so `coords` cannot be read back exactly.
    position: Vec<(Tag, f32)>,
    /// The face at `coords`, which the shaper and the outline scaler
    /// read.
    face: skrifa::Font,
    /// Identifies this face at this position to the shape plan and
    /// outline scaler caches.
    id: u64,
    raster: RasterOptions,
}

impl Font {
    pub fn load_default() -> Result<Self, ShapingError> {
        let provider = system_provider()?;
        let (id, _) = provider.default_monospace(REGULAR_WEIGHT)?;
        Self::load_face_id(&provider, id)
    }

    fn try_load_with<P: FaceProvider>(provider: &P, family: &str) -> Result<Self, ShapingError> {
        let id = provider
            .find_face(family, REGULAR_WEIGHT, false)
            .ok_or(ShapingError::NoFont)?;
        Self::load_face_id(provider, id)
    }

    /// Whether `self` maps every codepoint `regular` maps. A view moved
    /// along its own axes, or a sibling file with a byte-identical cmap
    /// (Noto Sans CJK's static weights), passes without walking 65k
    /// codepoints.
    fn covers_all_of(&self, regular: &Self) -> bool {
        if Arc::ptr_eq(&self.bytes, &regular.bytes) && self.index == regular.index {
            return true;
        }
        if let Some(own) = self.cmap_bytes()
            && regular.cmap_bytes() == Some(own)
        {
            return true;
        }
        let own = self.face.charmap();
        regular
            .face
            .charmap()
            .unicodes()
            .all(|(cp, glyph)| glyph == skrifa::GlyphId::NOTDEF || own.map_unicode(cp).is_some())
    }

    fn cmap_bytes(&self) -> Option<&[u8]> {
        let font = skrifa::FontRef::from_index(self.bytes.as_ref().as_ref(), self.index).ok()?;
        Some(font.table_data(Tag::new(b"cmap"))?.as_bytes())
    }

    /// The pinned probe font; panics when the fixture is absent.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    #[must_use]
    pub fn load_test_font() -> Self {
        Self::load_test_font_from(&test_font_dir())
    }

    fn load_test_font_from(dir: &std::path::Path) -> Self {
        let mut set = FileSet::default();
        set.add_dir(dir);
        Self::try_load_with(&set, TEST_FONT_FAMILY).unwrap_or_else(|_| {
            panic!(
                "{TEST_FONT_FAMILY:?} did not load from FELIS_TEST_FONT_DIR={}",
                dir.display()
            )
        })
    }

    /// Load the test font when `$FELIS_TEST_FONT_DIR` is set, else default.
    ///
    /// Benchmarks load through this to prevent host-dependent baselines and
    /// failures on font-less CI runners.
    pub fn load_test_font_or_default() -> Result<Self, ShapingError> {
        std::env::var_os("FELIS_TEST_FONT_DIR").map_or_else(Self::load_default, |dir| {
            Ok(Self::load_test_font_from(std::path::Path::new(&dir)))
        })
    }

    fn load_face_id<P: FaceProvider>(provider: &P, id: P::Id) -> Result<Self, ShapingError> {
        Self::load_resolved(provider.resolve_face(id).ok_or(ShapingError::NoFont)?)
    }

    fn load_resolved(face: ResolvedFace) -> Result<Self, ShapingError> {
        let (face_index, instance, hinting) = (face.index, face.instance, face.hinting);
        let bytes: FontBytes = match face.source {
            FaceSource::Shared(bytes) => bytes,
            FaceSource::File(path) => Arc::new(map_face_file(&path)?),
        };
        let face = untracked_face(&bytes, face_index)
            .ok_or_else(|| ShapingError::BadFont("skrifa::Font::new returned None".into()))?;
        let color = !face.palettes().is_empty()
            || !face
                .bitmap_strikes_with(GlyphSourceOrder::COLOR_BITMAPS)
                .is_empty();
        let font = Self {
            bytes,
            index: face_index,
            color,
            color_tables: color,
            coords: Vec::new(),
            position: Vec::new(),
            face,
            id: next_font_id(),
            raster: RasterOptions::for_face(hinting),
        };
        Ok(match instance {
            Some(index) => font.at_instance(index).ok_or_else(|| {
                ShapingError::BadFont(format!("named instance {index} not in the face"))
            })?,
            None => font,
        })
    }

    /// The face at named instance `index` (zero-based, in `fvar` order);
    /// `None` when the face has no such instance.
    fn at_instance(&self, index: usize) -> Option<Self> {
        let instance = self
            .face
            .named_instances()
            .get(u32::try_from(index).ok()?)?;
        let settings: Vec<(Tag, f32)> = self
            .face
            .axes()
            .iter()
            .map(|axis| axis.tag())
            .zip(instance.user_coords())
            .collect();
        let coords = self.normalized_coords(&settings);
        Some(self.with_coords(coords, settings))
    }

    fn normalized_coords(&self, settings: &[(Tag, f32)]) -> Vec<F2Dot14> {
        metadata::normalized_coords(self.bytes.as_ref().as_ref(), self.index, settings)
    }

    /// The regular view a stack draws at `weight` and `axes`, `None` for
    /// `self` unmoved. With `weight` unset, a face loaded at a named
    /// instance keeps its `wght` and any other variable face moves to 400.
    fn regular_view(&self, weight: Option<u16>, axes: &[AxisSetting]) -> Option<Self> {
        let weight = weight.or_else(|| self.coords.is_empty().then_some(REGULAR_WEIGHT));
        if weight.is_none() && axes.is_empty() {
            return None;
        }
        self.at_position(weight, false, axes)
    }

    fn with_coords(&self, coords: Vec<F2Dot14>, position: Vec<(Tag, f32)>) -> Self {
        let face = self
            .face
            .instance_builder()
            .normalized_coords(coords.iter().copied())
            .build();
        Self {
            bytes: self.bytes.clone(),
            index: self.index,
            color: self.color,
            color_tables: self.color_tables,
            coords,
            position,
            face,
            id: next_font_id(),
            raster: self.raster,
        }
    }

    /// The same face at `weight` on its `wght` axis (clamped to the
    /// axis range) and, when `italic`, at the [`italic_position`] of its
    /// `ital`/`slnt` axes, sharing the font bytes; `None` when neither
    /// applies. Axes neither setting names keep `self`'s position, so a
    /// face loaded at a named instance keeps its width or optical size.
    #[must_use]
    pub fn at_style(&self, weight: u16, italic: bool) -> Option<Self> {
        self.at_position(Some(weight), italic, &[])
    }

    /// [`Self::at_style`] with `wght` left where it is when `weight` is
    /// `None`, then `axes` applied over it. An explicit `ital` or `slnt`
    /// replaces the italic position; `wght` and axes the face lacks are
    /// skipped.
    fn at_position(&self, weight: Option<u16>, italic: bool, axes: &[AxisSetting]) -> Option<Self> {
        let face_axes = self.face.axes();
        let axes_info: Vec<AxisInfo> = face_axes
            .iter()
            .map(|v| AxisInfo {
                tag: v.tag(),
                default: v.default_value(),
            })
            .collect();
        let has_axis = |tag| axes_info.iter().any(|a| a.tag == tag);
        let explicit: Vec<(Tag, f32)> = axes
            .iter()
            .map(|a| (Tag::new(&a.tag), a.value))
            .filter(|&(tag, _)| tag != WGHT && has_axis(tag))
            .collect();
        let explicit_slant = explicit.iter().any(|&(tag, _)| tag == ITAL || tag == SLNT);
        let target = f32::from(weight.unwrap_or(REGULAR_WEIGHT));
        let mut settings = Vec::new();
        if weight.is_some() && has_axis(WGHT) {
            settings.push((WGHT, target));
        }
        if italic && !explicit_slant {
            let instances: Vec<InstanceInfo> = self
                .face
                .named_instances()
                .iter()
                .map(|i| InstanceInfo {
                    name: metadata::english_name(&self.face, i.subfamily_name())
                        .unwrap_or_default(),
                    values: i.user_coords().collect(),
                })
                .collect();
            settings.extend(italic_position(target, &axes_info, &instances));
        }
        for (tag, value) in explicit {
            settings.retain(|&(t, _)| t != tag);
            settings.push((tag, value));
        }
        if settings.is_empty() {
            return None;
        }
        let mut position = self.position.clone();
        for &(tag, value) in &settings {
            let value = face_axes.get_by_tag(tag).map_or(value, |axis| {
                value.clamp(axis.min_value(), axis.max_value())
            });
            match position.iter_mut().find(|(t, _)| *t == tag) {
                Some(entry) => entry.1 = value,
                None => position.push((tag, value)),
            }
        }
        let mut coords = self.normalized_coords(&settings);
        for ((coord, axis), base) in coords.iter_mut().zip(&axes_info).zip(&self.coords) {
            if !settings.iter().any(|(tag, _)| *tag == axis.tag) {
                *coord = *base;
            }
        }
        Some(self.with_coords(coords, position))
    }

    #[must_use]
    pub fn has_glyph(&self, c: char) -> bool {
        self.glyph_for(c).is_some()
    }

    /// `None` when the face does not map `c`.
    fn glyph_for(&self, c: char) -> Option<GlyphId> {
        self.face
            .charmap()
            .map_unicode(c)
            .map(skrifa::GlyphId::to_u32)
    }

    /// `None` when the face does not map `c`.
    #[must_use]
    pub fn advance_px(&self, c: char, px: f32) -> Option<f32> {
        self.advance_width(self.glyph_for(c)?, px)
    }

    /// `hmtx` and `HVAR` alone miss a varied advance; skrifa falls back
    /// to `gvar`'s phantom points, which a face without `HVAR` (Monaspace's
    /// variable builds) varies its width through.
    fn advance_width(&self, glyph_id: GlyphId, px: f32) -> Option<f32> {
        let font = skrifa::FontRef::from_index(self.bytes.as_ref().as_ref(), self.index).ok()?;
        font.glyph_metrics(Size::new(px), LocationRef::new(&self.coords))
            .advance_width(skrifa::GlyphId::new(glyph_id))
    }

    #[must_use]
    pub const fn has_color_glyphs(&self) -> bool {
        self.color
    }

    /// A CBDT face carries an empty `glyf` entry under each bitmap.
    fn has_outline(&self, c: char) -> bool {
        #[derive(Default)]
        struct Inked(bool);
        impl PathSink for Inked {
            fn move_to(&mut self, _: f32, _: f32) {
                self.0 = true;
            }
            fn line_to(&mut self, _: f32, _: f32) {
                self.0 = true;
            }
            fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {
                self.0 = true;
            }
            fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {
                self.0 = true;
            }
            fn close(&mut self) {}
        }
        let glyph = self.glyph_for(c).unwrap_or_default();
        let mut inked = Inked::default();
        ScalerFont::force_unhinted(self.face.clone(), None)
            .outline(skrifa::GlyphId::new(glyph))
            .is_some_and(|outline| {
                outline
                    .draw(skrifa::DrawMemory::Scratch, &mut inked)
                    .is_ok()
                    && inked.0
            })
    }

    /// The same face with its color tables left unread, for a scalar
    /// drawn in text presentation.
    fn outlines_only(&self) -> Self {
        Self {
            color: false,
            ..self.with_coords(self.coords.clone(), self.position.clone())
        }
    }

    /// The `name` table's full name, English first. A variation view
    /// carries its file's default-instance name, so the axis position it
    /// draws at follows it, as `(wght=700, slnt=-11)`.
    #[must_use]
    pub fn display_name(&self) -> String {
        let name = [NameId::FULL_NAME, NameId::FAMILY_NAME]
            .into_iter()
            .find_map(|id| metadata::english_name(&self.face, id))
            .unwrap_or_else(|| "unnamed".to_owned());
        if self.position.is_empty() {
            return name;
        }
        let axes = self
            .position
            .iter()
            .map(|(tag, value)| {
                let tag = String::from_utf8_lossy(&tag.to_be_bytes())
                    .trim_end()
                    .to_owned();
                format!("{tag}={value}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{name} ({axes})")
    }

    #[must_use]
    pub fn cell_metrics(&self, px: f32) -> CellMetrics {
        let width = self.advance_px('M', px).unwrap_or(px * 0.6);
        let (ascent, descent, leading) = self.line_metrics(px);
        CellMetrics::from_scaled(width, ascent, descent, leading)
    }

    /// Ascent, descent (positive below the baseline) and line gap at `px`.
    fn line_metrics(&self, px: f32) -> (f32, f32, f32) {
        let metrics = self.face.metrics();
        let Some(line) = metrics.h_line() else {
            return (0.0, 0.0, 0.0);
        };
        let scale = px / f32::from(metrics.units_per_em);
        (
            line.ascender.to_f32() * scale,
            -line.descender.to_f32() * scale,
            line.line_gap.to_f32() * scale,
        )
    }
}

fn next_font_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

const TRAK: Tag = Tag::new(b"trak");

/// The face at `index` in `bytes`, its tables read in place, with no
/// `trak` table. harfrust tracks every face that has `trak` and `STAT`,
/// with no feature or option to turn it off, and tracking only moves the
/// pens of a grid whose cell width the face's `M` advance already fixed.
fn untracked_face(bytes: &FontBytes, index: u32) -> Option<skrifa::Font> {
    let data = bytes.as_ref().as_ref();
    let directory = skrifa::FontRef::from_index(data, index).ok()?;
    let tables: Vec<(Tag, Range<usize>)> = directory
        .table_directory()
        .table_records()
        .iter()
        .filter_map(|record| {
            let start = usize::try_from(record.offset()).ok()?;
            let end = start.checked_add(usize::try_from(record.length()).ok()?)?;
            (end <= data.len()).then_some((record.tag(), start..end))
        })
        .collect();
    let bytes = bytes.clone();
    let table = move |tag: Tag| {
        if tag == TRAK {
            return None;
        }
        let (_, range) = tables.iter().find(|(t, _)| *t == tag)?;
        let slice: FontBytes = Arc::new(TableSlice {
            bytes: bytes.clone(),
            range: range.clone(),
        });
        Some(Blob::Shared(slice))
    };
    skrifa::Font::new(TableFunction::new(Arc::new(table)), 0)
}

struct TableSlice {
    bytes: FontBytes,
    range: Range<usize>,
}

impl AsRef<[u8]> for TableSlice {
    fn as_ref(&self) -> &[u8] {
        self.bytes
            .as_ref()
            .as_ref()
            .get(self.range.clone())
            .unwrap_or_default()
    }
}

const WGHT: Tag = Tag::new(b"wght");
const ITAL: Tag = Tag::new(b"ital");
const SLNT: Tag = Tag::new(b"slnt");

struct AxisInfo {
    tag: Tag,
    default: f32,
}

struct InstanceInfo {
    name: String,
    /// Design-space values in fvar axis order.
    values: Vec<f32>,
}

/// The `ital`/`slnt` settings for an italic view of an upright variable
/// face: the italic named instance nearest `target_wght` among those
/// whose other axes sit at their defaults, else `ital` = 1, else none.
/// STAT is not read (docs/explanation/rendering/text-shaping.md
/// "Per-style faces").
fn italic_position(
    target_wght: f32,
    axes: &[AxisInfo],
    instances: &[InstanceInfo],
) -> Vec<(Tag, f32)> {
    let is_italic_axis = |tag| tag == ITAL || tag == SLNT;
    if !axes.iter().any(|a| is_italic_axis(a.tag)) {
        return Vec::new();
    }
    let wght = axes.iter().position(|a| a.tag == WGHT);
    let distance = |inst: &InstanceInfo| wght.map_or(0.0, |w| (inst.values[w] - target_wght).abs());
    let nearest = instances
        .iter()
        .filter(|inst| inst.values.len() == axes.len() && has_italic_token(&inst.name))
        .filter(|inst| {
            axes.iter().zip(&inst.values).all(|(a, v)| {
                a.tag == WGHT || is_italic_axis(a.tag) || v.to_bits() == a.default.to_bits()
            })
        })
        .min_by(|a, b| distance(a).total_cmp(&distance(b)));
    if let Some(inst) = nearest {
        return axes
            .iter()
            .zip(&inst.values)
            .filter(|(a, _)| is_italic_axis(a.tag))
            .map(|(a, v)| (a.tag, *v))
            .collect();
    }
    if axes.iter().any(|a| a.tag == ITAL) {
        return vec![(ITAL, 1.0)];
    }
    Vec::new()
}

/// Splits on spaces, `-`, `_` and lowercase-to-uppercase boundaries, so
/// `BoldItalic` matches and `Italicized` does not.
fn has_italic_token(name: &str) -> bool {
    let mut tokens = Vec::new();
    for word in name.split([' ', '-', '_']) {
        let mut start = 0;
        let mut prev_lower = false;
        for (i, c) in word.char_indices() {
            if prev_lower && c.is_uppercase() {
                tokens.push(&word[start..i]);
                start = i;
            }
            prev_lower = c.is_lowercase();
        }
        tokens.push(&word[start..]);
    }
    tokens
        .iter()
        .any(|t| t.eq_ignore_ascii_case("italic") || t.eq_ignore_ascii_case("oblique"))
}

/// The host's font set: fontconfig on Linux, where it is the configuration
/// users edit, and the platform's own font API elsewhere.
#[cfg(target_os = "linux")]
fn system_provider() -> Result<provider::fontconfig::Fontconfig, ShapingError> {
    provider::fontconfig::Fontconfig::system()
}

#[cfg(target_os = "macos")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "one signature with the Linux provider, which can fail to load"
)]
fn system_provider() -> Result<provider::coretext::CoreText, ShapingError> {
    Ok(provider::coretext::CoreText::system())
}

#[cfg(windows)]
fn system_provider() -> Result<provider::directwrite::DirectWrite, ShapingError> {
    provider::directwrite::DirectWrite::system()
}

// Mapped rather than read: a 183 MB emoji face read into a `Vec` is dirty
// heap instead of reclaimable page cache
// (docs/explanation/rendering/text-shaping.md "Font loading").
fn map_face_file(path: &std::path::Path) -> Result<memmap2::Mmap, ShapingError> {
    let file = std::fs::File::open(path)
        .map_err(|e| ShapingError::BadFont(format!("open {}: {e}", path.display())))?;
    // SAFETY: `path` is a provider-discovered face under a
    // system font root: root-owned, read-only, not mutated
    // for the process lifetime
    // (docs/reference/security-audits.md "`O_CLOEXEC` +
    // `O_NOFOLLOW` audit (standing)"). The only documented
    // hazard of `Mmap::map` is SIGBUS if the file is
    // truncated while mapped, which does not happen for
    // these immutable OS assets. This is the same trade-off
    // FreeType's path-based `FT_New_Face` accepts.
    unsafe { memmap2::Mmap::map(&file) }
        .map_err(|e| ShapingError::BadFont(format!("mmap {}: {e}", path.display())))
}

/// A family name an installed face carries, with what its faces declare.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InstalledFamily {
    pub name: String,
    /// Every face maps all of printable ASCII to glyphs of one advance.
    pub monospace: bool,
    /// Sorted upright first, then by weight; one entry per distinct pair.
    pub styles: Vec<FaceStyle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct FaceStyle {
    pub italic: bool,
    /// On the OpenType 1–1000 scale.
    pub weight: u16,
}

impl InstalledFamily {
    const fn new(name: String) -> Self {
        Self {
            name,
            monospace: true,
            styles: Vec::new(),
        }
    }

    fn add_face(&mut self, monospace: bool, style: FaceStyle) {
        self.monospace &= monospace;
        if let Err(at) = self.styles.binary_search(&style) {
            self.styles.insert(at, style);
        }
    }
}

/// Every family name `font.family` and the other `family` keys accept
/// as an installed family: the faces fontconfig lists on Linux in the
/// formats the loader parses, CoreText's on macOS and DirectWrite's on
/// Windows. Names the provider matches as one family appear once.
pub fn installed_families() -> Result<Vec<InstalledFamily>, ShapingError> {
    let host = system_provider()?;
    let mut families = host.installed_families();
    sort_ignoring_case(&mut families);
    Ok(families)
}

fn sort_ignoring_case(families: &mut [InstalledFamily]) {
    families.sort_by_cached_key(|family| (family.name.to_lowercase(), family.name.clone()));
}

/// A resolved `font.fallback` entry or styled font table.
///
/// `None` inherits: `family` derives from base `font.family` at the target
/// weight/slant, while `features` inherits base `font.features` (`Some([])`
/// opts out).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FaceSpec {
    pub family: Option<String>,
    pub features: Option<Vec<String>>,
    /// On the OpenType 1–1000 scale; `None` keeps the style's own weight
    /// (italic takes regular's, bold italic takes bold's). Read with
    /// `axes` for a styled table only: a fallback entry takes the styles'.
    pub weight: Option<u16>,
    pub axes: Vec<AxisSetting>,
}

impl FaceSpec {
    #[must_use]
    pub fn named(family: impl Into<String>) -> Self {
        Self {
            family: Some(family.into()),
            ..Self::default()
        }
    }
}

/// One variation-axis coordinate, in the axis's design units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisSetting {
    pub tag: [u8; 4],
    pub value: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct StyleFaces<'a> {
    /// `font.weight`; `None` draws the face as the provider matched it.
    pub regular_weight: Option<u16>,
    pub regular_axes: &'a [AxisSetting],
    pub bold: &'a FaceSpec,
    pub italic: &'a FaceSpec,
    pub bold_italic: &'a FaceSpec,
}

impl Default for StyleFaces<'_> {
    fn default() -> Self {
        const INHERIT: &FaceSpec = &FaceSpec {
            family: None,
            features: None,
            weight: None,
            axes: Vec::new(),
        };
        Self {
            regular_weight: None,
            regular_axes: &[],
            bold: INHERIT,
            italic: INHERIT,
            bold_italic: INHERIT,
        }
    }
}

impl StyleFaces<'_> {
    /// The configured weight of each style, by [`FontStyle::index`].
    const fn weights(&self) -> StyleWeights {
        let italic = match self.italic.weight {
            Some(weight) => Some(weight),
            None => self.regular_weight,
        };
        let bold_italic = match self.bold_italic.weight {
            Some(weight) => Some(weight),
            None => self.bold.weight,
        };
        [self.regular_weight, self.bold.weight, italic, bold_italic]
    }

    /// `over`'s own axes merged over the regular ones it inherits when it
    /// derives from `font.family`; the weight and slant axes are the
    /// style's own.
    fn axes_of(&self, over: &FaceSpec) -> Vec<AxisSetting> {
        let mut axes: Vec<AxisSetting> = if over.family.is_none() {
            self.regular_axes
                .iter()
                .filter(|a| !matches!(&a.tag, b"wght" | b"ital" | b"slnt"))
                .copied()
                .collect()
        } else {
            Vec::new()
        };
        for setting in &over.axes {
            axes.retain(|a| a.tag != setting.tag);
            axes.push(*setting);
        }
        axes
    }
}

/// A configured weight per [`FontStyle::index`]; `None` keeps that
/// style's default path.
type StyleWeights = [Option<u16>; 4];

const UNSET_WEIGHTS: StyleWeights = [None; 4];

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StackDescription {
    pub primary: [String; 4],
    pub fallbacks: Vec<String>,
    /// The platform API asked per character for what `fallbacks` miss.
    pub system_fallback: Option<String>,
}

/// Ordered chain of fonts queried per glyph (`fonts[0]` is regular primary,
/// `fonts[1..]` fallbacks). Cell metrics come from the regular primary alone,
/// so styled faces must share its advance width.
#[derive(Clone)]
pub struct FontStack {
    fonts: Vec<Arc<Font>>,
    features: Vec<Vec<String>>,
    /// One chain per [`FontStyle::index`], indexed like `fonts`: `[0]` is
    /// the styled primary, `[i]` entry `i`'s view at that style. A
    /// fallback's view covers every codepoint its regular entry does, so
    /// the entry is chosen on `fonts` and only the drawn face varies.
    chains: [Vec<Arc<Font>>; 4],
    styled_features: [Vec<String>; 4],
    /// Where the host's ranked cascade starts in `fonts`. Its order is the
    /// host's rather than declared or discovered by family, so among its
    /// faces a text-presentation scalar takes a mono face before an
    /// earlier-ranked color one.
    cascade_from: Option<usize>,
    /// Asked once the faces above miss; its faces take the ids after them.
    system: Option<Arc<SystemFaces>>,
}

/// Faces the host's per-character fallback picked, outside the ordered
/// chain: a face found for one scalar never answers for another without
/// asking the host, so what a cell draws does not depend on which text
/// came first.
struct SystemFaces {
    source: Box<dyn SystemFallback>,
    /// Append-only, so a face id handed out stays valid while the stack
    /// lives.
    pool: Box<[OnceLock<Arc<Font>>]>,
    state: Mutex<SystemState>,
}

#[derive(Default)]
struct SystemState {
    answers: HashMap<(char, Presentation, usize), Option<usize>>,
    /// `None` for a face that failed to load or found the pool full.
    slots: HashMap<FaceKey, Option<usize>>,
    len: usize,
}

/// Bounds what a stream of distinct scalars can make the stack remember;
/// past it the answers are asked for again, the faces stay.
const MAX_SYSTEM_ANSWERS: usize = 1 << 16;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Presentation {
    Text,
    Color,
}

impl Presentation {
    /// The selector rides along so the host weighs the presentation the
    /// stack asks for, not the scalar's default.
    fn query(self, c: char) -> String {
        match self {
            Self::Text if is_emoji_presentation(c) => format!("{c}\u{FE0E}"),
            Self::Text => c.to_string(),
            Self::Color => format!("{c}\u{FE0F}"),
        }
    }
}

/// One face as the pool draws it: a variable face found for a bold cell
/// sits at another weight than the same face found for a regular one, and
/// a color face drawn in text presentation is its outlines alone.
#[derive(Clone, PartialEq, Eq, Hash)]
struct FaceKey {
    source: FaceSourceKey,
    index: u32,
    instance: Option<usize>,
    hinting: Option<FaceHinting>,
    bold: bool,
    outlines_only: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum FaceSourceKey {
    File(std::path::PathBuf),
    Shared(usize),
}

impl FaceKey {
    fn of(face: &ResolvedFace, style: FontStyle) -> Self {
        let source = match &face.source {
            FaceSource::File(path) => FaceSourceKey::File(path.clone()),
            FaceSource::Shared(bytes) => {
                FaceSourceKey::Shared(Arc::as_ptr(bytes).cast::<u8>() as usize)
            }
        };
        Self {
            source,
            index: face.index,
            instance: face.instance,
            hinting: face.hinting,
            bold: style.bold,
            outlines_only: false,
        }
    }
}

impl SystemFaces {
    fn new(source: Box<dyn SystemFallback>, capacity: usize) -> Self {
        Self {
            source,
            pool: (0..capacity).map(|_| OnceLock::new()).collect(),
            state: Mutex::default(),
        }
    }

    fn face(&self, slot: usize) -> Option<&Arc<Font>> {
        self.pool.get(slot)?.get()
    }

    /// The pool slot of the face the host draws `c` with in
    /// `presentation`, `None` when it has none that covers `c` in it.
    fn slot_for(&self, c: char, presentation: Presentation, style: FontStyle) -> Option<usize> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (c, presentation, style.index());
        if let Some(&answer) = state.answers.get(&key) {
            return answer;
        }
        let face = match self.source.face_for(&presentation.query(c), style) {
            Ok(face) => face,
            Err(error) => {
                debug!(%error, ?c, "system fallback lookup failed");
                return None;
            }
        };
        let slot = face
            .and_then(|face| {
                let text = (presentation == Presentation::Text).then_some(c);
                self.intern(&mut state, face, text, style)
            })
            .filter(|&slot| {
                self.face(slot).is_some_and(|font| {
                    font.has_glyph(c)
                        && (presentation == Presentation::Text || font.has_color_glyphs())
                })
            });
        if state.answers.len() >= MAX_SYSTEM_ANSWERS {
            state.answers.clear();
        }
        state.answers.insert(key, slot);
        slot
    }

    /// A color face answering for `text`, a scalar asked for in text
    /// presentation, draws its outline: DirectWrite names Segoe UI Emoji
    /// for U+2733 whatever selector follows it. A glyph with no outline
    /// stays in color.
    fn intern(
        &self,
        state: &mut SystemState,
        face: ResolvedFace,
        text: Option<char>,
        style: FontStyle,
    ) -> Option<usize> {
        let key = FaceKey::of(&face, style);
        let slot = if let Some(&slot) = state.slots.get(&key) {
            slot
        } else {
            let slot = match Font::load_resolved(face) {
                Ok(font) => {
                    let weight = if style.bold {
                        BOLD_WEIGHT
                    } else {
                        REGULAR_WEIGHT
                    };
                    let font = if font.coords.is_empty() {
                        font.at_style(weight, false).unwrap_or(font)
                    } else {
                        font
                    };
                    self.push(state, font)
                }
                Err(error) => {
                    warn!(%error, "system fallback face failed to load");
                    None
                }
            };
            state.slots.insert(key.clone(), slot);
            slot
        }?;
        let font = self.face(slot)?;
        if !font.has_color_glyphs() || !text.is_some_and(|c| font.has_outline(c)) {
            return Some(slot);
        }
        let key = FaceKey {
            outlines_only: true,
            ..key
        };
        if let Some(&slot) = state.slots.get(&key) {
            return slot;
        }
        let outlines = self.push(state, font.outlines_only()).unwrap_or(slot);
        state.slots.insert(key, Some(outlines));
        Some(outlines)
    }

    /// `None` for a full pool.
    fn push(&self, state: &mut SystemState, font: Font) -> Option<usize> {
        if state.len == self.pool.len() {
            warn!("the system fallback picked more faces than a stack holds; drawing .notdef");
            return None;
        }
        let slot = state.len;
        self.pool[slot].get_or_init(|| Arc::new(font));
        state.len += 1;
        Some(slot)
    }
}

/// The bold, italic and bold-italic slots with the slant each is queried
/// at and the weight it takes when none is configured.
const STYLE_VARIANTS: [(FontStyle, u16, bool); 3] = [
    (
        FontStyle {
            bold: true,
            italic: false,
        },
        BOLD_WEIGHT,
        false,
    ),
    (
        FontStyle {
            bold: false,
            italic: true,
        },
        REGULAR_WEIGHT,
        true,
    ),
    (
        FontStyle {
            bold: true,
            italic: true,
        },
        BOLD_WEIGHT,
        true,
    ),
];

/// Faces one family's styles resolved to, so two styles landing on the
/// same file and axis position share one `Arc`.
struct StyleViews<'a, P: FaceProvider> {
    /// Axis settings already reported as not applying to a face, so
    /// styles sharing a face log each once.
    reported: HashSet<(P::Id, [u8; 4], u32)>,
    provider: &'a P,
    /// Set for a fallback, whose styles are looked up among its own
    /// family's faces rather than by name.
    sibling_of: Option<P::Id>,
    /// `None` for a face that failed to load.
    by_id: HashMap<P::Id, Option<Arc<Font>>>,
    views: HashMap<(P::Id, Vec<F2Dot14>), Arc<Font>>,
    regular: Arc<Font>,
}

impl<'a, P: FaceProvider> StyleViews<'a, P> {
    /// `matched` is the regular face as the provider matched it, which
    /// the styles move from; `regular` is the view drawn, whose position
    /// a style must not inherit.
    fn new(provider: &'a P, regular_id: P::Id, matched: &Arc<Font>, regular: &Arc<Font>) -> Self {
        Self {
            reported: HashSet::new(),
            provider,
            sibling_of: None,
            by_id: HashMap::from([(regular_id, Some(matched.clone()))]),
            views: HashMap::from([((regular_id, regular.coords.clone()), regular.clone())]),
            regular: regular.clone(),
        }
    }

    /// Logs each of `axes` that `font` (face `id`) lacks or holds only
    /// in a narrower range, which drawing ignores or clamps.
    fn report_axes(&mut self, id: P::Id, font: &Font, axes: &[AxisSetting]) {
        for setting in axes {
            let axis = font.face.axes().get_by_tag(Tag::new(&setting.tag));
            let applies = axis
                .is_some_and(|axis| (axis.min_value()..=axis.max_value()).contains(&setting.value));
            if applies
                || !self
                    .reported
                    .insert((id, setting.tag, setting.value.to_bits()))
            {
                continue;
            }
            let tag = String::from_utf8_lossy(&setting.tag);
            match axis {
                None => warn!(
                    face = font.display_name(),
                    %tag,
                    "the face has no such variation axis; ignoring it"
                ),
                Some(axis) if !(axis.min_value()..=axis.max_value()).contains(&setting.value) => {
                    warn!(
                        face = font.display_name(),
                        %tag,
                        value = setting.value,
                        min = axis.min_value(),
                        max = axis.max_value(),
                        "variation axis value outside the face's range; clamping"
                    );
                }
                Some(_) => {}
            }
        }
    }

    /// `family` at `weight`, `slant` and `axes`; `None` when no face
    /// matches.
    fn resolve(
        &mut self,
        family: &str,
        weight: u16,
        italic: bool,
        axes: &[AxisSetting],
    ) -> Option<Arc<Font>> {
        let id = match self.sibling_of {
            Some(of) => self.provider.find_sibling(of, family, weight, italic),
            None => self.provider.find_face(family, weight, italic),
        }?;
        let loaded = self
            .by_id
            .entry(id)
            .or_insert_with(|| match Font::load_face_id(self.provider, id) {
                Ok(font) => Some(Arc::new(font)),
                Err(e) => {
                    warn!(error = %e, "styled font face failed to load; using regular");
                    None
                }
            })
            .clone();
        let Some(face) = loaded else {
            return Some(self.regular.clone());
        };
        self.report_axes(id, &face, axes);
        let italic = italic && self.provider.resolve_face(id).is_some_and(|f| !f.slanted);
        Some(match face.at_position(Some(weight), italic, axes) {
            Some(view) => self
                .views
                .entry((id, view.coords.clone()))
                .or_insert_with(|| Arc::new(view))
                .clone(),
            None => face,
        })
    }
}

/// First installed wins. JP before SC/KR/TC is a locale choice, not a
/// coverage one.
pub const CJK_FALLBACK_FAMILIES: &[&str] = &[
    "Noto Sans CJK JP",
    "Noto Sans CJK SC",
    "Noto Sans CJK KR",
    "Noto Sans CJK TC",
    "Source Han Sans JP",
    "Hiragino Sans",
    "IPAGothic",
    "MS Gothic",
];

pub const EMOJI_FALLBACK_FAMILIES: &[&str] = &[
    "Noto Color Emoji",
    "Apple Color Emoji",
    "Segoe UI Emoji",
    "Twemoji Mozilla",
];

/// Scalars whose presence forces a cluster onto the color-emoji face.
///
/// U+1F000..=U+1FAFF anchors ZWJ sequences with text-default bases onto the
/// color face, while stopping below U+20000 so astral CJK never anchors emoji.
const fn is_emoji_face_anchor(c: char) -> bool {
    matches!(c, '\u{1F000}'..='\u{1FAFF}')
}

/// Monochrome symbol and dingbat faces, appended before emoji fallbacks.
///
/// Text-presentation symbols resolve mono instead of color. All installed
/// families are kept because symbol coverage is fragmented across system fonts.
pub const SYMBOL_FALLBACK_FAMILIES: &[&str] = &[
    "Noto Sans Symbols",
    "Noto Sans Symbols 2",
    "Segoe UI Symbol",
    "Zapf Dingbats",
    "STIX Two Math",
    "Apple Symbols",
];

/// Symbols-only Nerd Font releases. A patched-monospace variant
/// (`JetBrainsMono Nerd Font`, …) is what users pin as `font.family`;
/// auto-discovering another on top would mix two metric systems into
/// the cell layout.
pub const NERD_FONT_FAMILIES: &[&str] = &["Symbols Nerd Font", "Symbols Nerd Font Mono"];

impl FontStack {
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    #[must_use]
    pub fn new(primary: Arc<Font>) -> Self {
        Self::with_primary_features(primary, Vec::new())
    }

    /// Auto-discovery over the pinned `$FELIS_TEST_FONT_DIR` set alone,
    /// drawn with fontconfig's stock `hintslight`, so a fallback-dependent
    /// test sees the same faces and the same rasters on every host.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    #[must_use]
    pub fn pinned_test_stack() -> Self {
        let mut set = FileSet::default();
        set.set_hinting(FaceHinting::Slight { autohint: false });
        set.add_dir(test_font_dir());
        Self::auto_discover_in(&set, None, &[], &[], &StyleFaces::default())
            .unwrap_or_else(|e| panic!("the pinned set has no monospace face: {e}"))
    }

    #[must_use]
    pub fn with_primary_features(primary: Arc<Font>, features: Vec<String>) -> Self {
        let chains = std::array::from_fn(|_| vec![primary.clone()]);
        let styled_features = std::array::from_fn(|_| features.clone());
        Self {
            fonts: vec![primary],
            features: vec![features],
            chains,
            styled_features,
            cascade_from: None,
            system: None,
        }
    }

    /// Appends `font` as one face drawn in every style.
    #[must_use]
    pub fn with_fallback_features(self, font: Arc<Font>, features: Vec<String>) -> Self {
        let views = [font.clone(), font.clone(), font.clone(), font];
        self.with_fallback_views(views, features)
    }

    fn with_fallback_views(mut self, views: [Arc<Font>; 4], features: Vec<String>) -> Self {
        debug_assert!(
            self.system.is_none(),
            "system faces take the ids after the chain"
        );
        self.fonts.push(views[FontStyle::REGULAR.index()].clone());
        self.features.push(features);
        for (chain, view) in self.chains.iter_mut().zip(views) {
            chain.push(view);
        }
        debug_assert_eq!(self.fonts.len(), self.features.len());
        self
    }

    /// Load the primary font and fallbacks.
    ///
    /// Unknown families fall back to the default monospace face.
    /// `explicit_fallbacks` come before auto-discovery; `features: None`
    /// inherits `primary_features`, while `Some([])` opts out.
    pub fn auto_discover(
        primary_family: Option<&str>,
        primary_features: &[String],
        explicit_fallbacks: &[FaceSpec],
        styles: &StyleFaces<'_>,
    ) -> Result<Self, ShapingError> {
        Self::auto_discover_in(
            &system_provider()?,
            primary_family,
            primary_features,
            explicit_fallbacks,
            styles,
        )
    }

    /// [`Self::auto_discover`] over the faces in `files` alone instead of
    /// the system's, so the same inputs resolve to the same faces on every
    /// machine. The files are read into memory rather than mapped: unlike
    /// a system font root, the caller's files may be rewritten while the
    /// stack lives.
    pub fn discover_in_files(
        files: &[std::path::PathBuf],
        primary_family: Option<&str>,
        primary_features: &[String],
        explicit_fallbacks: &[FaceSpec],
        styles: &StyleFaces<'_>,
    ) -> Result<Self, ShapingError> {
        let mut set = FileSet::default();
        for path in files {
            let err = |detail: String| ShapingError::FontPath {
                path: path.clone(),
                detail,
            };
            let bytes = std::fs::read(path).map_err(|e| err(e.to_string()))?;
            if !set.add(bytes) {
                return Err(err("no font face in the file".to_owned()));
            }
        }
        Self::auto_discover_in(
            &set,
            primary_family,
            primary_features,
            explicit_fallbacks,
            styles,
        )
    }

    fn auto_discover_in<P: FaceProvider>(
        db: &P,
        primary_family: Option<&str>,
        primary_features: &[String],
        explicit_fallbacks: &[FaceSpec],
        styles: &StyleFaces<'_>,
    ) -> Result<Self, ShapingError> {
        let weights = styles.weights();
        let regular_weight = weights[FontStyle::REGULAR.index()].unwrap_or(REGULAR_WEIGHT);
        let named = primary_family.and_then(|name| {
            let Some(id) = db.find_face(name, regular_weight, false) else {
                warn!(
                    family = name,
                    "font family not found; falling back to monospace"
                );
                return None;
            };
            match Font::load_face_id(db, id) {
                Ok(loaded) => Some((id, loaded, name)),
                Err(error) => {
                    warn!(
                        family = name,
                        %error,
                        "font family failed to load; falling back to monospace"
                    );
                    None
                }
            }
        });
        let (regular_id, loaded, base_family, requested) = if let Some((id, loaded, name)) = named {
            (id, loaded, name.to_owned(), name)
        } else {
            let (id, family) = db.default_monospace(regular_weight)?;
            (id, Font::load_face_id(db, id)?, family, "monospace")
        };
        let matched = Arc::new(loaded);
        let regular = matched
            .regular_view(styles.regular_weight, styles.regular_axes)
            .map_or_else(|| matched.clone(), Arc::new);
        let mut stack = Self::with_primary_features(regular.clone(), primary_features.to_vec());
        stack.load_styled(
            db,
            regular_id,
            &matched,
            &regular,
            &base_family,
            primary_features,
            styles,
        );
        let mut declared = HashSet::new();
        for spec in explicit_fallbacks {
            let Some(family) = spec.family.as_deref() else {
                warn!("font.fallback entry names no family; skipping");
                continue;
            };
            let features = spec
                .features
                .as_deref()
                .map_or_else(|| primary_features.to_vec(), <[String]>::to_vec);
            if declared.len() == MAX_FALLBACKS {
                warn!(
                    family,
                    "font.fallback lists more than {MAX_FALLBACKS} faces; dropping the rest"
                );
                break;
            }
            let Some(id) = db.find_face(family, regular_weight, false) else {
                warn!(family, "font.fallback entry not installed; skipping");
                continue;
            };
            match fallback_views_of(db, id, family, weights) {
                Ok(views) if declared.insert(id) => {
                    stack = stack.with_fallback_views(views, features);
                }
                Ok(_) => {}
                Err(error) => warn!(family, %error, "font.fallback entry failed to load; skipping"),
            }
        }
        if let Some(cascaded) = append_cascade(&stack, db, requested, primary_features, &declared) {
            stack = cascaded;
        } else if let Some(system) = db.system_fallback(regular_id) {
            // CoreText draws a BMP private-use scalar, where the Nerd
            // glyphs live, from whichever face maps it (STIX Two Math, or
            // LastResort) rather than a Nerd font.
            stack =
                append_first_installed(stack, db, NERD_FONT_FAMILIES, primary_features, &declared);
            stack = stack.with_system_fallback(system);
        } else {
            stack = append_first_installed(
                stack,
                db,
                CJK_FALLBACK_FAMILIES,
                primary_features,
                &declared,
            );
            stack = append_all_installed(
                stack,
                db,
                SYMBOL_FALLBACK_FAMILIES,
                primary_features,
                &declared,
            );
            stack = append_first_installed(
                stack,
                db,
                EMOJI_FALLBACK_FAMILIES,
                primary_features,
                &declared,
            );
            stack =
                append_first_installed(stack, db, NERD_FONT_FAMILIES, primary_features, &declared);
        }
        Ok(stack)
    }

    fn load_styled<P: FaceProvider>(
        &mut self,
        db: &P,
        regular_id: P::Id,
        matched: &Arc<Font>,
        regular: &Arc<Font>,
        base_family: &str,
        base_features: &[String],
        styles: &StyleFaces<'_>,
    ) {
        let mut resolver = StyleViews::new(db, regular_id, matched, regular);
        resolver.report_axes(regular_id, regular, styles.regular_axes);
        let weights = styles.weights();
        let overrides = [styles.bold, styles.italic, styles.bold_italic];
        for ((style, weight, italic), over) in STYLE_VARIANTS.into_iter().zip(overrides) {
            let idx = style.index();
            let family = over.family.as_deref().unwrap_or(base_family);
            self.styled_features[idx] = over
                .features
                .as_deref()
                .map_or_else(|| base_features.to_vec(), <[String]>::to_vec);
            let weight = weights[idx].unwrap_or(weight);
            let axes = styles.axes_of(over);
            self.chains[idx][0] = resolver
                .resolve(family, weight, italic, &axes)
                .unwrap_or_else(|| {
                    if over.family.is_some() {
                        warn!("styled font family not installed; using regular");
                    }
                    regular.clone()
                });
        }
    }

    /// Hands the misses of every face so far to `source`, whose faces take
    /// the ids left after them.
    fn with_system_fallback(mut self, source: Box<dyn SystemFallback>) -> Self {
        let capacity = FACE_ID_SATURATED.saturating_sub(self.fonts.len());
        self.system = Some(Arc::new(SystemFaces::new(source, capacity)));
        self
    }

    fn system_index(&self, c: char, presentation: Presentation, style: FontStyle) -> Option<usize> {
        let slot = self.system.as_ref()?.slot_for(c, presentation, style)?;
        Some(self.fonts.len() + slot)
    }

    fn system_face(&self, index: usize) -> Option<&Arc<Font>> {
        self.system
            .as_ref()?
            .face(index.checked_sub(self.fonts.len())?)
    }

    #[must_use]
    pub fn primary(&self) -> &Arc<Font> {
        &self.fonts[0]
    }

    #[must_use]
    pub fn styled_primary(&self, style: FontStyle) -> &Arc<Font> {
        &self.chains[style.index()][0]
    }

    #[must_use]
    pub fn primary_covers(&self, c: char, style: FontStyle) -> bool {
        self.chains[style.index()][0].has_glyph(c)
    }

    /// Falls back to the styled primary when nothing covers `c`, so the
    /// caller always has a face to rasterize (the `.notdef` box).
    #[must_use]
    pub fn resolve(&self, c: char, style: FontStyle) -> &Arc<Font> {
        self.font_at(self.resolve_styled_index(c, style), style)
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.fonts.len()
    }

    #[must_use]
    pub fn features_at(&self, index: usize) -> &[String] {
        self.features.get(index).map_or(&[], Vec::as_slice)
    }

    /// Mirrors [`Self::font_at`]: index `0` is the styled primary, so a
    /// `[font.bold] features` override applies exactly where the bold
    /// face does. Features resolved off a different face than the glyphs
    /// were shaped against is the bug this pairing prevents.
    #[must_use]
    pub fn features_for(&self, index: usize, style: FontStyle) -> &[String] {
        if index != 0 && self.fonts.get(index).is_some() {
            return self.features_at(index);
        }
        if index != 0 && self.system_face(index).is_some() {
            return self.features_at(0);
        }
        &self.styled_features[style.index()]
    }

    #[must_use]
    pub fn resolve_index(&self, c: char) -> usize {
        for (i, font) in self.fonts.iter().enumerate() {
            if font.has_glyph(c) {
                return i;
            }
        }
        0
    }

    /// Honors `style`, unlike [`Self::resolve_index`]: atlas keys must use
    /// this one, or a bold cluster whose base only the regular primary
    /// covers is keyed to the wrong face. An emoji-presentation scalar
    /// takes the first color face covering it, ahead of the primary;
    /// first coverage decides only when none does.
    #[must_use]
    pub fn resolve_styled_index(&self, c: char, style: FontStyle) -> usize {
        if is_emoji_presentation(c)
            && let Some(index) = self.resolve_color_index(c, style)
        {
            return index;
        }
        self.first_cover_styled_index(c, style)
    }

    /// The face drawing `c` in text presentation: the styled primary, then
    /// the chain in order, then the system fallback; `0` for `.notdef`.
    /// Past [`Self::cascade_from`], a mono face goes before any color one,
    /// a color primary included.
    fn first_cover_styled_index(&self, c: char, style: FontStyle) -> usize {
        let primary = &self.chains[style.index()][0];
        let primary_covers = primary.has_glyph(c);
        let covers = |from: usize, to: usize| {
            self.fonts[from..to]
                .iter()
                .position(|f| f.has_glyph(c))
                .map(|i| from + i)
        };
        let Some(from) = self.cascade_from else {
            if primary_covers {
                return 0;
            }
            return covers(1, self.fonts.len())
                .or_else(|| self.system_index(c, Presentation::Text, style))
                .unwrap_or(0);
        };
        if primary_covers && !primary.has_color_glyphs() {
            return 0;
        }
        if let Some(i) = covers(1, from) {
            return i;
        }
        let mono = self.fonts[from..]
            .iter()
            .position(|f| !f.has_color_glyphs() && f.has_glyph(c));
        if let Some(i) = mono {
            return from + i;
        }
        if primary_covers {
            return 0;
        }
        covers(from, self.fonts.len())
            .or_else(|| self.system_index(c, Presentation::Text, style))
            .unwrap_or(0)
    }

    /// First face that covers `c` *and* carries color glyphs, `None`
    /// when none does. Routes a VS16 cluster onto the color-emoji face
    /// even when a mono symbol face earlier in the chain covers the base
    /// (U+2764 lives in Zapf Dingbats).
    #[must_use]
    pub fn resolve_color_index(&self, c: char, style: FontStyle) -> Option<usize> {
        let styled = &self.chains[style.index()][0];
        if styled.has_glyph(c) && styled.has_color_glyphs() {
            return Some(0);
        }
        self.fonts
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, font)| font.has_glyph(c) && font.has_color_glyphs())
            .map(|(i, _)| i)
            .or_else(|| self.system_index(c, Presentation::Color, style))
    }

    /// Index `0` is the styled primary; an index past the chain and the
    /// system faces (the [`SizingKey::with_font_id`] saturation bucket)
    /// falls back to it.
    #[must_use]
    pub fn font_at(&self, index: usize, style: FontStyle) -> &Arc<Font> {
        let chain = &self.chains[style.index()];
        chain
            .get(index)
            .or_else(|| self.system_face(index))
            .unwrap_or(&chain[0])
    }

    /// The faces a window would draw with, by [`FontStyle::index`] for
    /// the primary and in chain order for the fallbacks.
    #[must_use]
    pub fn describe(&self) -> StackDescription {
        StackDescription {
            primary: std::array::from_fn(|i| self.chains[i][0].display_name()),
            fallbacks: self
                .fonts
                .iter()
                .skip(1)
                .map(|f| f.display_name())
                .collect(),
            system_fallback: self
                .system
                .as_ref()
                .map(|system| system.source.name().to_owned()),
        }
    }

    /// Always `false`; exists for clippy's `len_without_is_empty`.
    #[must_use]
    #[allow(clippy::unused_self)]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// `family` as a fallback entry, one face per [`FontStyle::index`], each
/// drawn from the family's own face for that style at `weights`. A color
/// face, or a style whose face would drop a codepoint the regular one
/// covers, draws the regular face.
fn fallback_views_of<P: FaceProvider>(
    db: &P,
    id: P::Id,
    family: &str,
    weights: StyleWeights,
) -> Result<[Arc<Font>; 4], ShapingError> {
    let matched = Arc::new(Font::load_face_id(db, id)?);
    let regular = matched
        .regular_view(weights[FontStyle::REGULAR.index()], &[])
        .map_or_else(|| matched.clone(), Arc::new);
    let mut views = std::array::from_fn(|_| regular.clone());
    if regular.has_color_glyphs() {
        return Ok(views);
    }
    let mut resolver = StyleViews {
        sibling_of: Some(id),
        ..StyleViews::new(db, id, &matched, &regular)
    };
    for (style, weight, italic) in STYLE_VARIANTS {
        let weight = weights[style.index()].unwrap_or(weight);
        if let Some(view) = resolver.resolve(family, weight, italic, &[]) {
            views[style.index()] = covering_or_regular(view, &regular, family, style);
        }
    }
    Ok(views)
}

/// `view` when it maps every codepoint `regular` does, else `regular`: the
/// fallback entry is chosen on regular coverage, so a styled face missing
/// one of those codepoints would draw `.notdef` in its place.
fn covering_or_regular(
    view: Arc<Font>,
    regular: &Arc<Font>,
    family: &str,
    style: FontStyle,
) -> Arc<Font> {
    if view.covers_all_of(regular) {
        return view;
    }
    debug!(
        family,
        ?style,
        "styled fallback face covers less than regular; using regular"
    );
    regular.clone()
}

/// Bounds the declared entries and, separately, the host's cascade: each
/// face is loaded up front, and a full font set's cascade runs to hundreds
/// of faces that would cost seconds of startup.
const MAX_FALLBACKS: usize = 0x3E;

/// The stack with the provider's ranked cascade appended, or `None` when
/// it has none. The cascade is recomputed until every face it keeps has
/// loaded as the class it was kept for: a face that fails to load or
/// contradicts its reported color capability changes which later faces
/// add coverage. Each face is loaded once, so the loop ends.
fn append_cascade<P: FaceProvider>(
    stack: &FontStack,
    db: &P,
    requested: &str,
    features: &[String],
    declared: &HashSet<P::Id>,
) -> Option<FontStack> {
    let mut verdicts = HashMap::new();
    let mut loaded: HashMap<P::Id, [Arc<Font>; 4]> = HashMap::new();
    let cascade = loop {
        let mut cascade = db.fallback_cascade(requested, &verdicts)?;
        cascade.retain(|face| !declared.contains(&face.id));
        let mut changed = false;
        for face in cascade.iter().take(MAX_FALLBACKS) {
            if verdicts.contains_key(&face.id) {
                continue;
            }
            let family = db.family_of(face.id).unwrap_or_default();
            let verdict = match fallback_views_of(db, face.id, &family, UNSET_WEIGHTS) {
                Ok(views) => {
                    let color = views[0].has_color_glyphs();
                    loaded.insert(face.id, views);
                    changed |= color != face.color;
                    LoadedAs::Color(color)
                }
                Err(e) => {
                    warn!(family, error = %e, "fallback face failed to load");
                    changed = true;
                    LoadedAs::Unusable
                }
            };
            verdicts.insert(face.id, verdict);
        }
        if !changed {
            break cascade;
        }
    };
    let mut cascaded = stack.clone();
    cascaded.cascade_from = Some(cascaded.fonts.len());
    let dropped = cascade.len().saturating_sub(MAX_FALLBACKS);
    for face in cascade.iter().take(MAX_FALLBACKS) {
        if let Some(views) = loaded.remove(&face.id) {
            cascaded = cascaded.with_fallback_views(views, features.to_vec());
        }
    }
    if dropped > 0 {
        debug!(
            dropped,
            "fallback cascade has more than {MAX_FALLBACKS} faces; dropping the least preferred"
        );
    }
    Some(cascaded)
}

/// Skips a candidate the user already declared, so the next one gets
/// its place.
fn append_first_installed<P: FaceProvider>(
    stack: FontStack,
    db: &P,
    candidates: &[&str],
    inherited_features: &[String],
    declared: &HashSet<P::Id>,
) -> FontStack {
    for &name in candidates {
        if let Some(views) = undeclared_views(db, name, declared) {
            return stack.with_fallback_views(views, inherited_features.to_vec());
        }
    }
    stack
}

fn append_all_installed<P: FaceProvider>(
    mut stack: FontStack,
    db: &P,
    candidates: &[&str],
    inherited_features: &[String],
    declared: &HashSet<P::Id>,
) -> FontStack {
    for &name in candidates {
        if let Some(views) = undeclared_views(db, name, declared) {
            stack = stack.with_fallback_views(views, inherited_features.to_vec());
        }
    }
    stack
}

fn undeclared_views<P: FaceProvider>(
    db: &P,
    family: &str,
    declared: &HashSet<P::Id>,
) -> Option<[Arc<Font>; 4]> {
    let id = db.find_face(family, REGULAR_WEIGHT, false)?;
    if declared.contains(&id) {
        return None;
    }
    fallback_views_of(db, id, family, UNSET_WEIGHTS).ok()
}

/// Pixel-space cell metrics at a given font size. `glyph_box` is the
/// cell before `font.cell_*_scale`; `pad` is its top-left corner inside
/// the scaled cell, below zero when a scale shrinks the cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
    pub ascent: u32,
    pub glyph_box: [u32; 2],
    pub pad: [i32; 2],
}

impl CellMetrics {
    #[must_use]
    pub const fn unscaled(width: u32, height: u32, ascent: u32) -> Self {
        Self {
            width,
            height,
            ascent,
            glyph_box: [width, height],
            pad: [0, 0],
        }
    }

    /// Ascent and descent ceil separately rather than rounding their
    /// sum, which clips the top row of a full-ascent glyph
    /// (`docs/explanation/rendering/text-shaping.md` "Atlas integration").
    #[must_use]
    pub fn from_scaled(width: f32, ascent: f32, descent: f32, leading: f32) -> Self {
        let ascent = ascent.ceil();
        let height = (ascent + descent.ceil() + leading.round()).max(1.0);
        Self::unscaled(width.round().max(1.0) as u32, height as u32, ascent as u32)
    }

    /// Resizes the cell by `[width, height]` factors and splits the
    /// difference evenly around the font's own cell, the top and left
    /// halves taking the floor.
    #[must_use]
    pub fn with_cell_scale(self, scale: [f32; 2]) -> Self {
        let [glyph_w, glyph_h] = self.glyph_box;
        let resize = |side: u32, factor: f32| {
            let scaled = (side as f32 * factor).round().max(1.0) as u32;
            let extra = i64::from(scaled) - i64::from(side);
            (scaled, i32::try_from(extra.div_euclid(2)).unwrap_or(0))
        };
        let (width, pad_x) = resize(glyph_w, scale[0]);
        let (height, pad_y) = resize(glyph_h, scale[1]);
        let glyph_ascent = i64::from(self.ascent) - i64::from(self.pad[1]);
        let ascent = u32::try_from((glyph_ascent + i64::from(pad_y)).max(0)).unwrap_or(0);
        Self {
            width,
            height,
            ascent,
            glyph_box: self.glyph_box,
            pad: [pad_x, pad_y],
        }
    }

    /// The font's ascent inside the glyph box, before the top pad.
    #[must_use]
    pub fn glyph_ascent(self) -> f32 {
        (i64::from(self.ascent) - i64::from(self.pad[1])) as f32
    }

    /// Cell top to baseline for a glyph drawn at `glyph_scale` in a block
    /// laid out at `layout_scale`: the pad belongs to the layout cell, the
    /// ascent to the glyph, and the two differ under OSC 66 `n/d`.
    #[must_use]
    pub fn baseline(self, glyph_scale: f32, layout_scale: f32) -> f32 {
        self.glyph_ascent()
            .mul_add(glyph_scale, self.pad[1] as f32 * layout_scale)
    }

    /// The padding right of and below the glyph box; negative on a
    /// shrink, like `pad`.
    #[must_use]
    pub fn trailing_pad(self) -> [i32; 2] {
        let trailing = |cell: u32, glyph: u32, lead: i32| {
            i32::try_from(i64::from(cell) - i64::from(glyph) - i64::from(lead)).unwrap_or(0)
        };
        [
            trailing(self.width, self.glyph_box[0], self.pad[0]),
            trailing(self.height, self.glyph_box[1], self.pad[1]),
        ]
    }
}

#[derive(Debug, Clone)]
pub enum GlyphPixels {
    /// 8-bit coverage, one byte per pixel, tinted by the cell foreground.
    Coverage(Vec<u8>),
    /// Straight (non-premultiplied) sRGB RGBA, four bytes per pixel,
    /// composited with its own color: collapsing color to a mask turns a
    /// tile emoji like ⏸ into a solid foreground square
    /// (`docs/explanation/rendering/text-shaping.md`).
    Rgba(Vec<u8>),
}

impl GlyphPixels {
    #[must_use]
    pub const fn bytes_per_pixel(&self) -> usize {
        match self {
            Self::Coverage(_) => 1,
            Self::Rgba(_) => 4,
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Coverage(bytes) | Self::Rgba(bytes) => bytes,
        }
    }

    #[must_use]
    pub const fn is_color(&self) -> bool {
        matches!(self, Self::Rgba(_))
    }

    /// The bytes the buffer holds allocated, which a color glyph near the
    /// raster budget may keep beyond its pixels.
    const fn allocated(&self) -> usize {
        match self {
            Self::Coverage(bytes) | Self::Rgba(bytes) => bytes.capacity(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GlyphBitmap {
    pixels: GlyphPixels,
    width: u32,
    height: u32,
    /// X offset from the cell origin to the bitmap left edge.
    pub left: i32,
    /// Y offset from the baseline up to the bitmap top edge.
    pub top: i32,
}

impl GlyphBitmap {
    /// # Panics
    ///
    /// When `pixels` disagrees with `width * height * bytes-per-pixel`.
    #[must_use]
    pub fn new(width: u32, height: u32, left: i32, top: i32, pixels: GlyphPixels) -> Self {
        assert_eq!(
            pixels.as_bytes().len(),
            width as usize * height as usize * pixels.bytes_per_pixel(),
            "glyph pixel buffer length must be width * height * bytes-per-pixel",
        );
        Self {
            pixels,
            width,
            height,
            left,
            top,
        }
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn pixels(&self) -> &GlyphPixels {
        &self.pixels
    }

    #[must_use]
    pub fn into_pixels(self) -> GlyphPixels {
        self.pixels
    }

    #[must_use]
    pub const fn is_color(&self) -> bool {
        self.pixels.is_color()
    }

    #[must_use]
    pub fn is_blank(&self) -> bool {
        let bytes = self.pixels.as_bytes();
        bytes.is_empty() || bytes.iter().all(|p| *p == 0)
    }
}

/// Sizing component of the shape-cache key packed into a `u32`.
///
/// Encodes OSC 66 sizing fields, [`FontStyle`] (bits 20-21), and face id
/// (bits 22-31). Packed into one `u32` because hashing the tuple form costs ~4×
/// on the cache hit path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SizingKey(u32);

/// The face id saturation bucket: [`SizingKey::with_font_id`] folds every
/// larger id into it.
pub const FACE_ID_SATURATED: usize = 0x3FF;

impl SizingKey {
    /// `v`/`h` are masked to 2 bits and the rest to 4; the VT parser
    /// already rejects OSC 66 metadata past the spec ranges (`s` 1-7,
    /// `w` 0-7, `n`/`d` 0-15, `v`/`h` 0-2).
    #[must_use]
    pub const fn new(
        scale: u8,
        cell_width: u8,
        frac_num: u8,
        frac_den: u8,
        valign: u8,
        halign: u8,
    ) -> Self {
        let scale = (scale & 0xF) as u32;
        let cell_width = (cell_width & 0xF) as u32;
        let frac_num = (frac_num & 0xF) as u32;
        let frac_den = (frac_den & 0xF) as u32;
        let valign = (valign & 0b11) as u32;
        let halign = (halign & 0b11) as u32;
        Self(
            halign
                | (valign << 2)
                | (frac_den << 4)
                | (frac_num << 8)
                | (cell_width << 12)
                | (scale << 16),
        )
    }

    #[must_use]
    pub const fn with_style(self, style: FontStyle) -> Self {
        Self((self.0 & !(0b11 << 20)) | ((style.index() as u32) << 20))
    }

    #[must_use]
    pub const fn style(self) -> FontStyle {
        FontStyle::from_index(((self.0 >> 20) & 0b11) as usize)
    }

    /// Ids past [`FACE_ID_SATURATED`] share its bucket.
    #[must_use]
    pub const fn with_font_id(self, font_id: usize) -> Self {
        let id = if font_id > FACE_ID_SATURATED {
            FACE_ID_SATURATED as u32
        } else {
            font_id as u32
        };
        Self((self.0 & !(0x3FF << 22)) | (id << 22))
    }

    #[must_use]
    pub const fn font_id(self) -> usize {
        ((self.0 >> 22) & 0x3FF) as usize
    }

    /// The OSC 66 `w` field; `0` is auto.
    #[must_use]
    pub const fn cell_width(self) -> u8 {
        ((self.0 >> 12) & 0xF) as u8
    }

    #[must_use]
    pub const fn scale(self) -> u8 {
        ((self.0 >> 16) & 0xF) as u8
    }

    #[must_use]
    pub const fn frac_num(self) -> u8 {
        ((self.0 >> 8) & 0xF) as u8
    }

    #[must_use]
    pub const fn frac_den(self) -> u8 {
        ((self.0 >> 4) & 0xF) as u8
    }

    /// `s`, or `s × n/d` when fractional (kitty text-sizing spec); the
    /// parser enforces `d > n`.
    #[must_use]
    pub fn effective_scale(self) -> f32 {
        let s = f32::from(self.scale().max(1));
        let n = f32::from(self.frac_num());
        let d = f32::from(self.frac_den());
        if d == 0.0 { s } else { s * n / d }
    }
}

impl Default for SizingKey {
    /// Spec default: `scale` 1, everything else 0, matching
    /// `Sizing::default()`. A derived `Default` would give `scale = 0`,
    /// and a default-sized cell through the dispatcher would miss the
    /// unsized-path slot and double-populate every glyph.
    fn default() -> Self {
        Self::new(1, 0, 0, 0, 0, 0)
    }
}

type ShapeKey = (char, u32, SizingKey);

/// Charged to every entry on top of its pixels: a blank bitmap has no
/// pixels, and PTY output picks the key.
const ENTRY_BYTES: usize = size_of::<(ShapeKey, GlyphBitmap)>();

const fn entry_cost(bitmap: &GlyphBitmap) -> usize {
    ENTRY_BYTES + bitmap.pixels.allocated()
}

/// Byte-capped glyph-bitmap cache. Eviction is arbitrary, not LRU:
/// recency is not tracked, so a hot glyph can go before a cold one.
pub struct ShapeCache {
    map: HashMap<ShapeKey, GlyphBitmap>,
    bytes_used: usize,
    cap: usize,
    /// Reused: it keeps per-font scaler state, so a fresh one per
    /// rasterize would rebuild it on every miss.
    rasterizer: Rasterizer,
}

impl Default for ShapeCache {
    fn default() -> Self {
        Self::new(16 * 1024 * 1024)
    }
}

impl ShapeCache {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            bytes_used: 0,
            cap,
            rasterizer: Rasterizer::default(),
        }
    }

    pub fn get_or_insert(&mut self, font: &Font, c: char, px: u32) -> &GlyphBitmap {
        self.get_or_insert_sized(font, c, px, SizingKey::default())
    }

    /// No sizing transform is applied here; the renderer bakes the scale
    /// into `px`, and `sizing` only keys the entry.
    pub fn get_or_insert_sized(
        &mut self,
        font: &Font,
        c: char,
        px: u32,
        sizing: SizingKey,
    ) -> &GlyphBitmap {
        let key = (c, px, sizing);
        if !self.map.contains_key(&key) {
            let bitmap = rasterize(&mut self.rasterizer, font, c, px as f32);
            self.bytes_used += entry_cost(&bitmap);
            self.map.insert(key, bitmap);
            while self.bytes_used > self.cap {
                let Some(evict_key) = self.map.keys().copied().find(|k| *k != key) else {
                    break;
                };
                if let Some(evicted) = self.map.remove(&evict_key) {
                    self.bytes_used = self.bytes_used.saturating_sub(entry_cost(&evicted));
                }
            }
        }
        &self.map[&key]
    }

    /// Not cached here: glyph-id bitmaps are cached at the atlas layer in
    /// `felis-render-core`, so a map entry would double-store them.
    /// `glyph_id == 0` returns a blank bitmap, as the char path does for
    /// a charmap miss.
    #[must_use]
    pub fn rasterize_glyph_id(&mut self, font: &Font, glyph_id: GlyphId, px: f32) -> GlyphBitmap {
        if glyph_id == 0 {
            return GlyphBitmap::blank();
        }
        self.rasterizer.rasterize(font, glyph_id, px)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

fn rasterize(rasterizer: &mut Rasterizer, font: &Font, c: char, px: f32) -> GlyphBitmap {
    let Some(glyph_id) = font.glyph_for(c) else {
        return GlyphBitmap::blank();
    };
    rasterizer.rasterize(font, glyph_id, px)
}

impl GlyphBitmap {
    const fn blank() -> Self {
        Self {
            pixels: GlyphPixels::Coverage(Vec::new()),
            width: 0,
            height: 0,
            left: 0,
            top: 0,
        }
    }
}

/// One output glyph from [`Shaper::shape_run`]. Source byte indices
/// double as cell indices (cells are 1:1 with bytes on the felis side);
/// every glyph of a ligature claims the whole source span of the cells
/// it replaces, and is painted from its leftmost cell. Advances and
/// offsets are already in pixels at the shaper's `px`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    pub glyph_id: GlyphId,
    pub source_byte_start: u32,
    pub source_byte_len: u32,
    pub advance_px: f32,
    pub x_offset_px: f32,
    pub y_offset_px: f32,
}

/// A whole grapheme cluster shaped against one face (marks and joiners
/// have no standalone coverage). `font_id` is the
/// [`FontStack::resolve_styled_index`] handle the renderer packs into
/// [`SizingKey::with_font_id`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClusterShaping {
    pub font_id: usize,
    pub glyphs: Vec<ShapedGlyph>,
}

/// Parse one `font.features` entry.
///
/// Not harfrust's `Feature::from_str`: it reads `HarfBuzz` syntax
/// (`kern=0`, `+kern`) and rejects the documented `ss03 on` and
/// `ss03 2` forms. Quotes are tolerated for CSS compatibility.
fn parse_feature(entry: &str) -> Option<Feature> {
    let entry = entry.trim();
    let (entry, negated) = entry
        .strip_prefix('-')
        .map_or((entry, false), |rest| (rest, true));
    let mut tokens = entry.split_whitespace();
    let tag = tokens.next()?.trim_matches(|c| c == '"' || c == '\'');
    // OpenType tags are 1-4 printable-ASCII bytes.
    if tag.is_empty() || tag.len() > 4 || !tag.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let value: u16 = match tokens.next() {
        None => u16::from(!negated),
        Some(_) if negated => return None,
        Some("on") => 1,
        Some("off") => 0,
        Some(v) => v.parse().ok()?,
    };
    if tokens.next().is_some() {
        return None;
    }
    let tag = Tag::new_checked(tag.as_bytes()).ok()?;
    Some(Feature::new(tag, u32::from(value), ..))
}

/// Enough for the four styled primaries and the faces one frame's
/// clusters fall back to.
const PLAN_SLOTS: usize = 16;

struct PlanSlot {
    font: u64,
    features: Vec<Feature>,
    used: u64,
    plan: ShapePlan,
}

#[derive(Default)]
struct PlanCache {
    slots: Vec<PlanSlot>,
    clock: u64,
}

impl PlanCache {
    fn get(&mut self, font: &Font, features: &[Feature]) -> &ShapePlan {
        self.clock += 1;
        let found = self
            .slots
            .iter()
            .position(|slot| slot.font == font.id && slot.features == features);
        let index = found.unwrap_or_else(|| {
            let slot = PlanSlot {
                font: font.id,
                features: features.to_vec(),
                used: 0,
                plan: ShapePlan::new(
                    &font.face,
                    Direction::LeftToRight,
                    Some(Script::LATIN),
                    None,
                    features,
                ),
            };
            if self.slots.len() < PLAN_SLOTS {
                self.slots.push(slot);
                self.slots.len() - 1
            } else {
                let oldest = (0..self.slots.len())
                    .min_by_key(|&i| self.slots[i].used)
                    .unwrap_or(0);
                self.slots[oldest] = slot;
                oldest
            }
        });
        self.slots[index].used = self.clock;
        &self.slots[index].plan
    }
}

/// Reusable shaper, held across frames: it keeps the glyph buffer and the
/// shape plans of the last few (face, features) pairs, which a fresh one
/// would rebuild.
pub struct Shaper {
    buffer: Buffer,
    plans: PlanCache,
}

impl Default for Shaper {
    fn default() -> Self {
        Self::new()
    }
}

impl Shaper {
    #[must_use]
    pub fn new() -> Self {
        let mut buffer = Buffer::new();
        buffer.set_flags(BufferFlags::REMOVE_DEFAULT_IGNORABLES);
        Self {
            buffer,
            plans: PlanCache::default(),
        }
    }

    /// One `ShapedGlyph` per output glyph (fewer than cells when a
    /// ligature fires). Malformed `features` entries are dropped
    /// silently, as CSS does. Direction is fixed LTR: a cell row is LTR
    /// by construction and bidi reordering lives in the bidi marker
    /// render path. The script is fixed Latin.
    pub fn shape_run(
        &mut self,
        font: &Font,
        px: f32,
        features: &[String],
        text: &str,
    ) -> Vec<ShapedGlyph> {
        if text.is_empty() {
            return Vec::new();
        }
        let features: Vec<Feature> = features.iter().filter_map(|s| parse_feature(s)).collect();
        let buffer = &mut self.buffer;
        buffer.push_str(text);
        buffer.set_direction(Direction::LeftToRight);
        buffer.set_script(Some(Script::LATIN));
        let shaped = harfrust::shape(
            &ShaperFont::new(&font.face),
            buffer,
            ShapeOptions::new()
                .plan(Some(self.plans.get(font, &features)))
                .features(&features),
        );
        debug_assert!(shaped.is_ok(), "the plan matches the buffer: {shaped:?}");
        let out = if shaped.is_ok() && buffer.allocation_successful() {
            glyphs_of(
                buffer,
                px / f32::from(font.face.units_per_em().max(1)),
                text,
            )
        } else {
            Vec::new()
        };
        buffer.clear();
        out
    }

    /// Shape one grapheme cluster against a single face: the base's, except
    /// that a VS15 right after the base asks for its text face, and a ZWJ
    /// sequence or a VS16 anchors on the color emoji face so it can ligate
    /// into one color glyph.
    pub fn shape_cluster(
        &mut self,
        stack: &FontStack,
        px: f32,
        style: FontStyle,
        text: &str,
    ) -> ClusterShaping {
        let Some(font_id) = cluster_face(stack, style, text) else {
            return ClusterShaping {
                font_id: 0,
                glyphs: Vec::new(),
            };
        };
        let font = stack.font_at(font_id, style).clone();
        let features = stack.features_for(font_id, style);
        let glyphs = self.shape_run(&font, px, features, text);
        ClusterShaping { font_id, glyphs }
    }
}

fn cluster_face(stack: &FontStack, style: FontStyle, text: &str) -> Option<usize> {
    let base = text.chars().next()?;
    Some(if text.chars().nth(1) == Some('\u{FE0E}') {
        stack.first_cover_styled_index(base, style)
    } else if let Some(anchor) = text.chars().find(|&c| is_emoji_face_anchor(c)) {
        stack.resolve_styled_index(anchor, style)
    } else if text.contains('\u{FE0F}') {
        stack
            .resolve_color_index(base, style)
            .unwrap_or_else(|| stack.resolve_styled_index(base, style))
    } else {
        stack.resolve_styled_index(base, style)
    })
}

/// Clusters stay in logical order at harfrust's default cluster level, so
/// a glyph's source span ends where the next cluster starts.
fn glyphs_of(buffer: &Buffer, scale: f32, text: &str) -> Vec<ShapedGlyph> {
    let mut out: Vec<ShapedGlyph> = buffer
        .glyph_infos()
        .iter()
        .zip(buffer.glyph_positions())
        .map(|(info, pos)| ShapedGlyph {
            glyph_id: info.glyph_id,
            source_byte_start: info.cluster,
            source_byte_len: 0,
            advance_px: pos.x_advance as f32 * scale,
            x_offset_px: pos.x_offset as f32 * scale,
            y_offset_px: pos.y_offset as f32 * scale,
        })
        .collect();
    let mut end = u32::try_from(text.len()).unwrap_or(u32::MAX);
    let mut next_start = None;
    for glyph in out.iter_mut().rev() {
        if let Some(next) = next_start
            && next != glyph.source_byte_start
        {
            end = next;
        }
        glyph.source_byte_len = end.saturating_sub(glyph.source_byte_start);
        next_start = Some(glyph.source_byte_start);
    }
    out
}

#[cfg(test)]
#[cfg_attr(
    windows,
    allow(
        dead_code,
        reason = "the pinned-font tests are compiled out on Windows"
    )
)]
mod tests;
